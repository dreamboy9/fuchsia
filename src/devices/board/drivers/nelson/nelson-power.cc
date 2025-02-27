// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/ddk/binding.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/device.h>
#include <lib/ddk/metadata.h>
#include <lib/ddk/platform-defs.h>

#include "nelson.h"
#include "src/devices/power/drivers/ti-ina231/ti-ina231-metadata.h"

namespace nelson {

// These values are specific to Nelson, and are only used within this board driver.
enum : uint32_t {
  kPowerSensorDomainMlb = 0,
  kPowerSensorDomainAudio = 1,
};

constexpr power_sensor::Ina231Metadata kMlbSensorMetadata = {
    .mode = power_sensor::Ina231Metadata::kModeShuntAndBusContinuous,
    .shunt_voltage_conversion_time = power_sensor::Ina231Metadata::kConversionTime332us,
    .bus_voltage_conversion_time = power_sensor::Ina231Metadata::kConversionTime332us,
    .averages = power_sensor::Ina231Metadata::kAverages1024,
    .shunt_resistance_microohm = 10'000,
    .bus_voltage_limit_microvolt = 0,
    .alert = power_sensor::Ina231Metadata::kAlertNone,
    .power_sensor_domain = kPowerSensorDomainMlb,
};

constexpr power_sensor::Ina231Metadata kAudioSensorMetadata = {
    .mode = power_sensor::Ina231Metadata::kModeShuntAndBusContinuous,
    .shunt_voltage_conversion_time = power_sensor::Ina231Metadata::kConversionTime332us,
    .bus_voltage_conversion_time = power_sensor::Ina231Metadata::kConversionTime332us,
    .averages = power_sensor::Ina231Metadata::kAverages1024,
    .shunt_resistance_microohm = 10'000,
    .bus_voltage_limit_microvolt = 11'000'000,
    .alert = power_sensor::Ina231Metadata::kAlertBusUnderVoltage,
    .power_sensor_domain = kPowerSensorDomainAudio,
};

constexpr device_metadata_t kMlbMetadata[] = {
    {
        .type = DEVICE_METADATA_PRIVATE,
        .data = &kMlbSensorMetadata,
        .length = sizeof(kMlbSensorMetadata),
    },
};

constexpr device_metadata_t kAudioMetadata[] = {
    {
        .type = DEVICE_METADATA_PRIVATE,
        .data = &kAudioSensorMetadata,
        .length = sizeof(kAudioSensorMetadata),
    },
};


constexpr zx_bind_inst_t mlb_i2c_match[] = {
    BI_ABORT_IF(NE, BIND_PROTOCOL, ZX_PROTOCOL_I2C),
    BI_ABORT_IF(NE, BIND_I2C_BUS_ID, NELSON_I2C_3),
    BI_MATCH_IF(EQ, BIND_I2C_ADDRESS, I2C_TI_INA231_MLB_ADDR),
};

constexpr device_fragment_part_t mlb_i2c_fragment[] = {
    {countof(mlb_i2c_match), mlb_i2c_match},
};

constexpr device_fragment_t mlb_fragments[] = {
    {"i2c", countof(mlb_i2c_fragment), mlb_i2c_fragment},
};

constexpr zx_bind_inst_t speakers_i2c_match[] = {
    BI_ABORT_IF(NE, BIND_PROTOCOL, ZX_PROTOCOL_I2C),
    BI_ABORT_IF(NE, BIND_I2C_BUS_ID, NELSON_I2C_3),
    BI_MATCH_IF(EQ, BIND_I2C_ADDRESS, I2C_TI_INA231_SPEAKERS_ADDR),
};

constexpr device_fragment_part_t speakers_i2c_fragment[] = {
    {countof(speakers_i2c_match), speakers_i2c_match},
};

constexpr device_fragment_t speakers_fragments[] = {
    {"i2c", countof(speakers_i2c_fragment), speakers_i2c_fragment},
};

constexpr zx_device_prop_t props[] = {
    {BIND_PLATFORM_DEV_VID, 0, PDEV_VID_TI},
    {BIND_PLATFORM_DEV_DID, 0, PDEV_DID_TI_INA231},
};

constexpr composite_device_desc_t mlb_power_sensor_dev = {
    .props = props,
    .props_count = countof(props),
    .fragments = mlb_fragments,
    .fragments_count = countof(mlb_fragments),
    .coresident_device_index = 0,
    .metadata_list = kMlbMetadata,
    .metadata_count = countof(kMlbMetadata),
};

constexpr composite_device_desc_t speakers_power_sensor_dev = {
    .props = props,
    .props_count = countof(props),
    .fragments = speakers_fragments,
    .fragments_count = countof(speakers_fragments),
    .coresident_device_index = 0,
    .metadata_list = kAudioMetadata,
    .metadata_count = countof(kAudioMetadata),
};

zx_status_t Nelson::PowerInit() {
  zx_status_t status = DdkAddComposite("ti-ina231-mlb", &mlb_power_sensor_dev);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s DdkAddComposite failed %d", __FUNCTION__, status);
    return status;
  }

  if ((status = DdkAddComposite("ti-ina231-speakers", &speakers_power_sensor_dev)) != ZX_OK) {
    zxlogf(ERROR, "%s DdkAddComposite failed %d", __FUNCTION__, status);
    return status;
  }

  return ZX_OK;
}

}  // namespace nelson
