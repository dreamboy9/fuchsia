// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DRIVERS_MSD_IMG_RGX_MTK_MT8167S_GPU_H_
#define SRC_GRAPHICS_DRIVERS_MSD_IMG_RGX_MTK_MT8167S_GPU_H_

#include <fuchsia/gpu/magma/llcpp/fidl.h>
#include <fuchsia/hardware/clock/cpp/banjo.h>
#include <fuchsia/hardware/platform/device/c/banjo.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/device.h>
#include <lib/ddk/driver.h>
#include <lib/ddk/hw/reg.h>
#include <lib/ddk/platform-defs.h>
#include <lib/device-protocol/platform-device.h>
#include <lib/fidl-utils/bind.h>
#include <lib/mmio/mmio.h>
#include <zircon/errors.h>

#include <memory>
#include <mutex>

#include <ddktl/device.h>
#include <ddktl/protocol/empty-protocol.h>

#include "img-sys-device.h"
#include "magma_util/macros.h"
#include "sys_driver/magma_driver.h"

#define GPU_ERROR(fmt, ...) zxlogf(ERROR, "[%s %d]" fmt, __func__, __LINE__, ##__VA_ARGS__)

namespace {
// Indices into clocks provided by the board file.
constexpr uint32_t kClkSlowMfgIndex = 0;
constexpr uint32_t kClkAxiMfgIndex = 1;
constexpr uint32_t kClkMfgMmIndex = 2;
constexpr uint32_t kClockCount = 3;

// Indices into mmio buffers provided by the board file.
constexpr uint32_t kMfgMmioIndex = 0;
constexpr uint32_t kMfgTopMmioIndex = 1;
constexpr uint32_t kScpsysMmioIndex = 2;
constexpr uint32_t kXoMmioIndex = 3;

// This register enables accessing registers in a power domain.
constexpr uint32_t kInfraTopAxiSi1Ctl = 0x1204;
// When protection is enabled the unit is disconnected from the AXI bus so
// it can't cause issues when powered down.
constexpr uint32_t kInfraTopAxiProtectEn = 0x1220;
constexpr uint32_t kInfraTopAxiProtectSta1 = 0x1228;

constexpr uint32_t kInfraTopAxiBusProtMaskMfg2d = (1 << 2) | (1 << 5);
constexpr uint32_t kInfraTopAxiSi1WayEnMfg2d = (1 << 7);
constexpr uint32_t kPwrStatus = 0x60c;
constexpr uint32_t kPwrStatus2nd = 0x610;

}  // namespace

class Mt8167sGpu;

using DeviceType = ddk::Device<Mt8167sGpu, ddk::Messageable<fuchsia_gpu_magma::Device>::Mixin>;

class Mt8167sGpu : public DeviceType,
                   public ddk::EmptyProtocol<ZX_PROTOCOL_GPU>,
                   public ImgSysDevice {
 public:
  Mt8167sGpu(zx_device_t* parent) : DeviceType(parent) {}

  virtual ~Mt8167sGpu();

  zx_status_t Bind();
  void DdkRelease();

  void Query2(Query2RequestView request, Query2Completer::Sync& _completer) override;
  void QueryReturnsBuffer(QueryReturnsBufferRequestView request,
                          QueryReturnsBufferCompleter::Sync& _completer) override;
  void Connect(ConnectRequestView request, ConnectCompleter::Sync& _completer) override;
  void DumpState(DumpStateRequestView request, DumpStateCompleter::Sync& _completer) override;
  void TestRestart(TestRestartRequestView request, TestRestartCompleter::Sync& _completer) override;
  void GetUnitTestStatus(GetUnitTestStatusRequestView request,
                         GetUnitTestStatusCompleter::Sync& _completer) override;
  void GetIcdList(GetIcdListRequestView request, GetIcdListCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  zx_status_t PowerUp() override;
  zx_status_t PowerDown() override;
  void* device() override { return parent(); }

 private:
  friend class Mt8167GpuTest;

  bool StartMagma() MAGMA_REQUIRES(magma_mutex_);
  void StopMagma() MAGMA_REQUIRES(magma_mutex_);

  // MFG is Mediatek's name for their graphics subsystem.
  zx_status_t PowerOnMfgAsync();
  zx_status_t PowerOnMfg2d();
  zx_status_t PowerOnMfg();
  zx_status_t PowerDownMfgAsync();
  zx_status_t PowerDownMfg2d();
  zx_status_t PowerDownMfg();

  void EnableMfgHwApm();

  ddk::ClockProtocolClient clks_[kClockCount];
  // MFG TOP MMIO - Controls mediatek's gpu-related power- and
  // clock-management hardware.
  std::optional<ddk::MmioBuffer> gpu_buffer_;
  // MFG MMIO (corresponds to the IMG GPU's registers)
  std::optional<ddk::MmioBuffer> real_gpu_buffer_;
  std::optional<ddk::MmioBuffer> power_gpu_buffer_;  // SCPSYS MMIO
  std::optional<ddk::MmioBuffer> clock_gpu_buffer_;  // XO MMIO

  bool logged_gpu_info_ = false;

  std::mutex magma_mutex_;
  std::unique_ptr<MagmaDriver> magma_driver_ MAGMA_GUARDED(magma_mutex_);
  std::shared_ptr<MagmaSystemDevice> magma_system_device_ MAGMA_GUARDED(magma_mutex_);
};

#endif  // SRC_GRAPHICS_DRIVERS_MSD_IMG_RGX_MTK_MT8167S_GPU_H_
