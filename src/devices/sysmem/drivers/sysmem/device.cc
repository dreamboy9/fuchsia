// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "device.h"

#include <fuchsia/hardware/platform/bus/c/banjo.h>
#include <fuchsia/hardware/platform/bus/cpp/banjo.h>
#include <fuchsia/sysmem/c/banjo.h>
#include <fuchsia/sysmem/c/fidl.h>
#include <fuchsia/sysmem/llcpp/fidl.h>
#include <fuchsia/sysmem2/llcpp/fidl.h>
#include <inttypes.h>
#include <lib/async/dispatcher.h>
#include <lib/ddk/device.h>
#include <lib/ddk/platform-defs.h>
#include <lib/fidl-async-2/simple_binding.h>
#include <lib/fidl-utils/bind.h>
#include <lib/sync/completion.h>
#include <lib/sysmem-version/sysmem-version.h>
#include <lib/zx/channel.h>
#include <lib/zx/event.h>
#include <zircon/assert.h>
#include <zircon/errors.h>

#include <memory>
#include <thread>

#include <fbl/string_printf.h>

#include "allocator.h"
#include "buffer_collection_token.h"
#include "contiguous_pooled_memory_allocator.h"
#include "driver.h"
#include "external_memory_allocator.h"
#include "macros.h"

using sysmem_driver::MemoryAllocator;

namespace sysmem_driver {
namespace {

// These defaults only take effect if there is no SYSMEM_METADATA_TYPE, and also
// neither of these kernel cmdline parameters set:
// driver.sysmem.contiguous_memory_size
// driver.sysmem.protected_memory_size
//
// Typically these defaults are overriden.
//
// By default there is no protected memory pool.
constexpr int64_t kDefaultProtectedMemorySize = 0;
// By default we pre-reserve 5% of physical memory for contiguous memory
// allocation via sysmem.
//
// This is enough to allow tests in sysmem_tests.cc to pass, and avoids relying
// on zx::vmo::create_contiguous() after early boot (by default), since it can
// fail if physical memory has gotten too fragmented.
constexpr int64_t kDefaultContiguousMemorySize = -5;

// fbl::round_up() doesn't work on signed types.
template <typename T>
T AlignUp(T value, T divisor) {
  return (value + divisor - 1) / divisor * divisor;
}

// Helper function to build owned HeapProperties table with coherency domain support.
fuchsia_sysmem2::wire::HeapProperties BuildHeapPropertiesWithCoherencyDomainSupport(
    fidl::AnyAllocator& allocator, bool cpu_supported, bool ram_supported,
    bool inaccessible_supported, bool need_clear) {
  using fuchsia_sysmem2::wire::CoherencyDomainSupport;
  using fuchsia_sysmem2::wire::HeapProperties;

  CoherencyDomainSupport coherency_domain_support(allocator);
  coherency_domain_support.set_cpu_supported(allocator, cpu_supported)
      .set_ram_supported(allocator, ram_supported)
      .set_inaccessible_supported(allocator, inaccessible_supported);

  HeapProperties heap_properties(allocator);
  heap_properties.set_coherency_domain_support(allocator, std::move(coherency_domain_support))
      .set_need_clear(allocator, need_clear);
  return heap_properties;
}

class SystemRamMemoryAllocator : public MemoryAllocator {
 public:
  SystemRamMemoryAllocator(Owner* parent_device)
      : MemoryAllocator(parent_device->table_set(),
                        BuildHeapPropertiesWithCoherencyDomainSupport(
                            parent_device->table_set().allocator(), true /*cpu*/, true /*ram*/,
                            true /*inaccessible*/,
                            // Zircon guarantees created VMO are filled with 0; sysmem doesn't
                            // need to clear it once again.
                            false /*need_clear*/)) {
    node_ = parent_device->heap_node()->CreateChild("SysmemRamMemoryAllocator");
    node_.CreateUint("id", id(), &properties_);
  }

  zx_status_t Allocate(uint64_t size, std::optional<std::string> name,
                       zx::vmo* parent_vmo) override {
    zx_status_t status = zx::vmo::create(size, 0, parent_vmo);
    if (status != ZX_OK) {
      return status;
    }
    constexpr const char vmo_name[] = "Sysmem-core";
    parent_vmo->set_property(ZX_PROP_NAME, vmo_name, sizeof(vmo_name));
    return status;
  }

  zx_status_t SetupChildVmo(const zx::vmo& parent_vmo, const zx::vmo& child_vmo,
                            fuchsia_sysmem2::wire::SingleBufferSettings buffer_settings) override {
    // nothing to do here
    return ZX_OK;
  }

