// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_CLOCK_DRIVERS_MTK_CLK_MTK_CLK_H_
#define SRC_DEVICES_CLOCK_DRIVERS_MTK_CLK_MTK_CLK_H_

#include <fuchsia/hardware/clock/llcpp/fidl.h>
#include <fuchsia/hardware/clockimpl/cpp/banjo.h>
#include <lib/mmio/mmio.h>

#include <ddktl/device.h>

namespace clk {

class MtkClk;
using DeviceType = ddk::Device<MtkClk, ddk::Messageable<fuchsia_hardware_clock::Device>::Mixin>;

class MtkClk : public DeviceType, public ddk::ClockImplProtocol<MtkClk, ddk::base_protocol> {
 public:
  static zx_status_t Create(zx_device_t* parent);

  void DdkRelease() { delete this; }

  zx_status_t Bind();

  zx_status_t ClockImplEnable(uint32_t index);
  zx_status_t ClockImplDisable(uint32_t index);
  zx_status_t ClockImplIsEnabled(uint32_t id, bool* out_enabled);

  zx_status_t ClockImplSetRate(uint32_t id, uint64_t hz);
  zx_status_t ClockImplQuerySupportedRate(uint32_t id, uint64_t max_rate, uint64_t* out_best_rate);
  zx_status_t ClockImplGetRate(uint32_t id, uint64_t* out_current_rate);

  zx_status_t ClockImplSetInput(uint32_t id, uint32_t idx);
  zx_status_t ClockImplGetNumInputs(uint32_t id, uint32_t* out);
  zx_status_t ClockImplGetInput(uint32_t id, uint32_t* out);

  void Measure(MeasureRequestView request, MeasureCompleter::Sync& completer);
  void GetCount(GetCountRequestView request, GetCountCompleter::Sync& completer);

 private:
  MtkClk(zx_device_t* parent, ddk::MmioBuffer mmio) : DeviceType(parent), mmio_(std::move(mmio)) {}

  ddk::MmioBuffer mmio_;
};

}  // namespace clk

#endif  // SRC_DEVICES_CLOCK_DRIVERS_MTK_CLK_MTK_CLK_H_
