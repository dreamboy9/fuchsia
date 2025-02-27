// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_BOOT_ZBI_H_
#define ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_BOOT_ZBI_H_

#include <lib/arch/zbi-boot.h>
#include <lib/fitx/result.h>
#include <lib/zbitl/image.h>
#include <lib/zbitl/view.h>

#include <cstdint>

#include <ktl/optional.h>
#include <ktl/span.h>
#include <phys/allocation.h>

// BootZbi represents a bootable ZBI and manages the memory allocation and ZBI
// protocol details for getting its kernel image and data ZBI in place and
// handing off control.
//
// BootZbi is a move-only object that can only be created by BootZbi::Create
// from a valid input ZBI.  After construction yields information about the
// kernel's load requirements before doing anything.  Then Load() does any
// necessary allocations via Allocation::New and/or reuses the input ZBI's
// storage() space.  DataZbi() can then be amended before calling Boot().

class BootZbi {
 public:
  using Bytes = ktl::span<std::byte>;
  using InputZbi = zbitl::View<zbitl::ByteView>;
  using Zbi = zbitl::Image<Bytes>;
  using Error = InputZbi::CopyError<Bytes>;

  struct Size {
    size_t size, alignment;
  };

  // The boot_alloc code uses arbitrary pages after the official bss space.
  // So make sure to allocate some extra slop for the kernel.
  //
  // TODO(mcgrathr): Remove this when ZBI kernels in use actually conform to
  // the protocol and don't clobber extra memory.
  static constexpr uint64_t kKernelBootAllocReserve = 1024 * 1024 * 4;

  // Default-constructible and move-only.
  BootZbi() = default;
  BootZbi(BootZbi&&) = default;
  BootZbi& operator=(BootZbi&&) = default;

  // Suggest allocation parameters for a whole bootable ZBI image whose
  // incoming size is known but whose contents haven't been seen yet.  A
  // conforming allocation will be optimal for reuse by Load().
  static Size SuggestedAllocation(uint32_t zbi_size_bytes);

  // This initializes a new BootZbi object representing the input ZBI.  This is
  // to be called on a default-constructed object, which should not otherwise
  // be used before calling Init() or being moved-into from a BootZbi object on
  // which Init() has been called.  The initialized object describes the kernel
  // image in place before loading.
  fitx::result<Error> Init(InputZbi zbi);

  // Load the kernel and data ZBIs from the input ZBI.  The data ZBI will have
  // at least extra_data_capacity bytes of space available to Append() items to
  // it.  The input ZBI's memory may be reused for the kernel or data images,
  // so it should not be referenced afterwards.
  fitx::result<Error> Load(uint32_t extra_data_capacity = 0,
                           ktl::optional<uintptr_t> kernel_load_address = {});

  // Boot into the kernel loaded by Load(), which must have been called first.
  // This cannot fail and never returns.
  [[noreturn]] void Boot();

  // The Kernel* methods can be used at any time, even before Load().

  const zircon_kernel_t* KernelImage() const { return kernel_; }

  const zbi_kernel_t* KernelHeader() const { return &KernelImage()->data_kernel; }

  uint64_t KernelLoadAddress() const { return reinterpret_cast<uintptr_t>(KernelImage()); }

  uint32_t KernelLoadSize() const {
    return static_cast<uint32_t>(offsetof(zircon_kernel_t, data_kernel)) +
           KernelImage()->hdr_kernel.length;
  }

  uint64_t KernelMemorySize() const {
    return KernelLoadSize() + KernelHeader()->reserve_memory_size + kKernelBootAllocReserve;
  }

  uint64_t KernelEntryAddress() const { return KernelLoadAddress() + KernelHeader()->entry; }

  // If this returns true, then instead of using Load() it works to just assign
  // to DataZbi().storage() with a different data location and use the kernel
  // image already in place after construction by Init().
  bool KernelCanLoadInPlace() const;

  // The Data* methods can be used after a successful Load() or if a valid data
  // ZBI has been installed directly into DataZbi().  The data ZBI can be
  // modified in place up to its capacity.  Assigning to DataZbi() or its
  // storage() after Load() is allowed if the new address is properly aligned.

  Zbi& DataZbi() { return data_; }

  uint64_t DataLoadAddress() const { return reinterpret_cast<uint64_t>(data_.storage().data()); }

  uint64_t DataLoadSize() { return static_cast<uint64_t>(data_.size_bytes()); }

 private:
  // These are set on construction by Init().
  InputZbi zbi_;
  InputZbi::iterator kernel_item_;

  // These are set by Load().
  Allocation kernel_buffer_, data_buffer_;
  Zbi data_;

  // This points to the kernel load image at its aligned start, i.e. the start
  // of the container header before the kernel item.  In some cases there isn't
  // actually a container header there (at kernel_->hdr_file), so we only refer
  // only to the kernel item header (at kernel_->hdr_kernel) and payload.  At
  // construction by Init() this points to just before kernel_item_.  After
  // Load() it may instead point into kernel_buffer_.data(), but is guaranteed
  // to be aligned to arch::kZbiBootKernelAlignment and to have enough memory
  // after the load image for the requested reserves (either allocated in
  // kernel_buffer_ or reusing zbi_.storage() when no longer in use for data).
  const zircon_kernel_t* kernel_ = nullptr;
};

#endif  // ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_BOOT_ZBI_H_