  virtual void Delete(zx::vmo parent_vmo) override {
    // ~parent_vmo
  }
  // Since this allocator only allocates independent VMOs, it's fine to orphan those VMOs from the
  // allocator since the VMOs independently track what pages they're using.  So this allocator can
  // always claim is_empty() true.
  bool is_empty() override { return true; }

 private:
  inspect::Node node_;
  inspect::ValueList properties_;
};

class ContiguousSystemRamMemoryAllocator : public MemoryAllocator {
 public:
  explicit ContiguousSystemRamMemoryAllocator(Owner* parent_device)
      : MemoryAllocator(parent_device->table_set(),
                        BuildHeapPropertiesWithCoherencyDomainSupport(
                            parent_device->table_set().allocator(), true /*cpu*/, true /*ram*/,
                            true /*inaccessible*/,
                            // Zircon guarantees contagious VMO created are filled with 0;
                            // sysmem doesn't need to clear it once again.
                            false /*need_clear*/)),
        parent_device_(parent_device) {
    node_ = parent_device_->heap_node()->CreateChild("ContiguousSystemRamMemoryAllocator");
    node_.CreateUint("id", id(), &properties_);
  }

  zx_status_t Allocate(uint64_t size, std::optional<std::string> name,
                       zx::vmo* parent_vmo) override {
    zx::vmo result_parent_vmo;
    // This code is unlikely to work after running for a while and physical
    // memory is more fragmented than early during boot. The
    // ContiguousPooledMemoryAllocator handles that case by keeping
    // a separate pool of contiguous memory.
    zx_status_t status =
        zx::vmo::create_contiguous(parent_device_->bti(), size, 0, &result_parent_vmo);
    if (status != ZX_OK) {
      DRIVER_ERROR(
          "zx::vmo::create_contiguous() failed - size_bytes: %lu "
          "status: %d",
          size, status);
      zx_info_kmem_stats_t kmem_stats;
      status = zx_object_get_info(get_root_resource(), ZX_INFO_KMEM_STATS, &kmem_stats,
                                  sizeof(kmem_stats), nullptr, nullptr);
      if (status == ZX_OK) {
        DRIVER_ERROR(
            "kmem stats: total_bytes: 0x%lx free_bytes 0x%lx: wired_bytes: 0x%lx vmo_bytes: 0x%lx\n"
            "mmu_overhead_bytes: 0x%lx other_bytes: 0x%lx",
            kmem_stats.total_bytes, kmem_stats.free_bytes, kmem_stats.wired_bytes,
            kmem_stats.vmo_bytes, kmem_stats.mmu_overhead_bytes, kmem_stats.other_bytes);
      }
      // sanitize to ZX_ERR_NO_MEMORY regardless of why.
      status = ZX_ERR_NO_MEMORY;
      return status;
    }
    constexpr const char vmo_name[] = "Sysmem-contig-core";
    result_parent_vmo.set_property(ZX_PROP_NAME, vmo_name, sizeof(vmo_name));
    *parent_vmo = std::move(result_parent_vmo);
    return ZX_OK;
  }
  virtual zx_status_t SetupChildVmo(
      const zx::vmo& parent_vmo, const zx::vmo& child_vmo,
      fuchsia_sysmem2::wire::SingleBufferSettings buffer_settings) override {
    // nothing to do here
    return ZX_OK;
  }
  void Delete(zx::vmo parent_vmo) override {
    // ~vmo
  }
  // Since this allocator only allocates independent VMOs, it's fine to orphan those VMOs from the
  // allocator since the VMOs independently track what pages they're using.  So this allocator can
  // always claim is_empty() true.
  bool is_empty() override { return true; }

