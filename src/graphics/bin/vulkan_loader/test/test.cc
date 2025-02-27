// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/gpu/magma/cpp/fidl.h>
#include <fuchsia/io/cpp/fidl.h>
#include <fuchsia/vulkan/loader/cpp/fidl.h>
#include <lib/fdio/directory.h>
#include <lib/fzl/vmo-mapper.h>
#include <lib/zx/vmo.h>
#include <zircon/types.h>

#include <gtest/gtest.h>

TEST(VulkanLoader, ManifestLoad) {
  fuchsia::vulkan::loader::LoaderSyncPtr loader;
  EXPECT_EQ(ZX_OK, fdio_service_connect("/svc/fuchsia.vulkan.loader.Loader",
                                        loader.NewRequest().TakeChannel().release()));

  zx::vmo vmo_out;
  // manifest.json remaps this to bin/pkg-server.
  EXPECT_EQ(ZX_OK, loader->Get("pkg-server2", &vmo_out));
  EXPECT_TRUE(vmo_out.is_valid());
  zx_info_handle_basic_t handle_info;
  EXPECT_EQ(ZX_OK, vmo_out.get_info(ZX_INFO_HANDLE_BASIC, &handle_info, sizeof(handle_info),
                                    nullptr, nullptr));
  EXPECT_TRUE(handle_info.rights & ZX_RIGHT_EXECUTE);
  EXPECT_FALSE(handle_info.rights & ZX_RIGHT_WRITE);
  EXPECT_EQ(ZX_OK, loader->Get("not-present", &vmo_out));
  EXPECT_FALSE(vmo_out.is_valid());
}

// Check that writes to one VMO returned by the server will not modify a separate VMO returned by
// the service.
TEST(VulkanLoader, VmosIndependent) {
  fuchsia::vulkan::loader::LoaderSyncPtr loader;
  EXPECT_EQ(ZX_OK, fdio_service_connect("/svc/fuchsia.vulkan.loader.Loader",
                                        loader.NewRequest().TakeChannel().release()));

  zx::vmo vmo_out;
  // manifest.json remaps this to bin/pkg-server.
  EXPECT_EQ(ZX_OK, loader->Get("pkg-server2", &vmo_out));
  EXPECT_TRUE(vmo_out.is_valid());

  fzl::VmoMapper mapper;
  EXPECT_EQ(ZX_OK, mapper.Map(vmo_out, 0, 0, ZX_VM_PERM_EXECUTE | ZX_VM_PERM_READ));
  uint8_t original_value = *static_cast<uint8_t*>(mapper.start());
  uint8_t byte_to_write = original_value + 1;
  size_t actual;
  // zx_process_write_memory can write to memory mapped without ZX_VM_PERM_WRITE. If that ever
  // changes, this test can probably be removed.
  zx_status_t status = zx::process::self()->write_memory(
      reinterpret_cast<uint64_t>(mapper.start()), &byte_to_write, sizeof(byte_to_write), &actual);

  // zx_process_write_memory may be disabled using a kernel command-line flag.
  if (status == ZX_ERR_NOT_SUPPORTED) {
    EXPECT_EQ(original_value, *static_cast<uint8_t*>(mapper.start()));
  } else {
    EXPECT_EQ(ZX_OK, status);

    EXPECT_EQ(byte_to_write, *static_cast<uint8_t*>(mapper.start()));
  }

  // Ensure that the new clone is unaffected.
  zx::vmo vmo2;
  EXPECT_EQ(ZX_OK, loader->Get("pkg-server2", &vmo2));
  EXPECT_TRUE(vmo2.is_valid());

  fzl::VmoMapper mapper2;
  EXPECT_EQ(ZX_OK, mapper2.Map(vmo2, 0, 0, ZX_VM_PERM_EXECUTE | ZX_VM_PERM_READ));
  EXPECT_EQ(original_value, *static_cast<uint8_t*>(mapper2.start()));
}

TEST(VulkanLoader, DeviceFs) {
  fuchsia::vulkan::loader::LoaderSyncPtr loader;
  EXPECT_EQ(ZX_OK, fdio_service_connect("/svc/fuchsia.vulkan.loader.Loader",
                                        loader.NewRequest().TakeChannel().release()));

  fidl::InterfaceHandle<fuchsia::io::Directory> dir;
  EXPECT_EQ(ZX_OK, loader->ConnectToDeviceFs(dir.NewRequest().TakeChannel()));

  zx::vmo vmo_out;
  // Waiting for this will ensure that the device exists.
  EXPECT_EQ(ZX_OK, loader->Get("pkg-server2", &vmo_out));

  fuchsia::gpu::magma::DeviceSyncPtr device_ptr;
  EXPECT_EQ(ZX_OK, fdio_service_connect_at(dir.channel().get(), "class/gpu/000",
                                           device_ptr.NewRequest().TakeChannel().release()));
  fuchsia::gpu::magma::Device_Query2_Result query_result;
  EXPECT_EQ(ZX_OK, device_ptr->Query2(0u, &query_result));
  ASSERT_TRUE(query_result.is_response());
  EXPECT_EQ(5u, query_result.response().result);
}

TEST(VulkanLoader, Features) {
  fuchsia::vulkan::loader::LoaderSyncPtr loader;
  EXPECT_EQ(ZX_OK, fdio_service_connect("/svc/fuchsia.vulkan.loader.Loader",
                                        loader.NewRequest().TakeChannel().release()));

  fuchsia::vulkan::loader::Features features;
  EXPECT_EQ(ZX_OK, loader->GetSupportedFeatures(&features));
  constexpr fuchsia::vulkan::loader::Features kExpectedFeatures =
      fuchsia::vulkan::loader::Features::CONNECT_TO_DEVICE_FS |
      fuchsia::vulkan::loader::Features::GET;
  EXPECT_EQ(kExpectedFeatures, features);
}
