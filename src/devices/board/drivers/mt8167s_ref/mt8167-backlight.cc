// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/hardware/platform/bus/c/banjo.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/device.h>
#include <lib/ddk/platform-defs.h>

#include <fbl/algorithm.h>
#include <soc/mt8167/mt8167-hw.h>

#include "mt8167.h"
#include "src/devices/board/drivers/mt8167s_ref/mt8167_bind.h"

namespace board_mt8167 {

zx_status_t Mt8167::BacklightInit() {
  if (board_info_.vid != PDEV_VID_GOOGLE || board_info_.pid != PDEV_PID_CLEO) {
    return ZX_OK;
  }

  // Add a composite device
  constexpr zx_bind_inst_t i2c_match[] = {
      BI_ABORT_IF(NE, BIND_PROTOCOL, ZX_PROTOCOL_I2C),
      BI_ABORT_IF(NE, BIND_I2C_BUS_ID, 2),
      BI_MATCH_IF(EQ, BIND_I2C_ADDRESS, 0x36),
  };
  constexpr zx_bind_inst_t gpio_match[] = {
      BI_ABORT_IF(NE, BIND_PROTOCOL, ZX_PROTOCOL_GPIO),
      BI_MATCH_IF(EQ, BIND_GPIO_PIN, MT8167_CLEO_GPIO_LCM_EN),
  };
  const device_fragment_part_t i2c_fragment[] = {
      {std::size(i2c_match), i2c_match},
  };
  const device_fragment_part_t gpio_fragment[] = {
      {std::size(gpio_match), gpio_match},
  };
  const device_fragment_t fragments[] = {
      {"i2c", std::size(i2c_fragment), i2c_fragment},
      {"gpio", std::size(gpio_fragment), gpio_fragment},
  };

  const zx_device_prop_t props[] = {
      {BIND_PLATFORM_DEV_VID, 0, PDEV_VID_GENERIC},
      {BIND_PLATFORM_DEV_PID, 0, PDEV_PID_GENERIC},
      {BIND_PLATFORM_DEV_DID, 0, PDEV_DID_SG_MICRO_SGM37603A},
  };

  const composite_device_desc_t comp_desc = {
      .props = props,
      .props_count = std::size(props),
      .fragments = fragments,
      .fragments_count = countof(fragments),
      .coresident_device_index = UINT32_MAX,
      .metadata_list = nullptr,
      .metadata_count = 0,
  };

  auto status = DdkAddComposite("sgm37603a", &comp_desc);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s: Failed to add SGM37603A device: %d", __FUNCTION__, status);
  }

  return ZX_OK;
}

}  // namespace board_mt8167