 private:
  Owner* const parent_device_;
  inspect::Node node_;
  inspect::ValueList properties_;
};

}  // namespace

Device::Device(zx_device_t* parent_device, Driver* parent_driver)
    : DdkDeviceType(parent_device),
      parent_driver_(parent_driver),
      loop_(&kAsyncLoopConfigNeverAttachToThread),
      in_proc_sysmem_protocol_{.ops = &sysmem_protocol_ops_, .ctx = this} {
  ZX_DEBUG_ASSERT(parent_);
  ZX_DEBUG_ASSERT(parent_driver_);
  zx_status_t status = loop_.StartThread("sysmem", &loop_thrd_);
  ZX_ASSERT(status == ZX_OK);
  // Up until DdkAdd, all access to member variables must happen on this thread.
  loop_checker_.emplace(fit::thread_checker());
}

// static
zx_status_t Device::OverrideSizeFromCommandLine(const char* name, int64_t* memory_size) {
  const char* pool_arg = getenv(name);
  if (!pool_arg || strlen(pool_arg) == 0)
    return ZX_OK;
  char* end = nullptr;
  int64_t override_size = strtoll(pool_arg, &end, 10);
  // Check that entire string was used and there isn't garbage at the end.
  if (*end != '\0') {
    DRIVER_ERROR("Ignoring flag %s with invalid size \"%s\"", name, pool_arg);
    return ZX_ERR_INVALID_ARGS;
  }
  DRIVER_INFO("Flag %s overriding size to %ld", name, override_size);
  if (override_size < -99) {
    DRIVER_ERROR("Flag %s specified too-large percentage: %" PRId64, name, -override_size);
    return ZX_ERR_INVALID_ARGS;
  }
  *memory_size = override_size;
  return ZX_OK;
}

zx_status_t Device::GetContiguousGuardParameters(uint64_t* guard_bytes_out,
                                                 bool* internal_guard_pages_out,
                                                 bool* crash_on_fail_out) {
  constexpr uint64_t kDefaultGuardBytes = ZX_PAGE_SIZE;
  *guard_bytes_out = kDefaultGuardBytes;
  *internal_guard_pages_out = false;
  *crash_on_fail_out = false;

  // If true, sysmem crashes on a guard page violation.
  if (getenv("driver.sysmem.contiguous_guard_pages_fatal")) {
    DRIVER_INFO("Setting contiguous_guard_pages_fatal");
    *crash_on_fail_out = true;
  }

  // If true, sysmem will create guard regions around every allocation.
  if (getenv("driver.sysmem.contiguous_guard_pages_internal")) {
    DRIVER_INFO("Setting contiguous_guard_pages_internal");
    *internal_guard_pages_out = true;
  }

  const char* kName = "driver.sysmem.contiguous_guard_page_count";
  const char* guard_count = getenv(kName);
  if (!guard_count || strlen(guard_count) == 0) {
    return ZX_OK;
  }
  char* end = nullptr;
  int64_t page_count = strtoll(guard_count, &end, 10);
  // Check that entire string was used and there isn't garbage at the end.
  if (*end != '\0') {
    DRIVER_ERROR("Ignoring flag %s with invalid value \"%s\"", kName, guard_count);
    return ZX_ERR_INVALID_ARGS;
  }
  DRIVER_INFO("Flag %s setting guard page count to %ld", kName, page_count);
  *guard_bytes_out = ZX_PAGE_SIZE * page_count;

  return ZX_OK;
}

void Device::DdkUnbind(ddk::UnbindTxn txn) {
  // Try to ensure there are no outstanding VMOS before shutting down the loop.
  async::PostTask(loop_.dispatcher(), [this]() mutable {
    std::lock_guard checker(*loop_checker_);
    waiting_for_unbind_ = true;
    CheckForUnbind();
  });

  // JoinThreads waits for the Quit() in CheckForUnbind to execute and cause the thread to exit. We
  // could instead try to asynchronously do these operations on another thread, but the display unit
  // tests don't have a way to wait for the unbind to be complete before tearing down the device.
  loop_.JoinThreads();
  loop_.Shutdown();
  // After this point the FIDL servers should have been shutdown and all DDK and other protocol
  // methods will error out because posting tasks to the dispatcher fails.
  txn.Reply();
  zxlogf(INFO, "Finished unbind.");
}

void Device::CheckForUnbind() {
  std::lock_guard checker(*loop_checker_);
  if (!waiting_for_unbind_)
    return;
  if (!logical_buffer_collections().empty()) {
    zxlogf(INFO, "Not unbinding because there are logical buffer collections count %ld",
           logical_buffer_collections().size());
    return;
  }
  if (!contiguous_system_ram_allocator_->is_empty()) {
    zxlogf(INFO, "Not unbinding because contiguous system ram allocator is not empty");
    return;
  }
  for (auto& [type, allocator] : allocators_) {
    if (!allocator->is_empty()) {
      zxlogf(INFO, "Not unbinding because allocator %lx is not empty", static_cast<uint64_t>(type));

      return;
    }
  }

  // This will cause the loop to exit and will allow DdkUnbind to continue.
  loop_.Quit();
}

TableSet& Device::table_set() { return table_set_; }

zx_status_t Device::Bind() {
  std::lock_guard checker(*loop_checker_);
  // Put everything under a node called "sysmem" because there's currently there's not a simple way
  // to distinguish (using a selector) which driver inspect information is coming from.
  sysmem_root_ = inspector_.GetRoot().CreateChild("sysmem");
  heaps_ = sysmem_root_.CreateChild("heaps");
  collections_node_ = sysmem_root_.CreateChild("collections");

  zx_status_t status = ddk::PDevProtocolClient::CreateFromDevice(parent_, &pdev_);
  if (status != ZX_OK) {
    DRIVER_ERROR("Failed device_get_protocol() ZX_PROTOCOL_PDEV - status: %d", status);
    return status;
  }

  int64_t protected_memory_size = kDefaultProtectedMemorySize;
  int64_t contiguous_memory_size = kDefaultContiguousMemorySize;

  sysmem_metadata_t metadata;

  size_t metadata_actual;
  status = DdkGetMetadata(SYSMEM_METADATA_TYPE, &metadata, sizeof(metadata), &metadata_actual);
  if (status == ZX_OK && metadata_actual == sizeof(metadata)) {
    pdev_device_info_vid_ = metadata.vid;
    pdev_device_info_pid_ = metadata.pid;
    protected_memory_size = metadata.protected_memory_size;
    contiguous_memory_size = metadata.contiguous_memory_size;
  }

  status =
      OverrideSizeFromCommandLine("driver.sysmem.protected_memory_size", &protected_memory_size);
  if (status != ZX_OK) {
    // OverrideSizeFromCommandLine() already printed an error.
    return status;
  }
  status =
      OverrideSizeFromCommandLine("driver.sysmem.contiguous_memory_size", &contiguous_memory_size);
  if (status != ZX_OK) {
    // OverrideSizeFromCommandLine() already printed an error.
    return status;
  }

  // Negative values are interpreted as a percentage of physical RAM.
  if (protected_memory_size < 0) {
    protected_memory_size = -protected_memory_size;
    ZX_DEBUG_ASSERT(protected_memory_size >= 1 && protected_memory_size <= 99);
    protected_memory_size = zx_system_get_physmem() * protected_memory_size / 100;
  }
  if (contiguous_memory_size < 0) {
    contiguous_memory_size = -contiguous_memory_size;
    ZX_DEBUG_ASSERT(contiguous_memory_size >= 1 && contiguous_memory_size <= 99);
    contiguous_memory_size = zx_system_get_physmem() * contiguous_memory_size / 100;
  }

  constexpr int64_t kMinProtectedAlignment = 64 * 1024;
  static_assert(kMinProtectedAlignment % ZX_PAGE_SIZE == 0);
  protected_memory_size = AlignUp(protected_memory_size, kMinProtectedAlignment);
  contiguous_memory_size = AlignUp(contiguous_memory_size, static_cast<int64_t>(ZX_PAGE_SIZE));

  allocators_[fuchsia_sysmem2::wire::HeapType::kSystemRam] =
      std::make_unique<SystemRamMemoryAllocator>(this);

  status = pdev_.GetBti(0, &bti_);
  if (status != ZX_OK) {
    DRIVER_ERROR("Failed pdev_get_bti() - status: %d", status);
    return status;
  }

  zx::bti bti_copy;
  status = bti_.duplicate(ZX_RIGHT_SAME_RIGHTS, &bti_copy);
  if (status != ZX_OK) {
    DRIVER_ERROR("BTI duplicate failed: %d", status);
    return status;
  }

  if (contiguous_memory_size) {
    constexpr bool kIsCpuAccessible = true;
    constexpr bool kIsReady = true;
    constexpr bool kCanBeTornDown = true;
    auto pooled_allocator = std::make_unique<ContiguousPooledMemoryAllocator>(
        this, "SysmemContiguousPool", &heaps_, fuchsia_sysmem_HeapType_SYSTEM_RAM,
        contiguous_memory_size, kIsCpuAccessible, kIsReady, kCanBeTornDown, loop_.dispatcher());
    if (pooled_allocator->Init() != ZX_OK) {
      DRIVER_ERROR("Contiguous system ram allocator initialization failed");
      return ZX_ERR_NO_MEMORY;
    }
    uint64_t guard_region_size;
    bool internal_guard_regions;
    bool crash_on_guard;
    if (GetContiguousGuardParameters(&guard_region_size, &internal_guard_regions,
                                     &crash_on_guard) == ZX_OK) {
      pooled_allocator->InitGuardRegion(guard_region_size, internal_guard_regions, crash_on_guard,
                                        loop_.dispatcher());
    }
    contiguous_system_ram_allocator_ = std::move(pooled_allocator);
  } else {
    contiguous_system_ram_allocator_ = std::make_unique<ContiguousSystemRamMemoryAllocator>(this);
  }

  // TODO: Separate protected memory allocator into separate driver or library
  if (pdev_device_info_vid_ == PDEV_VID_AMLOGIC && protected_memory_size > 0) {
    constexpr bool kIsCpuAccessible = false;
    constexpr bool kIsReady = false;
    // We have no way to tear down secure memory.
    constexpr bool kCanBeTornDown = false;
    auto amlogic_allocator = std::make_unique<ContiguousPooledMemoryAllocator>(
        this, "SysmemAmlogicProtectedPool", &heaps_, fuchsia_sysmem_HeapType_AMLOGIC_SECURE,
        protected_memory_size, kIsCpuAccessible, kIsReady, kCanBeTornDown, loop_.dispatcher());
    // Request 64kB alignment because the hardware can only modify protections along 64kB
    // boundaries.
    status = amlogic_allocator->Init(16);
    if (status != ZX_OK) {
      DRIVER_ERROR("Failed to init allocator for amlogic protected memory: %d", status);
      return status;
    }
    secure_allocators_[fuchsia_sysmem2::wire::HeapType::kAmlogicSecure] = amlogic_allocator.get();
    allocators_[fuchsia_sysmem2::wire::HeapType::kAmlogicSecure] = std::move(amlogic_allocator);
  }

  ddk::PBusProtocolClient pbus;
  status = ddk::PBusProtocolClient::CreateFromDevice(parent_, &pbus);
  if (status != ZX_OK) {
    zxlogf(INFO, "ZX_PROTOCL_PBUS not available %d", status);
  }

  sync_completion_t completion;
  async::PostTask(loop_.dispatcher(), [this, &completion] {
    // After this point, all operations must happen on the loop thread.
    loop_checker_.emplace(fit::thread_checker());
    sync_completion_signal(&completion);
  });
  sync_completion_wait_deadline(&completion, ZX_TIME_INFINITE);

  status = DdkAdd(ddk::DeviceAddArgs("sysmem")
                      .set_flags(DEVICE_ADD_ALLOW_MULTI_COMPOSITE)
                      .set_inspect_vmo(inspector_.DuplicateVmo()));
  if (status != ZX_OK) {
    DRIVER_ERROR("Failed to bind device");
    return status;
  }

  if (pbus.is_valid()) {
    // Register the sysmem protocol with the platform bus.
    //
    // This is essentially the in-proc version of
    // fuchsia.sysmem.DriverConnector.
    //
    // We should only pbus_register_protocol() if device_add() succeeded, but if
    // pbus_register_protocol() fails, we should remove the device without it
    // ever being visible.
    // TODO(fxbug.dev/33536) Remove this after all clients have switched to using composite
    // protocol.
    status = pbus.RegisterProtocol(ZX_PROTOCOL_SYSMEM,
                                   reinterpret_cast<uint8_t*>(&in_proc_sysmem_protocol_),
                                   sizeof(in_proc_sysmem_protocol_));
    if (status != ZX_OK) {
      DdkAsyncRemove();
      return status;
    }
  }

  return ZX_OK;
}

void Device::Connect(ConnectRequestView request, ConnectCompleter::Sync& completer) {
  async::PostTask(loop_.dispatcher(),
                  [this, allocator_request = std::move(request->allocator_request)]() mutable {
                    // The Allocator is channel-owned / self-owned.
                    Allocator::CreateChannelOwned(allocator_request.TakeChannel(), this);
                  });
}

zx_status_t Device::SysmemConnect(zx::channel allocator_request) {
  // The Allocator is channel-owned / self-owned.
  return async::PostTask(loop_.dispatcher(),
                         [this, allocator_request = std::move(allocator_request)]() mutable {
                           table_set_.MitigateChurn();
                           // The Allocator is channel-owned / self-owned.
                           Allocator::CreateChannelOwned(std::move(allocator_request), this);
                         });
}

zx_status_t Device::SysmemRegisterHeap(uint64_t heap_param, zx::channel heap_connection) {
  // External heaps should not have bit 63 set but bit 60 must be set.
  if ((heap_param & 0x8000000000000000) || !(heap_param & 0x1000000000000000)) {
    DRIVER_ERROR("Invalid external heap");
    return ZX_ERR_INVALID_ARGS;
  }
  auto heap = static_cast<fuchsia_sysmem2::wire::HeapType>(heap_param);

  return async::PostTask(loop_.dispatcher(), [this, heap,
                                              heap_connection =
                                                  std::move(heap_connection)]() mutable {
    std::lock_guard checker(*loop_checker_);
    table_set_.MitigateChurn();
    // Clean up heap allocator after peer closed channel.
    auto wait_for_close = std::make_unique<async::Wait>(
        heap_connection.get(), ZX_CHANNEL_PEER_CLOSED, 0,
        async::Wait::Handler([this, heap](async_dispatcher_t* dispatcher, async::Wait* wait,
                                          zx_status_t status, const zx_packet_signal_t* signal) {
          std::lock_guard checker(*loop_checker_);
          allocators_.erase(heap);
        }));
    // It is safe to call Begin() here before adding entry to the map as
    // handler will run on current thread.
    zx_status_t status = wait_for_close->Begin(dispatcher());
    if (status != ZX_OK) {
      DRIVER_ERROR("Device::RegisterHeap() failed wait_for_close->Begin()");
      return;
    }

    class EventHandler : public fidl::WireAsyncEventHandler<fuchsia_sysmem2::Heap> {
     public:
      EventHandler(Device* device, std::unique_ptr<fidl::Client<fuchsia_sysmem2::Heap>> heap_client,
                   fuchsia_sysmem2::wire::HeapType heap,
                   std::unique_ptr<async::Wait> wait_for_close)
          : device_(device),
            heap_client_(std::move(heap_client)),
            heap_(heap),
            wait_for_close_(std::move(wait_for_close)) {}

      void OnRegister(fidl::WireResponse<fuchsia_sysmem2::Heap::OnRegister>* event) override {
        std::lock_guard checker(*device_->loop_checker_);
        // A heap should not be registered twice.
        ZX_DEBUG_ASSERT(heap_client_);
        // This replaces any previously registered allocator for heap (also cancels the old
        // wait). This behavior is preferred as it avoids a potential race-condition during
        // heap restart.
        device_->allocators_[heap_] = std::make_unique<ExternalMemoryAllocator>(
            device_, std::move(*heap_client_), std::move(wait_for_close_),
            sysmem::V2CloneHeapProperties(device_->table_set_.allocator(), event->properties));
      }

      void Unbound(fidl::UnbindInfo info) override {
        std::lock_guard checker(*device_->loop_checker_);
        if (info.reason() != fidl::Reason::kPeerClosed && info.reason() != fidl::Reason::kClose) {
          DRIVER_ERROR("Heap failed: %s\n", info.FormatDescription().c_str());
          device_->allocators_.erase(heap_);
        }
      }

     private:
      Device* const device_;
      std::unique_ptr<fidl::Client<fuchsia_sysmem2::Heap>> heap_client_;
      const fuchsia_sysmem2::wire::HeapType heap_;
      std::unique_ptr<async::Wait> wait_for_close_;
    };

    auto heap_client = std::make_unique<fidl::Client<fuchsia_sysmem2::Heap>>();
    auto heap_client_ptr = heap_client.get();
    heap_client_ptr->Bind(std::move(heap_connection), loop_.dispatcher(),
                          std::make_shared<EventHandler>(this, std::move(heap_client), heap,
                                                         std::move(wait_for_close)));
  });
}

zx_status_t Device::SysmemRegisterSecureMem(zx::channel secure_mem_connection) {
  LOG(DEBUG, "sysmem RegisterSecureMem begin");

  current_close_is_abort_ = std::make_shared<std::atomic_bool>(true);

  return async::PostTask(
      loop_.dispatcher(), [this, secure_mem_connection = std::move(secure_mem_connection),
                           close_is_abort = current_close_is_abort_]() mutable {
        std::lock_guard checker(*loop_checker_);
        table_set_.MitigateChurn();
        // This code must run asynchronously for two reasons:
        // 1) It does synchronous IPCs to the secure mem device, so SysmemRegisterSecureMem must
        // have return so the call from the secure mem device is unblocked.
        // 2) It modifies member variables like |secure_mem_| and |heaps_| that should only be
        // touched on |loop_|'s thread.
        auto wait_for_close = std::make_unique<async::Wait>(
            secure_mem_connection.get(), ZX_CHANNEL_PEER_CLOSED, 0,
            async::Wait::Handler([this, close_is_abort](async_dispatcher_t* dispatcher,
                                                        async::Wait* wait, zx_status_t status,
                                                        const zx_packet_signal_t* signal) {
              std::lock_guard checker(*loop_checker_);
              if (*close_is_abort && secure_mem_) {
                // The server end of this channel (the aml-securemem driver) is the driver that
                // listens for suspend(mexec) so that soft reboot can succeed.  If that driver has
                // failed, intentionally force a hard reboot here to get back to a known-good state.
                //
                // TODO(dustingreen): If there's any more direct way to intentionally trigger a hard
                // reboot, that would probably be better here.
                ZX_PANIC(
                    "secure_mem_ connection unexpectedly lost; secure mem in unknown state; hard "
                    "reboot");
              }
            }));

        // It is safe to call Begin() here before setting up secure_mem_ because handler will either
        // run on current thread (loop_thrd_), or be run after the current task finishes while the
        // loop is shutting down.
        zx_status_t status = wait_for_close->Begin(dispatcher());
        if (status != ZX_OK) {
          DRIVER_ERROR("Device::RegisterSecureMem() failed wait_for_close->Begin()");
          return;
        }

        secure_mem_ = std::make_unique<SecureMemConnection>(std::move(secure_mem_connection),
                                                            std::move(wait_for_close));

        // Else we already ZX_PANIC()ed in wait_for_close.
        ZX_DEBUG_ASSERT(secure_mem_);

        // At this point secure_allocators_ has only the secure heaps that are configured via sysmem
        // (not those configured via the TEE), and the memory for these is not yet protected.  Tell
        // the TEE about these.
        fuchsia_sysmem::wire::PhysicalSecureHeaps sysmem_configured_heaps;
        for (const auto& [heap_type, allocator] : secure_allocators_) {
          uint64_t base;
          uint64_t size;
          status = allocator->GetPhysicalMemoryInfo(&base, &size);
          // Should be impossible for this to fail for now.
          ZX_ASSERT(status == ZX_OK);
          LOG(DEBUG,
              "allocator.GetPhysicalMemoryInfo() heap_type: %08lx base: %016" PRIx64
              " size: %016" PRIx64,
              static_cast<uint64_t>(heap_type), base, size);

          fuchsia_sysmem::wire::PhysicalSecureHeap& heap =
              sysmem_configured_heaps.heaps[sysmem_configured_heaps.heaps_count];
          heap.heap = static_cast<fuchsia_sysmem::wire::HeapType>(heap_type);
          heap.physical_address = base;
          heap.size_bytes = size;
          ++sysmem_configured_heaps.heaps_count;
        }
        auto set_result =
            fidl::WireCall<fuchsia_sysmem::SecureMem>(zx::unowned_channel(secure_mem_->channel()))
                .SetPhysicalSecureHeaps(std::move(sysmem_configured_heaps));
        // For now the FIDL IPC failing is fatal.  Among the reasons is without that
        // call succeeding, we haven't told the HW to secure/protect the physical
        // range. However we still allow it to fail if the secure mem device
        // unregistered itself.
        // For now it could return an error on sherlock if the bootloader is old, so
        // in that case just don't mark the allocators as ready.
        if (!set_result.ok()) {
          ZX_ASSERT(!*close_is_abort);
          return;
        }
        if (set_result->result.is_err()) {
          LOG(WARNING, "Unable to set physical secure heaps. Status: %d", set_result->result.err());
          return;
        }

        for (const auto& [heap_type, allocator] : secure_allocators_) {
          // The TEE has now told the HW about this heap's physical range being secure/protected.
          allocator->set_ready();
        }

        // Now we get the secure heaps that are configured via the TEE.
        auto get_result =
            fidl::WireCall<fuchsia_sysmem::SecureMem>(zx::unowned_channel(secure_mem_->channel()))
                .GetPhysicalSecureHeaps();
        if (!get_result.ok()) {
          // For now this is fatal, since this case is very unexpected, and in this case rebooting
          // is the most plausible way to get back to a working state anyway.
          ZX_ASSERT(!*close_is_abort);
          return;
        }
        ZX_ASSERT(get_result->result.is_response());
        const fuchsia_sysmem::wire::PhysicalSecureHeaps& tee_configured_heaps =
            get_result->result.response().heaps;

        for (uint32_t heap_index = 0; heap_index < tee_configured_heaps.heaps_count; ++heap_index) {
          const fuchsia_sysmem::wire::PhysicalSecureHeap& heap =
              tee_configured_heaps.heaps[heap_index];
          constexpr bool kIsCpuAccessible = false;
          constexpr bool kIsReady = true;
          constexpr bool kCanBeTornDown = true;
          auto secure_allocator = std::make_unique<ContiguousPooledMemoryAllocator>(
              this, "tee_secure", &heaps_, static_cast<uint64_t>(heap.heap), heap.size_bytes,
              kIsCpuAccessible, kIsReady, kCanBeTornDown, loop_.dispatcher());
          status = secure_allocator->InitPhysical(heap.physical_address);
          // A failing status is fatal for now.
          ZX_ASSERT(status == ZX_OK);
          LOG(DEBUG,
              "created secure allocator: heap_type: %08lx base: %016" PRIx64 " size: %016" PRIx64,
              static_cast<uint64_t>(heap.heap), heap.physical_address, heap.size_bytes);
          auto heap_type = static_cast<fuchsia_sysmem2::wire::HeapType>(heap.heap);
          ZX_ASSERT(secure_allocators_.find(heap_type) == secure_allocators_.end());
          secure_allocators_[heap_type] = secure_allocator.get();
          ZX_ASSERT(allocators_.find(heap_type) == allocators_.end());
          allocators_[heap_type] = std::move(secure_allocator);
        }

        LOG(DEBUG, "sysmem RegisterSecureMem() done (async)");
      });
}

// This call allows us to tell the difference between expected vs. unexpected close of the tee_
// channel.
zx_status_t Device::SysmemUnregisterSecureMem() {
  // By this point, the aml-securemem driver's suspend(mexec) has already prepared for mexec.
  //
  // In this path, the server end of the channel hasn't closed yet, but will be closed shortly after
  // return from UnregisterSecureMem().
  //
  // We set a flag here so that a PEER_CLOSED of the channel won't cause the wait handler to crash.
  *current_close_is_abort_ = false;
  current_close_is_abort_.reset();
  return async::PostTask(loop_.dispatcher(), [this]() {
    std::lock_guard checker(*loop_checker_);
    LOG(DEBUG, "begin UnregisterSecureMem()");
    table_set_.MitigateChurn();
    secure_mem_.reset();
    LOG(DEBUG, "end UnregisterSecureMem()");
  });
}

const zx::bti& Device::bti() { return bti_; }

// Only use this in cases where we really can't use zx::vmo::create_contiguous() because we must
// specify a specific physical range.
zx_status_t Device::CreatePhysicalVmo(uint64_t base, uint64_t size, zx::vmo* vmo_out) {
  zx::vmo result_vmo;
  // Please do not use get_root_resource() in new code. See fxbug.dev/31358.
  zx::unowned_resource root_resource(get_root_resource());
  zx_status_t status = zx::vmo::create_physical(*root_resource, base, size, &result_vmo);
  if (status != ZX_OK) {
    return status;
  }
  *vmo_out = std::move(result_vmo);
  return ZX_OK;
}

uint32_t Device::pdev_device_info_vid() {
  ZX_DEBUG_ASSERT(pdev_device_info_vid_ != std::numeric_limits<uint32_t>::max());
  return pdev_device_info_vid_;
}

uint32_t Device::pdev_device_info_pid() {
  ZX_DEBUG_ASSERT(pdev_device_info_pid_ != std::numeric_limits<uint32_t>::max());
  return pdev_device_info_pid_;
}

void Device::TrackToken(BufferCollectionToken* token) {
  std::lock_guard checker(*loop_checker_);
  zx_koid_t server_koid = token->server_koid();
  ZX_DEBUG_ASSERT(server_koid != ZX_KOID_INVALID);
  ZX_DEBUG_ASSERT(tokens_by_koid_.find(server_koid) == tokens_by_koid_.end());
  tokens_by_koid_.insert({server_koid, token});
}

void Device::UntrackToken(BufferCollectionToken* token) {
  std::lock_guard checker(*loop_checker_);
  zx_koid_t server_koid = token->server_koid();
  if (server_koid == ZX_KOID_INVALID) {
    // The caller is allowed to un-track a token that never saw
    // SetServerKoid().
    return;
  }
  // This is intentionally idempotent, to allow un-tracking from
  // BufferCollectionToken::CloseChannel() as well as from
  // ~BufferCollectionToken().
  tokens_by_koid_.erase(server_koid);
}

bool Device::TryRemoveKoidFromUnfoundTokenList(zx_koid_t token_server_koid) {
  std::lock_guard checker(*loop_checker_);
  // unfound_token_koids_ is limited to kMaxUnfoundTokenCount (and likely empty), so a loop over it
  // should be efficient enough.
  for (auto it = unfound_token_koids_.begin(); it != unfound_token_koids_.end(); ++it) {
    if (*it == token_server_koid) {
      unfound_token_koids_.erase(it);
      return true;
    }
  }
  return false;
}

BufferCollectionToken* Device::FindTokenByServerChannelKoid(zx_koid_t token_server_koid) {
  std::lock_guard checker(*loop_checker_);
  auto iter = tokens_by_koid_.find(token_server_koid);
  if (iter == tokens_by_koid_.end()) {
    unfound_token_koids_.push_back(token_server_koid);
    constexpr uint32_t kMaxUnfoundTokenCount = 8;
    while (unfound_token_koids_.size() > kMaxUnfoundTokenCount) {
      unfound_token_koids_.pop_front();
    }
    return nullptr;
  }
  return iter->second;
}

MemoryAllocator* Device::GetAllocator(const fuchsia_sysmem2::wire::BufferMemorySettings& settings) {
  std::lock_guard checker(*loop_checker_);
  if (settings.heap() == fuchsia_sysmem2::wire::HeapType::kSystemRam &&
      settings.is_physically_contiguous()) {
    return contiguous_system_ram_allocator_.get();
  }

  auto iter = allocators_.find(settings.heap());
  if (iter == allocators_.end()) {
    return nullptr;
  }
  return iter->second.get();
}

const fuchsia_sysmem2::wire::HeapProperties& Device::GetHeapProperties(
    fuchsia_sysmem2::wire::HeapType heap) const {
  std::lock_guard checker(*loop_checker_);
  ZX_DEBUG_ASSERT(allocators_.find(heap) != allocators_.end());
  return allocators_.at(heap)->heap_properties();
}

Device::SecureMemConnection::SecureMemConnection(zx::channel connection,
                                                 std::unique_ptr<async::Wait> wait_for_close)
    : connection_(std::move(connection)), wait_for_close_(std::move(wait_for_close)) {
  // nothing else to do here
}

zx_handle_t Device::SecureMemConnection::channel() {
  ZX_DEBUG_ASSERT(connection_);
  return connection_.get();
}

}  // namespace sysmem_driver
