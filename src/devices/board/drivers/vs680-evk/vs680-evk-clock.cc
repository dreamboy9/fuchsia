// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/ddk/debug.h>
#include <lib/ddk/metadata.h>
#include <lib/ddk/platform-defs.h>

#include <ddk/metadata/clock.h>
#include <soc/vs680/vs680-clk.h>
#include <soc/vs680/vs680-hw.h>

#include "vs680-evk.h"

namespace board_vs680_evk {

zx_status_t Vs680Evk::ClockInit() {
  constexpr pbus_mmio_t clock_mmios[] = {
      {
          .base = vs680::kChipCtrlBase,
          .length = vs680::kChipCtrlSize,
      },
      {
          .base = vs680::kCpuPllBase,
          .length = vs680::kCpuPllSize,
      },
      {
          .base = vs680::kAvioBase,
          .length = vs680::kAvioSize,
      },
  };

  constexpr clock_id_t clock_ids[] = {
      {vs680::kCpuPll},
      {vs680::kSd0Clock},
  };

  const pbus_metadata_t clock_metadata[] = {
      {
          .type = DEVICE_METADATA_CLOCK_IDS,
          .data_buffer = reinterpret_cast<const uint8_t*>(&clock_ids),
          .data_size = sizeof(clock_ids),
      },
  };

  pbus_dev_t dev = {};
  dev.name = "vs680-clock";
  dev.vid = PDEV_VID_SYNAPTICS;
  dev.did = PDEV_DID_VS680_CLOCK;
  dev.mmio_list = clock_mmios;
  dev.mmio_count = countof(clock_mmios);
  dev.metadata_list = clock_metadata;
  dev.metadata_count = countof(clock_metadata);

  auto status = pbus_.DeviceAdd(&dev);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s: DeviceAdd failed %d", __func__, status);
    return status;
  }

  return ZX_OK;
}

}  // namespace board_vs680_evk
