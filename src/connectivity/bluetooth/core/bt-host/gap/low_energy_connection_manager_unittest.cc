// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/bluetooth/core/bt-host/gap/low_energy_connection_manager.h"

#include <lib/fit/function.h>
#include <lib/inspect/testing/cpp/inspect.h>
#include <zircon/assert.h>

#include <cstddef>
#include <limits>
#include <memory>
#include <vector>

#include <fbl/macros.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/connectivity/bluetooth/core/bt-host/common/byte_buffer.h"
#include "src/connectivity/bluetooth/core/bt-host/common/device_address.h"
#include "src/connectivity/bluetooth/core/bt-host/common/random.h"
#include "src/connectivity/bluetooth/core/bt-host/gap/fake_pairing_delegate.h"
#include "src/connectivity/bluetooth/core/bt-host/gap/gap.h"
#include "src/connectivity/bluetooth/core/bt-host/gap/low_energy_address_manager.h"
#include "src/connectivity/bluetooth/core/bt-host/gap/low_energy_connection_manager.h"
#include "src/connectivity/bluetooth/core/bt-host/gap/peer.h"
#include "src/connectivity/bluetooth/core/bt-host/gap/peer_cache.h"
#include "src/connectivity/bluetooth/core/bt-host/gatt/fake_layer.h"
#include "src/connectivity/bluetooth/core/bt-host/hci-spec/constants.h"
#include "src/connectivity/bluetooth/core/bt-host/hci-spec/defaults.h"
#include "src/connectivity/bluetooth/core/bt-host/hci-spec/util.h"
#include "src/connectivity/bluetooth/core/bt-host/hci/fake_local_address_delegate.h"
#include "src/connectivity/bluetooth/core/bt-host/hci/legacy_low_energy_scanner.h"
#include "src/connectivity/bluetooth/core/bt-host/hci/low_energy_connector.h"
#include "src/connectivity/bluetooth/core/bt-host/l2cap/fake_channel.h"
#include "src/connectivity/bluetooth/core/bt-host/l2cap/fake_channel_test.h"
#include "src/connectivity/bluetooth/core/bt-host/l2cap/fake_l2cap.h"
#include "src/connectivity/bluetooth/core/bt-host/l2cap/l2cap_defs.h"
#include "src/connectivity/bluetooth/core/bt-host/sm/smp.h"
#include "src/connectivity/bluetooth/core/bt-host/sm/test_security_manager.h"
#include "src/connectivity/bluetooth/core/bt-host/sm/types.h"
#include "src/connectivity/bluetooth/core/bt-host/testing/controller_test.h"
#include "src/connectivity/bluetooth/core/bt-host/testing/fake_controller.h"
#include "src/connectivity/bluetooth/core/bt-host/testing/fake_peer.h"
#include "src/connectivity/bluetooth/core/bt-host/testing/test_packets.h"

namespace bt::gap {
namespace {

using namespace inspect::testing;

using bt::sm::BondableMode;
using bt::testing::FakeController;
using bt::testing::FakePeer;

using TestingBase = bt::testing::ControllerTest<FakeController>;
using l2cap::testing::FakeChannel;
using TestSm = sm::testing::TestSecurityManager;
using TestSmFactory = sm::testing::TestSecurityManagerFactory;
using ConnectionResult = LowEnergyConnectionManager::ConnectionResult;

const DeviceAddress kAddress0(DeviceAddress::Type::kLEPublic, {1});
const DeviceAddress kAddrAlias0(DeviceAddress::Type::kBREDR, kAddress0.value());
const DeviceAddress kAddress1(DeviceAddress::Type::kLERandom, {2});
const DeviceAddress kAddress2(DeviceAddress::Type::kBREDR, {3});
const DeviceAddress kAddress3(DeviceAddress::Type::kLEPublic, {4});
const DeviceAddress kAdapterAddress(DeviceAddress::Type::kLEPublic, {9});

const size_t kLEMaxNumPackets = 10;
const hci::DataBufferInfo kLEDataBufferInfo(hci::kMaxACLPayloadSize, kLEMaxNumPackets);

constexpr std::array kConnectDelays = {zx::sec(0), zx::sec(2), zx::sec(4)};

const LowEnergyConnectionOptions kConnectionOptions{};

class LowEnergyConnectionManagerTest : public TestingBase {
 public:
  LowEnergyConnectionManagerTest() = default;
  ~LowEnergyConnectionManagerTest() override = default;

 protected:
  void SetUp() override {
    TestingBase::SetUp();

    // Initialize with LE buffers only.
    TestingBase::InitializeACLDataChannel(hci::DataBufferInfo(), kLEDataBufferInfo);

    FakeController::Settings settings;
    settings.ApplyLegacyLEConfig();
    test_device()->set_settings(settings);

    peer_cache_ = std::make_unique<PeerCache>();
    l2cap_ = l2cap::testing::FakeL2cap::Create();

    connector_ = std::make_unique<hci::LowEnergyConnector>(
        transport()->WeakPtr(), &addr_delegate_, dispatcher(),
        fit::bind_member(this, &LowEnergyConnectionManagerTest::OnIncomingConnection));

    gatt_ = std::make_unique<gatt::testing::FakeLayer>();
    sm_factory_ = std::make_unique<TestSmFactory>();

    address_manager_ = std::make_unique<LowEnergyAddressManager>(
        kAdapterAddress, /*delegate=*/[] { return false; }, transport()->WeakPtr());
    scanner_ = std::make_unique<hci::LegacyLowEnergyScanner>(address_manager_.get(),
                                                             transport()->WeakPtr(), dispatcher());
    discovery_manager_ = std::make_unique<LowEnergyDiscoveryManager>(
        transport()->WeakPtr(), scanner_.get(), peer_cache_.get());
    conn_mgr_ = std::make_unique<LowEnergyConnectionManager>(
        transport()->WeakPtr(), &addr_delegate_, connector_.get(), peer_cache_.get(), l2cap_,
        gatt_->AsWeakPtr(), discovery_manager_->GetWeakPtr(),
        fit::bind_member(sm_factory_.get(), &TestSmFactory::CreateSm));

    test_device()->set_connection_state_callback(
        fit::bind_member(this, &LowEnergyConnectionManagerTest::OnConnectionStateChanged));
    StartTestDevice();
  }

  void TearDown() override {
    if (conn_mgr_) {
      conn_mgr_ = nullptr;
    }
    discovery_manager_ = nullptr;
    scanner_ = nullptr;
    address_manager_ = nullptr;
    gatt_ = nullptr;
    connector_ = nullptr;
    peer_cache_ = nullptr;

    l2cap_ = nullptr;

    TestingBase::TearDown();
  }

  // Deletes |conn_mgr_|.
  void DeleteConnMgr() { conn_mgr_ = nullptr; }

  PeerCache* peer_cache() const { return peer_cache_.get(); }
  LowEnergyConnectionManager* conn_mgr() const { return conn_mgr_.get(); }
  l2cap::testing::FakeL2cap* fake_l2cap() const { return l2cap_.get(); }
  gatt::testing::FakeLayer* fake_gatt() { return gatt_.get(); }
  LowEnergyDiscoveryManager* discovery_mgr() { return discovery_manager_.get(); }

  // Addresses of currently connected fake peers.
  using PeerList = std::unordered_set<DeviceAddress>;
  const PeerList& connected_peers() const { return connected_peers_; }

  // Addresses of peers with a canceled connection attempt.
  const PeerList& canceled_peers() const { return canceled_peers_; }

  hci::ConnectionPtr MoveLastRemoteInitiated() { return std::move(last_remote_initiated_); }

  fxl::WeakPtr<TestSm> TestSmByHandle(hci::ConnectionHandle handle) {
    return sm_factory_->GetTestSm(handle);
  }

 private:
  // Called by |connector_| when a new remote initiated connection is received.
  void OnIncomingConnection(hci::ConnectionHandle handle, hci::Connection::Role role,
                            std::optional<DeviceAddress> opt_local_address,
                            const DeviceAddress& peer_address,
                            const hci::LEConnectionParameters& conn_params) {
    DeviceAddress local_address(DeviceAddress::Type::kLEPublic, {3, 2, 1, 1, 2, 3});

    // Create a production connection object that can interact with the fake
    // controller.
    last_remote_initiated_ = hci::Connection::CreateLE(handle, role, local_address, peer_address,
                                                       conn_params, transport()->WeakPtr());
  }

  // Called by FakeController on connection events.
  void OnConnectionStateChanged(const DeviceAddress& address, hci::ConnectionHandle handle,
                                bool connected, bool canceled) {
    bt_log(DEBUG, "gap-test",
           "OnConnectionStateChanged: %s (handle: %#.4x) (connected: %s) (canceled: %s):\n",
           address.ToString().c_str(), handle, (connected ? "true" : "false"),
           (canceled ? "true" : "false"));
    if (canceled) {
      canceled_peers_.insert(address);
    } else if (connected) {
      ZX_DEBUG_ASSERT(connected_peers_.find(address) == connected_peers_.end());
      connected_peers_.insert(address);
    } else {
      ZX_DEBUG_ASSERT(connected_peers_.find(address) != connected_peers_.end());
      connected_peers_.erase(address);
    }
  }

  fbl::RefPtr<l2cap::testing::FakeL2cap> l2cap_;

  hci::FakeLocalAddressDelegate addr_delegate_;
  std::unique_ptr<PeerCache> peer_cache_;
  std::unique_ptr<hci::LowEnergyConnector> connector_;
  std::unique_ptr<gatt::testing::FakeLayer> gatt_;
  std::unique_ptr<TestSmFactory> sm_factory_;
  std::unique_ptr<hci::LegacyLowEnergyScanner> scanner_;
  std::unique_ptr<LowEnergyAddressManager> address_manager_;
  std::unique_ptr<LowEnergyDiscoveryManager> discovery_manager_;
  std::unique_ptr<LowEnergyConnectionManager> conn_mgr_;

  // The most recent remote-initiated connection reported by |connector_|.
  hci::ConnectionPtr last_remote_initiated_;

  PeerList connected_peers_;
  PeerList canceled_peers_;

  DISALLOW_COPY_AND_ASSIGN_ALLOW_MOVE(LowEnergyConnectionManagerTest);
};

using GAP_LowEnergyConnectionManagerTest = LowEnergyConnectionManagerTest;

TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectUnknownPeer) {
  constexpr PeerId kUnknownId(1);
  ConnectionResult result;
  conn_mgr()->Connect(
      kUnknownId, [&result](auto res) { result = std::move(res); }, kConnectionOptions);
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(result.error(), HostError::kNotFound);
}

TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectClassicPeer) {
  auto* peer = peer_cache()->NewPeer(kAddress2, true);
  ConnectionResult result;
  conn_mgr()->Connect(
      peer->identifier(), [&result](auto res) { result = std::move(res); }, kConnectionOptions);
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(result.error(), HostError::kNotFound);
}

TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectNonConnectablePeer) {
  auto* peer = peer_cache()->NewPeer(kAddress0, false);
  ConnectionResult result;
  conn_mgr()->Connect(
      peer->identifier(), [&result](auto res) { result = std::move(res); }, kConnectionOptions);
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(result.error(), HostError::kNotFound);
}

// An error is received via the HCI Command cb_status event
TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectSinglePeerErrorStatus) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  fake_peer->set_connect_status(hci::StatusCode::kConnectionFailedToBeEstablished);
  test_device()->AddPeer(std::move(fake_peer));

  ASSERT_TRUE(peer->le());
  EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer->le()->connection_state());

  ConnectionResult result;
  auto callback = [&result](auto res) { result = std::move(res); };

  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());

  RunLoopUntilIdle();

  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(HostError::kFailed, result.error());
  EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer->le()->connection_state());
}

// LE Connection Complete event reports error
TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectSinglePeerFailure) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  fake_peer->set_connect_response(hci::StatusCode::kConnectionFailedToBeEstablished);
  test_device()->AddPeer(std::move(fake_peer));

  ConnectionResult result;
  auto callback = [&result](auto res) { result = std::move(res); };

  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  ASSERT_TRUE(peer->le());
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());

  RunLoopUntilIdle();

  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(HostError::kFailed, result.error());
  EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer->le()->connection_state());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectSinglePeerScanTimeout) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);

  // We add no fake peers to cause the scan to time out.

  ConnectionResult result;
  auto callback = [&result](auto res) { result = std::move(res); };

  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  ASSERT_TRUE(peer->le());
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());

  RunLoopFor(kLEGeneralCepScanTimeout);

  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(HostError::kTimedOut, result.error());
  EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer->le()->connection_state());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectSinglePeerAlreadyInScanCache) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  // Ensure peer is in scan cache by doing active discovery.
  LowEnergyDiscoverySessionPtr session;
  discovery_mgr()->StartDiscovery(/*active=*/true,
                                  [&session](auto cb_session) { session = std::move(cb_session); });
  RunLoopUntilIdle();

  ConnectionResult result;
  auto callback = [&result](auto res) { result = std::move(res); };

  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  RunLoopUntilIdle();
  ASSERT_TRUE(result.is_ok());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectSinglePeerRequestTimeout) {
  constexpr zx::duration kTestRequestTimeout = zx::sec(20);

  auto* peer = peer_cache()->NewPeer(kAddress0, true);

  // Add a fake peer so that scan succeeds but connect stalls.
  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  fake_peer->set_force_pending_connect(true);
  test_device()->AddPeer(std::move(fake_peer));

  ConnectionResult result;
  auto callback = [&result](auto res) { result = std::move(res); };

  conn_mgr()->set_request_timeout_for_testing(kTestRequestTimeout);
  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  ASSERT_TRUE(peer->le());
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());

  RunLoopFor(kTestRequestTimeout);
  RunLoopUntilIdle();

  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(HostError::kTimedOut, result.error());
  EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer->le()->connection_state());
}

// Tests that an entry in the cache does not expire while a connection attempt
// is pending.
TEST_F(GAP_LowEnergyConnectionManagerTest, PeerDoesNotExpireDuringTimeout) {
  // Set a connection timeout that is longer than the PeerCache expiry
  // timeout.
  // TODO(fxbug.dev/1418): Consider configuring the cache timeout explicitly rather than
  // relying on the kCacheTimeout constant.
  constexpr zx::duration kTestRequestTimeout = kCacheTimeout + zx::sec(1);
  conn_mgr()->set_request_timeout_for_testing(kTestRequestTimeout);

  // Note: Use a random address so that the peer becomes temporary upon failure.
  auto* peer = peer_cache()->NewPeer(kAddress1, true);
  EXPECT_TRUE(peer->temporary());

  ConnectionResult result;
  auto callback = [&result](auto res) { result = std::move(res); };

  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  ASSERT_TRUE(peer->le());
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());
  EXPECT_FALSE(peer->temporary());

  RunLoopFor(kTestRequestTimeout);
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(HostError::kTimedOut, result.error());
  EXPECT_EQ(peer, peer_cache()->FindByAddress(kAddress1));
  EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer->le()->connection_state());
  EXPECT_TRUE(peer->temporary());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, PeerDoesNotExpireDuringDelayedConnect) {
  // Make the connection resolve after a delay that is longer than the cache
  // timeout.
  constexpr zx::duration kConnectionDelay = kCacheTimeout + zx::sec(1);
  FakeController::Settings settings;
  settings.ApplyLegacyLEConfig();
  settings.le_connection_delay = kConnectionDelay;
  test_device()->set_settings(settings);

  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  auto id = peer->identifier();
  EXPECT_TRUE(peer->temporary());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  // Make sure the connection request doesn't time out while waiting for a
  // response.
  conn_mgr()->set_request_timeout_for_testing(kConnectionDelay + zx::sec(1));

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto callback = [&conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
    ASSERT_TRUE(conn_handle);
    EXPECT_TRUE(conn_handle->active());
  };
  conn_mgr()->Connect(id, callback, kConnectionOptions);
  ASSERT_TRUE(peer->le());
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());

  RunLoopFor(kConnectionDelay);
  ASSERT_TRUE(conn_handle);

  // The peer should not have expired during this time.
  peer = peer_cache()->FindByAddress(kAddress0);
  ASSERT_TRUE(peer);
  EXPECT_EQ(id, peer->identifier());
  EXPECT_TRUE(peer->connected());
  EXPECT_FALSE(peer->temporary());
}

// Successful connection to single peer
TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectSinglePeer) {
  auto* peer = peer_cache()->NewPeer(kAddress0, /*connectable=*/true);
  EXPECT_TRUE(peer->temporary());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  std::optional<hci::LECreateConnectionCommandParams> connect_params;
  test_device()->set_le_create_connection_command_callback(
      [&](auto params) { connect_params = params; });

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto callback = [&conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
    EXPECT_TRUE(conn_handle->active());
  };

  EXPECT_TRUE(connected_peers().empty());
  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  ASSERT_TRUE(peer->le());
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());

  RunLoopUntilIdle();

  EXPECT_EQ(1u, connected_peers().size());
  EXPECT_EQ(1u, connected_peers().count(kAddress0));

  ASSERT_TRUE(conn_handle);
  EXPECT_TRUE(conn_handle->active());
  EXPECT_EQ(peer->identifier(), conn_handle->peer_identifier());
  EXPECT_FALSE(peer->temporary());
  EXPECT_EQ(Peer::ConnectionState::kConnected, peer->le()->connection_state());
  ASSERT_TRUE(connect_params);
  EXPECT_EQ(letoh16(connect_params->scan_interval), kLEScanFastInterval);
  EXPECT_EQ(letoh16(connect_params->scan_window), kLEScanFastWindow);
}

struct TestObject final : fbl::RefCounted<TestObject> {
  explicit TestObject(bool* d) : deleted(d) {
    ZX_DEBUG_ASSERT(deleted);
    *deleted = false;
  }

  ~TestObject() { *deleted = true; }

  bool* deleted;
};

TEST_F(GAP_LowEnergyConnectionManagerTest, DeleteRefInClosedCallback) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));

  bool deleted = false;
  auto obj = fbl::AdoptRef(new TestObject(&deleted));
  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  int closed_count = 0;
  auto closed_cb = [&, obj = std::move(obj)] {
    closed_count++;
    conn_handle = nullptr;

    // The object should remain alive for the duration of this callback.
    EXPECT_FALSE(deleted);
  };

  auto success_cb = [&conn_handle, &closed_cb](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
    conn_handle->set_closed_callback(std::move(closed_cb));
  };

  conn_mgr()->Connect(peer->identifier(), success_cb, kConnectionOptions);
  RunLoopUntilIdle();

  ASSERT_TRUE(conn_handle);
  ASSERT_TRUE(conn_handle->active());

  // This will trigger the closed callback.
  EXPECT_TRUE(conn_mgr()->Disconnect(peer->identifier()));
  RunLoopUntilIdle();

  EXPECT_EQ(1, closed_count);
  EXPECT_TRUE(connected_peers().empty());
  EXPECT_FALSE(conn_handle);

  // The object should be deleted.
  EXPECT_TRUE(deleted);
}

TEST_F(GAP_LowEnergyConnectionManagerTest, ReleaseRef) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto callback = [&conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
    EXPECT_TRUE(conn_handle->active());
  };

  EXPECT_TRUE(connected_peers().empty());
  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);

  RunLoopUntilIdle();

  EXPECT_EQ(1u, connected_peers().size());
  ASSERT_TRUE(peer->le());
  EXPECT_EQ(Peer::ConnectionState::kConnected, peer->le()->connection_state());

  ASSERT_TRUE(conn_handle);
  conn_handle = nullptr;

  RunLoopUntilIdle();

  EXPECT_TRUE(connected_peers().empty());
  EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer->le()->connection_state());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, OnePeerTwoPendingRequestsBothFail) {
  constexpr int kRequestCount = 2;

  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  fake_peer->set_connect_response(hci::StatusCode::kConnectionFailedToBeEstablished);
  test_device()->AddPeer(std::move(fake_peer));

  ConnectionResult results[kRequestCount];

  int cb_count = 0;
  auto callback = [&results, &cb_count](auto result) { results[cb_count++] = std::move(result); };

  for (int i = 0; i < kRequestCount; ++i) {
    conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  }

  RunLoopUntilIdle();

  ASSERT_EQ(kRequestCount, cb_count);
  for (int i = 0; i < kRequestCount; ++i) {
    ASSERT_TRUE(results[i].is_error());
    EXPECT_EQ(HostError::kFailed, results[i].error()) << "request count: " << i + 1;
  }
}

TEST_F(GAP_LowEnergyConnectionManagerTest, OnePeerManyPendingRequests) {
  constexpr size_t kRequestCount = 50;

  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  std::vector<std::unique_ptr<LowEnergyConnectionHandle>> conn_handles;
  auto callback = [&conn_handles](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handles.emplace_back(result.take_value());
  };

  for (size_t i = 0; i < kRequestCount; ++i) {
    conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  }

  RunLoopUntilIdle();

  EXPECT_EQ(1u, connected_peers().size());
  EXPECT_EQ(1u, connected_peers().count(kAddress0));

  EXPECT_EQ(kRequestCount, conn_handles.size());
  for (size_t i = 0; i < kRequestCount; ++i) {
    ASSERT_TRUE(conn_handles[i]);
    EXPECT_TRUE(conn_handles[i]->active());
    EXPECT_EQ(peer->identifier(), conn_handles[i]->peer_identifier());
  }

  // Release one reference. The rest should be active.
  conn_handles[0] = nullptr;
  for (size_t i = 1; i < kRequestCount; ++i)
    EXPECT_TRUE(conn_handles[i]->active());

  // Release all but one reference.
  for (size_t i = 1; i < kRequestCount - 1; ++i)
    conn_handles[i] = nullptr;
  EXPECT_TRUE(conn_handles[kRequestCount - 1]->active());

  // Drop the last reference.
  conn_handles[kRequestCount - 1] = nullptr;

  RunLoopUntilIdle();

  EXPECT_TRUE(connected_peers().empty());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, AddRefAfterConnection) {
  constexpr size_t kRefCount = 50;

  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  std::vector<std::unique_ptr<LowEnergyConnectionHandle>> conn_handles;
  auto callback = [&conn_handles](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handles.emplace_back(result.take_value());
  };

  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);

  RunLoopUntilIdle();

  EXPECT_EQ(1u, connected_peers().size());
  EXPECT_EQ(1u, connected_peers().count(kAddress0));
  EXPECT_EQ(1u, conn_handles.size());

  // Add new references.
  for (size_t i = 1; i < kRefCount; ++i) {
    conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
    RunLoopUntilIdle();
  }

  EXPECT_EQ(1u, connected_peers().size());
  EXPECT_EQ(1u, connected_peers().count(kAddress0));
  EXPECT_EQ(kRefCount, conn_handles.size());

  // Disconnect.
  conn_handles.clear();

  RunLoopUntilIdle();

  EXPECT_TRUE(connected_peers().empty());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, PendingRequestsOnTwoPeers) {
  auto* peer0 = peer_cache()->NewPeer(kAddress0, true);
  auto* peer1 = peer_cache()->NewPeer(kAddress1, true);

  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress1));

  std::vector<std::unique_ptr<LowEnergyConnectionHandle>> conn_handles;
  auto callback = [&conn_handles](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handles.emplace_back(result.take_value());
  };

  conn_mgr()->Connect(peer0->identifier(), callback, kConnectionOptions);
  conn_mgr()->Connect(peer1->identifier(), callback, kConnectionOptions);

  RunLoopUntilIdle();

  EXPECT_EQ(2u, connected_peers().size());
  EXPECT_EQ(1u, connected_peers().count(kAddress0));
  EXPECT_EQ(1u, connected_peers().count(kAddress1));

  ASSERT_EQ(2u, conn_handles.size());
  ASSERT_TRUE(conn_handles[0]);
  ASSERT_TRUE(conn_handles[1]);
  EXPECT_EQ(peer0->identifier(), conn_handles[0]->peer_identifier());
  EXPECT_EQ(peer1->identifier(), conn_handles[1]->peer_identifier());

  // |peer1| should disconnect first.
  conn_handles[1] = nullptr;

  RunLoopUntilIdle();

  EXPECT_EQ(1u, connected_peers().size());
  EXPECT_EQ(1u, connected_peers().count(kAddress0));

  conn_handles.clear();

  RunLoopUntilIdle();
  EXPECT_TRUE(connected_peers().empty());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, PendingRequestsOnTwoPeersOneFails) {
  auto* peer0 = peer_cache()->NewPeer(kAddress0, true);
  auto* peer1 = peer_cache()->NewPeer(kAddress1, true);

  auto fake_peer0 = std::make_unique<FakePeer>(kAddress0);
  fake_peer0->set_connect_response(hci::StatusCode::kConnectionFailedToBeEstablished);
  test_device()->AddPeer(std::move(fake_peer0));
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress1));

  std::vector<ConnectionResult> conn_results;
  auto callback = [&conn_results](auto result) { conn_results.emplace_back(std::move(result)); };

  conn_mgr()->Connect(peer0->identifier(), callback, kConnectionOptions);
  conn_mgr()->Connect(peer1->identifier(), callback, kConnectionOptions);

  RunLoopUntilIdle();

  EXPECT_EQ(1u, connected_peers().size());
  EXPECT_EQ(1u, connected_peers().count(kAddress1));

  ASSERT_EQ(2u, conn_results.size());
  EXPECT_TRUE(conn_results[0].is_error());
  ASSERT_TRUE(conn_results[1].is_ok());
  EXPECT_EQ(peer1->identifier(), conn_results[1].value()->peer_identifier());

  // Both connections should disconnect.
  conn_results.clear();

  RunLoopUntilIdle();
  EXPECT_TRUE(connected_peers().empty());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, Destructor) {
  auto* peer0 = peer_cache()->NewPeer(kAddress0, true);
  auto* peer1 = peer_cache()->NewPeer(kAddress1, true);

  // Connecting to this peer will succeed.
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));

  // Connecting to this peer will remain pending.
  auto pending_peer = std::make_unique<FakePeer>(kAddress1);
  pending_peer->set_force_pending_connect(true);
  test_device()->AddPeer(std::move(pending_peer));

  // Below we create one connection and one pending request to have at the time
  // of destruction.

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto success_cb = [&conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
  };

  conn_mgr()->Connect(peer0->identifier(), success_cb, kConnectionOptions);
  RunLoopUntilIdle();

  ASSERT_TRUE(conn_handle);
  bool conn_closed = false;
  conn_handle->set_closed_callback([&conn_closed] { conn_closed = true; });

  bool error_cb_called = false;
  auto error_cb = [&error_cb_called](auto result) {
    ASSERT_TRUE(result.is_error());
    EXPECT_EQ(HostError::kCanceled, result.error());
    error_cb_called = true;
  };

  // This will send an HCI command to the fake controller. We delete the
  // connection manager before a connection event gets received which should
  // cancel the connection.
  conn_mgr()->Connect(peer1->identifier(), error_cb, kConnectionOptions);
  RunLoopUntilIdle();
  EXPECT_FALSE(error_cb_called);

  DeleteConnMgr();

  RunLoopUntilIdle();

  EXPECT_TRUE(error_cb_called);
  EXPECT_TRUE(conn_closed);
  EXPECT_EQ(1u, canceled_peers().size());
  EXPECT_EQ(1u, canceled_peers().count(kAddress1));
}

TEST_F(GAP_LowEnergyConnectionManagerTest, DisconnectPendingConnectionWhileAwaitingScanStart) {
  auto peer_0 = peer_cache()->NewPeer(kAddress0, true);
  auto peer_1 = peer_cache()->NewPeer(kAddress1, true);
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress1));

  int conn_cb_0_count = 0;
  auto conn_cb_0 = [&](auto result) {
    ASSERT_TRUE(result.is_error());
    EXPECT_EQ(result.error(), HostError::kCanceled);
    conn_cb_0_count++;
  };

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto conn_cb_1 = [&](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
  };

  conn_mgr()->Connect(peer_0->identifier(), conn_cb_0, kConnectionOptions);
  conn_mgr()->Connect(peer_1->identifier(), conn_cb_1, kConnectionOptions);
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer_0->le()->connection_state());
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer_1->le()->connection_state());

  // Do NOT wait for scanning to start asynchronously before calling Disconnect synchronously.
  // After peer_0's connection request is cancelled, peer_1's connection request should succeed.
  EXPECT_TRUE(conn_mgr()->Disconnect(peer_0->identifier()));
  RunLoopUntilIdle();
  EXPECT_EQ(conn_cb_0_count, 1);
  ASSERT_TRUE(conn_handle);
  EXPECT_EQ(conn_handle->peer_identifier(), peer_1->identifier());
  EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer_0->le()->connection_state());
  EXPECT_EQ(Peer::ConnectionState::kConnected, peer_1->le()->connection_state());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, DisconnectPendingConnectionDuringScan) {
  // Don't add FakePeer for peer_0 in order to stall during scanning.
  auto peer_0 = peer_cache()->NewPeer(kAddress0, true);
  auto peer_1 = peer_cache()->NewPeer(kAddress1, true);
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress1));

  int conn_cb_0_count = 0;
  auto conn_cb_0 = [&](auto result) {
    ASSERT_TRUE(result.is_error());
    EXPECT_EQ(result.error(), HostError::kCanceled);
    conn_cb_0_count++;
  };

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto conn_cb_1 = [&](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
  };

  conn_mgr()->Connect(peer_0->identifier(), conn_cb_0, kConnectionOptions);
  conn_mgr()->Connect(peer_1->identifier(), conn_cb_1, kConnectionOptions);

  // Wait for scanning to start & OnScanStart callback to be called.
  RunLoopUntilIdle();
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer_0->le()->connection_state());
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer_1->le()->connection_state());

  // After peer_0's connection request is cancelled, peer_1's connection request should succeed.
  EXPECT_TRUE(conn_mgr()->Disconnect(peer_0->identifier()));
  RunLoopUntilIdle();
  EXPECT_EQ(conn_cb_0_count, 1);
  ASSERT_TRUE(conn_handle);
  EXPECT_EQ(conn_handle->peer_identifier(), peer_1->identifier());
  EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer_0->le()->connection_state());
  EXPECT_EQ(Peer::ConnectionState::kConnected, peer_1->le()->connection_state());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, LocalDisconnectWhileConnectorPending) {
  auto peer_0 = peer_cache()->NewPeer(kAddress0, true);
  auto fake_peer_0 = std::make_unique<FakePeer>(kAddress0);
  fake_peer_0->set_force_pending_connect(true);
  test_device()->AddPeer(std::move(fake_peer_0));

  auto peer_1 = peer_cache()->NewPeer(kAddress1, true);
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress1));

  int conn_cb_0_count = 0;
  auto conn_cb_0 = [&](auto result) {
    EXPECT_TRUE(result.is_error());
    EXPECT_EQ(result.error(), HostError::kCanceled);
    conn_cb_0_count++;
  };

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto conn_cb_1 = [&](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
  };

  conn_mgr()->Connect(peer_0->identifier(), conn_cb_0, kConnectionOptions);
  conn_mgr()->Connect(peer_1->identifier(), conn_cb_1, kConnectionOptions);
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer_0->le()->connection_state());
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer_1->le()->connection_state());

  // Wait for peer_0 scanning to complete and kLECreateConnection command to be sent.
  RunLoopUntilIdle();

  // After peer_0's connection request is cancelled, peer_1's connection request should succeed.
  EXPECT_TRUE(conn_mgr()->Disconnect(peer_0->identifier()));
  RunLoopUntilIdle();
  EXPECT_EQ(conn_cb_0_count, 1);
  ASSERT_TRUE(conn_handle);
  EXPECT_EQ(conn_handle->peer_identifier(), peer_1->identifier());
  EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer_0->le()->connection_state());
  EXPECT_EQ(Peer::ConnectionState::kConnected, peer_1->le()->connection_state());
}

TEST_F(GAP_LowEnergyConnectionManagerTest,
       DisconnectQueuedPendingConnectionAndThenPendingConnectionWithPendingConnector) {
  auto peer_0 = peer_cache()->NewPeer(kAddress0, true);
  auto fake_peer_0 = std::make_unique<FakePeer>(kAddress0);
  fake_peer_0->set_force_pending_connect(true);
  test_device()->AddPeer(std::move(fake_peer_0));

  auto peer_1 = peer_cache()->NewPeer(kAddress1, true);
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress1));

  int conn_cb_0_count = 0;
  auto conn_cb_0 = [&](auto result) {
    ASSERT_TRUE(result.is_error());
    EXPECT_EQ(result.error(), HostError::kCanceled);
    conn_cb_0_count++;
  };

  int conn_cb_1_count = 0;
  auto conn_cb_1 = [&](auto result) {
    ASSERT_TRUE(result.is_error());
    EXPECT_EQ(result.error(), HostError::kCanceled);
    conn_cb_1_count++;
  };

  conn_mgr()->Connect(peer_0->identifier(), conn_cb_0, kConnectionOptions);
  conn_mgr()->Connect(peer_1->identifier(), conn_cb_1, kConnectionOptions);
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer_0->le()->connection_state());
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer_1->le()->connection_state());

  EXPECT_TRUE(conn_mgr()->Disconnect(peer_1->identifier()));
  RunLoopUntilIdle();
  EXPECT_EQ(conn_cb_0_count, 0);
  EXPECT_EQ(conn_cb_1_count, 1);
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer_0->le()->connection_state());
  EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer_1->le()->connection_state());

  EXPECT_TRUE(conn_mgr()->Disconnect(peer_0->identifier()));
  RunLoopUntilIdle();
  EXPECT_EQ(conn_cb_0_count, 1);
  EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer_0->le()->connection_state());
  EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer_1->le()->connection_state());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, DisconnectUnknownPeer) {
  // Unknown peers are inherently "not connected."
  EXPECT_TRUE(conn_mgr()->Disconnect(PeerId(999)));
}

TEST_F(GAP_LowEnergyConnectionManagerTest, DisconnectUnconnectedPeer) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));

  // This returns true so long the peer is not connected.
  EXPECT_TRUE(conn_mgr()->Disconnect(peer->identifier()));
}

TEST_F(GAP_LowEnergyConnectionManagerTest, Disconnect) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));

  int closed_count = 0;
  auto closed_cb = [&closed_count] { closed_count++; };

  std::vector<std::unique_ptr<LowEnergyConnectionHandle>> conn_handles;
  auto success_cb = [&conn_handles, &closed_cb](auto result) {
    ASSERT_TRUE(result.is_ok());
    auto conn_handle = result.take_value();
    conn_handle->set_closed_callback(closed_cb);
    conn_handles.push_back(std::move(conn_handle));
  };

  // Issue two connection refs.
  conn_mgr()->Connect(peer->identifier(), success_cb, kConnectionOptions);
  conn_mgr()->Connect(peer->identifier(), success_cb, kConnectionOptions);

  RunLoopUntilIdle();

  ASSERT_EQ(2u, conn_handles.size());

  EXPECT_TRUE(conn_mgr()->Disconnect(peer->identifier()));

  bool peer_removed = peer_cache()->RemoveDisconnectedPeer(peer->identifier());
  EXPECT_TRUE(peer_removed);

  RunLoopUntilIdle();

  EXPECT_EQ(2, closed_count);
  EXPECT_TRUE(connected_peers().empty());
  EXPECT_TRUE(canceled_peers().empty());

  // The central pause timeout handler should not run.
  RunLoopFor(kLEConnectionPauseCentral);
}

TEST_F(GAP_LowEnergyConnectionManagerTest, IntentionalDisconnectDisablesAutoConnectBehavior) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));

  std::vector<std::unique_ptr<LowEnergyConnectionHandle>> conn_handles;
  auto success_cb = [&conn_handles](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handles.push_back(result.take_value());
  };

  sm::PairingData data;
  data.peer_ltk = sm::LTK();
  data.local_ltk = sm::LTK();
  data.irk = sm::Key(sm::SecurityProperties(), Random<UInt128>());
  EXPECT_TRUE(peer_cache()->StoreLowEnergyBond(peer->identifier(), data));

  // Issue connection ref.
  conn_mgr()->Connect(peer->identifier(), success_cb, kConnectionOptions);
  RunLoopUntilIdle();

  // Bonded peer should have auto-connection enabled.
  EXPECT_TRUE(peer->le()->should_auto_connect());

  // Explicit disconnect should disable the auto-connection property.
  EXPECT_TRUE(conn_mgr()->Disconnect(peer->identifier()));
  RunLoopUntilIdle();
  EXPECT_FALSE(peer->le()->should_auto_connect());

  // Intentional re-connection should re-enable the auto-connection property.
  conn_mgr()->Connect(peer->identifier(), success_cb, kConnectionOptions);
  RunLoopUntilIdle();
  EXPECT_TRUE(peer->le()->should_auto_connect());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, IncidentalDisconnectDoesNotAffectAutoConnectBehavior) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));

  std::vector<std::unique_ptr<LowEnergyConnectionHandle>> conn_handles;
  auto success_cb = [&conn_handles](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handles.push_back(result.take_value());
  };

  sm::PairingData data;
  data.peer_ltk = sm::LTK();
  data.local_ltk = sm::LTK();
  data.irk = sm::Key(sm::SecurityProperties(), Random<UInt128>());
  EXPECT_TRUE(peer_cache()->StoreLowEnergyBond(peer->identifier(), data));

  // Issue connection ref.
  conn_mgr()->Connect(peer->identifier(), success_cb, kConnectionOptions);
  RunLoopUntilIdle();

  // Bonded peer should have auto-connection enabled.
  EXPECT_TRUE(peer->le()->should_auto_connect());

  // Incidental disconnect should NOT disable the auto-connection property.
  ASSERT_TRUE(conn_handles.size());
  conn_handles[0] = nullptr;
  RunLoopUntilIdle();
  EXPECT_TRUE(peer->le()->should_auto_connect());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, DisconnectThrice) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));

  int closed_count = 0;
  auto closed_cb = [&closed_count] { closed_count++; };

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto success_cb = [&closed_cb, &conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
    ASSERT_TRUE(conn_handle);
    conn_handle->set_closed_callback(closed_cb);
  };

  conn_mgr()->Connect(peer->identifier(), success_cb, kConnectionOptions);

  RunLoopUntilIdle();

  EXPECT_TRUE(conn_mgr()->Disconnect(peer->identifier()));

  // Try to disconnect again while the first disconnection is in progress.
  EXPECT_TRUE(conn_mgr()->Disconnect(peer->identifier()));

  RunLoopUntilIdle();

  // The single ref should get only one "closed" call.
  EXPECT_EQ(1, closed_count);
  EXPECT_TRUE(connected_peers().empty());
  EXPECT_TRUE(canceled_peers().empty());

  // Try to disconnect once more, now that the link is gone.
  EXPECT_TRUE(conn_mgr()->Disconnect(peer->identifier()));
}

// Tests when a link is lost without explicitly disconnecting
TEST_F(GAP_LowEnergyConnectionManagerTest, DisconnectEvent) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);

  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));

  int closed_count = 0;
  auto closed_cb = [&closed_count] { closed_count++; };

  std::vector<std::unique_ptr<LowEnergyConnectionHandle>> conn_handles;
  auto success_cb = [&conn_handles, &closed_cb](auto result) {
    ASSERT_TRUE(result.is_ok());
    auto conn_handle = result.take_value();
    conn_handle->set_closed_callback(closed_cb);
    conn_handles.push_back(std::move(conn_handle));
  };

  // Issue two connection refs.
  conn_mgr()->Connect(peer->identifier(), success_cb, kConnectionOptions);
  conn_mgr()->Connect(peer->identifier(), success_cb, kConnectionOptions);

  RunLoopUntilIdle();

  ASSERT_EQ(2u, conn_handles.size());

  // This makes FakeController send us HCI Disconnection Complete events.
  test_device()->Disconnect(kAddress0);

  RunLoopUntilIdle();

  EXPECT_EQ(2, closed_count);
}

TEST_F(GAP_LowEnergyConnectionManagerTest, DisconnectAfterRefsReleased) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto success_cb = [&conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
  };

  conn_mgr()->Connect(peer->identifier(), success_cb, kConnectionOptions);

  RunLoopUntilIdle();

  ASSERT_TRUE(conn_handle);
  conn_handle.reset();

  // Try to disconnect while the zero-refs connection is being disconnected.
  EXPECT_TRUE(conn_mgr()->Disconnect(peer->identifier()));

  RunLoopUntilIdle();

  EXPECT_TRUE(connected_peers().empty());
  EXPECT_TRUE(canceled_peers().empty());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, DisconnectAfterSecondConnectionRequestInvalidatesRefs) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle_0;
  auto success_cb = [&conn_handle_0](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle_0 = result.take_value();
    ASSERT_TRUE(conn_handle_0);
    EXPECT_TRUE(conn_handle_0->active());
  };

  conn_mgr()->Connect(peer->identifier(), success_cb, kConnectionOptions);
  RunLoopUntilIdle();
  ASSERT_TRUE(conn_handle_0);
  EXPECT_TRUE(conn_handle_0->active());

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle_1;
  auto ref_cb = [&conn_handle_1](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle_1 = result.take_value();
  };

  // Callback should be run synchronously with success status because connection already exists.
  conn_mgr()->Connect(peer->identifier(), ref_cb, kConnectionOptions);
  EXPECT_TRUE(conn_handle_1);
  EXPECT_TRUE(conn_handle_1->active());

  // This should invalidate the refs.
  EXPECT_TRUE(conn_mgr()->Disconnect(peer->identifier()));
  EXPECT_FALSE(conn_handle_1->active());
  EXPECT_FALSE(conn_handle_0->active());

  RunLoopUntilIdle();
}

// This tests that a connection reference callback succeeds if a HCI
// Disconnection Complete event is received for the corresponding ACL link
// immediately after the callback gets run.
TEST_F(GAP_LowEnergyConnectionManagerTest, DisconnectCompleteEventAfterConnect) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto success_cb = [&conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
    EXPECT_TRUE(conn_handle->active());
  };

  conn_mgr()->Connect(peer->identifier(), success_cb, kConnectionOptions);
  RunLoopUntilIdle();
  ASSERT_TRUE(conn_handle);

  // Request a new reference. Disconnect the link before the reference is
  // received.
  size_t ref_cb_count = 0;
  auto ref_cb = [&ref_cb_count](auto result) {
    ref_cb_count++;
    EXPECT_TRUE(result.is_ok());
  };

  size_t disconn_cb_count = 0;
  auto disconn_cb = [this, ref_cb, peer, &disconn_cb_count, &ref_cb_count](auto) {
    disconn_cb_count++;
    // The link is gone but conn_mgr() hasn't updated the connection state yet.
    // The request to connect will attempt to add a new reference which will succeed because ref_cb
    // is called synchronously.
    EXPECT_EQ(0u, ref_cb_count);
    conn_mgr()->Connect(peer->identifier(), ref_cb, kConnectionOptions);
    EXPECT_EQ(1u, ref_cb_count);
  };
  conn_mgr()->SetDisconnectCallbackForTesting(disconn_cb);

  test_device()->SendDisconnectionCompleteEvent(conn_handle->handle());

  RunLoopUntilIdle();

  EXPECT_EQ(1u, ref_cb_count);
  EXPECT_EQ(1u, disconn_cb_count);
}

TEST_F(GAP_LowEnergyConnectionManagerTest, RemovePeerFromPeerCacheDuringDisconnection) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto success_cb = [&conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
    EXPECT_TRUE(conn_handle->active());
  };

  conn_mgr()->Connect(peer->identifier(), success_cb, kConnectionOptions);
  RunLoopUntilIdle();
  ASSERT_TRUE(conn_handle);

  // This should invalidate the ref that was bound to |ref_cb|.
  const PeerId id = peer->identifier();
  EXPECT_TRUE(conn_mgr()->Disconnect(id));
  ASSERT_FALSE(peer->le()->connected());
  EXPECT_FALSE(conn_handle->active());

  EXPECT_TRUE(peer_cache()->RemoveDisconnectedPeer(id));

  RunLoopUntilIdle();

  EXPECT_FALSE(peer_cache()->FindById(id));
  EXPECT_FALSE(peer_cache()->FindByAddress(kAddress0));
}

// Listener receives remote initiated connection ref.
TEST_F(GAP_LowEnergyConnectionManagerTest, RegisterRemoteInitiatedLink) {
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));

  // First create a fake incoming connection.
  test_device()->ConnectLowEnergy(kAddress0);

  RunLoopUntilIdle();

  auto link = MoveLastRemoteInitiated();
  ASSERT_TRUE(link);

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  conn_mgr()->RegisterRemoteInitiatedLink(std::move(link), BondableMode::Bondable,
                                          [&](auto result) {
                                            ASSERT_TRUE(result.is_ok());
                                            conn_handle = result.take_value();
                                          });
  // A Peer should now exist in the cache.
  auto* peer = peer_cache()->FindByAddress(kAddress0);
  EXPECT_EQ(peer->le()->connection_state(), Peer::ConnectionState::kInitializing);

  RunLoopUntilIdle();

  ASSERT_TRUE(conn_handle);
  EXPECT_TRUE(conn_handle->active());
  ASSERT_TRUE(peer);
  EXPECT_EQ(peer->identifier(), conn_handle->peer_identifier());
  EXPECT_TRUE(peer->connected());
  EXPECT_TRUE(peer->le()->connected());
  EXPECT_TRUE(peer->version().has_value());
  EXPECT_TRUE(peer->le()->features().has_value());

  conn_handle = nullptr;

  RunLoopUntilIdle();
  EXPECT_TRUE(connected_peers().empty());
}

TEST_F(GAP_LowEnergyConnectionManagerTest,
       RegisterRemoteInitiatedLinkDuringLocalInitiatedLinkConnecting) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  fake_peer->set_force_pending_connect(true);
  test_device()->AddPeer(std::move(fake_peer));

  // Create a fake incoming connection.
  test_device()->ConnectLowEnergy(kAddress0);
  RunLoopUntilIdle();
  auto link = MoveLastRemoteInitiated();
  ASSERT_TRUE(link);

  // Create a pending outgoing connection.
  ConnectionResult result;
  auto callback = [&result](auto res) { result = std::move(res); };
  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  conn_mgr()->RegisterRemoteInitiatedLink(std::move(link), BondableMode::Bondable,
                                          [&](auto result) {
                                            ASSERT_TRUE(result.is_ok());
                                            conn_handle = result.take_value();
                                          });
  RunLoopUntilIdle();
  ASSERT_TRUE(conn_handle);

  // Local connector result handler should not crash when it finds that connection to peer already
  // exists.
  RunLoopFor(kLECreateConnectionTimeout);
  // An error should be returned if the connection complete was incorrectly not matched to the
  // pending connection request (see fxbug.dev/68969).
  // In the future it may make sense to return success because a link to the peer already exists.
  ASSERT_TRUE(result.is_error());
  EXPECT_TRUE(peer->le()->connected());
}

TEST_F(GAP_LowEnergyConnectionManagerTest,
       RegisterRemoteInitiatedLinkDuringLocalInitiatedConnectionScanning) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  fake_peer->set_advertising_enabled(false);
  test_device()->AddPeer(std::move(fake_peer));

  // Create a fake incoming connection.
  test_device()->ConnectLowEnergy(kAddress0);
  RunLoopUntilIdle();
  auto link = MoveLastRemoteInitiated();
  ASSERT_TRUE(link);

  // Create a pending outgoing connection.
  ConnectionResult result;
  auto callback = [&result](auto res) { result = std::move(res); };
  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  conn_mgr()->RegisterRemoteInitiatedLink(std::move(link), BondableMode::Bondable,
                                          [&](auto result) {
                                            ASSERT_TRUE(result.is_ok());
                                            conn_handle = result.take_value();
                                          });
  RunLoopUntilIdle();
  ASSERT_TRUE(conn_handle);

  // Local connector result handler should not crash when it finds that connection to peer already
  // exists.
  RunLoopFor(kLEGeneralCepScanTimeout);
  ASSERT_TRUE(result.is_error());
  EXPECT_TRUE(peer->le()->connected());
}

// Listener receives remote initiated connection ref for a known peer with the
// same BR/EDR address.
TEST_F(GAP_LowEnergyConnectionManagerTest, IncomingConnectionUpgradesKnownBrEdrPeerToDualMode) {
  Peer* peer = peer_cache()->NewPeer(kAddrAlias0, true);
  ASSERT_TRUE(peer);
  ASSERT_EQ(peer, peer_cache()->FindByAddress(kAddress0));
  ASSERT_EQ(TechnologyType::kClassic, peer->technology());

  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));

  // First create a fake incoming connection.
  test_device()->ConnectLowEnergy(kAddress0);

  RunLoopUntilIdle();

  auto link = MoveLastRemoteInitiated();
  ASSERT_TRUE(link);

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  conn_mgr()->RegisterRemoteInitiatedLink(std::move(link), BondableMode::Bondable,
                                          [&conn_handle](auto result) {
                                            ASSERT_TRUE(result.is_ok());
                                            conn_handle = result.take_value();
                                          });
  RunLoopUntilIdle();
  ASSERT_TRUE(conn_handle);

  EXPECT_EQ(peer->identifier(), conn_handle->peer_identifier());
  EXPECT_EQ(TechnologyType::kDualMode, peer->technology());
}

// Successful connection to a peer whose address type is kBREDR.
// TODO(fxbug.dev/2761): This test will likely become obsolete when LE connections are based on the
// presence of LowEnergyData in a Peer and no address type enum exists.
TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectAndDisconnectDualModeDeviceWithBrEdrAddress) {
  Peer* peer = peer_cache()->NewPeer(kAddrAlias0, true);
  ASSERT_TRUE(peer);
  ASSERT_TRUE(peer->bredr());

  peer->MutLe();
  ASSERT_EQ(TechnologyType::kDualMode, peer->technology());
  ASSERT_EQ(peer, peer_cache()->FindByAddress(kAddress0));
  ASSERT_EQ(DeviceAddress::Type::kBREDR, peer->address().type());

  // Only the LE transport connects in this test, so only add an LE FakePeer to FakeController.
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto callback = [&conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
  };

  EXPECT_TRUE(connected_peers().empty());
  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());

  RunLoopUntilIdle();

  EXPECT_EQ(1u, connected_peers().size());
  EXPECT_EQ(1u, connected_peers().count(kAddress0));

  ASSERT_TRUE(conn_handle);
  EXPECT_TRUE(conn_handle->active());
  EXPECT_EQ(peer->identifier(), conn_handle->peer_identifier());
  EXPECT_FALSE(peer->temporary());
  EXPECT_EQ(Peer::ConnectionState::kConnected, peer->le()->connection_state());

  conn_handle = nullptr;
  RunLoopUntilIdle();
  EXPECT_EQ(0u, connected_peers().size());
}

// Tests that the central accepts the connection parameters that are sent from
// a fake peripheral and eventually applies them to the link.
TEST_F(GAP_LowEnergyConnectionManagerTest,
       CentralAppliesL2capConnectionParameterUpdateRequestParams) {
  // Set up a fake peer and a connection over which to process the L2CAP
  // request.
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  ASSERT_TRUE(peer);

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto conn_cb = [&conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
  };
  conn_mgr()->Connect(peer->identifier(), conn_cb, kConnectionOptions);

  RunLoopUntilIdle();
  ASSERT_TRUE(conn_handle);

  hci::LEPreferredConnectionParameters preferred(
      hci::kLEConnectionIntervalMin, hci::kLEConnectionIntervalMax, hci::kLEConnectionLatencyMax,
      hci::kLEConnectionSupervisionTimeoutMax);

  std::optional<hci::LEConnectionParameters> actual;

  auto conn_params_updated_cb = [&](const auto& addr, const auto& params) { actual = params; };
  test_device()->set_le_connection_parameters_callback(conn_params_updated_cb);

  fake_l2cap()->TriggerLEConnectionParameterUpdate(conn_handle->handle(), preferred);

  // These connection update events for the wrong handle should be ignored.
  // Send twice: once before the parameter request is processed, and once after the request has been
  // processed.
  hci::LEConnectionParameters wrong_handle_conn_params(0, 1, 2);
  test_device()->SendLEConnectionUpdateCompleteSubevent(conn_handle->handle() + 1,
                                                        wrong_handle_conn_params);
  RunLoopUntilIdle();

  test_device()->SendLEConnectionUpdateCompleteSubevent(conn_handle->handle() + 1,
                                                        wrong_handle_conn_params);

  RunLoopUntilIdle();

  ASSERT_TRUE(actual.has_value());
  ASSERT_TRUE(peer->le());
  EXPECT_EQ(preferred, *peer->le()->preferred_connection_parameters());
  EXPECT_EQ(actual.value(), *peer->le()->connection_parameters());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, L2CAPSignalLinkError) {
  // Set up a fake peer and a connection over which to process the L2CAP
  // request.
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  ASSERT_TRUE(peer);

  fbl::RefPtr<l2cap::Channel> att_chan;
  auto l2cap_chan_cb = [&att_chan](auto chan) { att_chan = chan; };
  fake_l2cap()->set_channel_callback(l2cap_chan_cb);

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto conn_cb = [&conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
  };
  conn_mgr()->Connect(peer->identifier(), conn_cb, kConnectionOptions);

  RunLoopUntilIdle();
  ASSERT_TRUE(conn_handle);
  ASSERT_TRUE(att_chan);
  ASSERT_EQ(1u, connected_peers().size());

  // Signaling a link error through the channel should disconnect the link.
  att_chan->SignalLinkError();

  RunLoopUntilIdle();
  EXPECT_TRUE(connected_peers().empty());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, PairUnconnectedPeer) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  EXPECT_TRUE(peer->temporary());
  ASSERT_EQ(peer_cache()->count(), 1u);
  uint count_cb_called = 0;
  auto cb = [&count_cb_called](sm::Status status) {
    ASSERT_EQ(status.error(), bt::HostError::kNotFound);
    count_cb_called++;
  };
  conn_mgr()->Pair(peer->identifier(), sm::SecurityLevel::kEncrypted, sm::BondableMode::Bondable,
                   cb);
  ASSERT_EQ(count_cb_called, 1u);
}

TEST_F(GAP_LowEnergyConnectionManagerTest, PairWithBondableModes) {
  // clang-format on
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  EXPECT_TRUE(peer->temporary());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto callback = [&conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
    EXPECT_TRUE(conn_handle);
    EXPECT_TRUE(conn_handle->active());
  };

  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  ASSERT_TRUE(peer->le());

  RunLoopUntilIdle();
  fxl::WeakPtr<TestSm> mock_sm = TestSmByHandle(conn_handle->handle());
  ASSERT_TRUE(mock_sm);

  ASSERT_EQ(Peer::ConnectionState::kConnected, peer->le()->connection_state());

  EXPECT_FALSE(mock_sm->last_requested_upgrade().has_value());
  conn_mgr()->Pair(peer->identifier(), sm::SecurityLevel::kEncrypted, sm::BondableMode::Bondable,
                   [](sm::Status cb_status) {});
  RunLoopUntilIdle();

  EXPECT_EQ(BondableMode::Bondable, mock_sm->bondable_mode());
  EXPECT_EQ(sm::SecurityLevel::kEncrypted, mock_sm->last_requested_upgrade());

  conn_mgr()->Pair(peer->identifier(), sm::SecurityLevel::kAuthenticated,
                   sm::BondableMode::NonBondable, [](sm::Status cb_status) {});
  RunLoopUntilIdle();

  EXPECT_EQ(BondableMode::NonBondable, mock_sm->bondable_mode());
  EXPECT_EQ(sm::SecurityLevel::kAuthenticated, mock_sm->last_requested_upgrade());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectAndDiscoverByServiceWithoutUUID) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);

  bool cb_called = false;
  auto expect_uuids = [&cb_called](PeerId peer_id, auto uuids) {
    ASSERT_TRUE(uuids.empty());
    cb_called = true;
  };
  fake_gatt()->SetDiscoverServicesCallback(expect_uuids);

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto callback = [&conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
    EXPECT_TRUE(conn_handle);
    EXPECT_TRUE(conn_handle->active());
  };

  LowEnergyConnectionOptions connection_options{.service_uuid = std::nullopt};
  conn_mgr()->Connect(peer->identifier(), callback, connection_options);

  RunLoopUntilIdle();

  ASSERT_TRUE(cb_called);
}

TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectAndDiscoverByServiceUuid) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);

  UUID kConnectUuid({0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15});
  std::array<UUID, 2> expected_uuids = {kConnectUuid, kGenericAccessService};

  bool cb_called = false;
  auto expect_uuid = [&cb_called, expected_uuids](PeerId peer_id, auto uuids) {
    EXPECT_THAT(uuids, ::testing::UnorderedElementsAreArray(expected_uuids));
    cb_called = true;
  };
  fake_gatt()->SetDiscoverServicesCallback(expect_uuid);

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto callback = [&conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
    ASSERT_TRUE(conn_handle);
    EXPECT_TRUE(conn_handle->active());
  };

  LowEnergyConnectionOptions connection_options{.service_uuid = std::optional(kConnectUuid)};
  conn_mgr()->Connect(peer->identifier(), callback, connection_options);

  RunLoopUntilIdle();

  ASSERT_TRUE(cb_called);
}

TEST_F(GAP_LowEnergyConnectionManagerTest,
       ReadPeripheralPreferredConnectionParametersCharacteristicAndUpdateConnectionParameters) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  // Set up GAP service
  gatt::ServiceData service_data(gatt::ServiceKind::PRIMARY, /*start=*/0x0001, /*end=*/0x0009,
                                 kGenericAccessService);
  auto [remote_svc, service_client] = fake_gatt()->AddPeerService(peer->identifier(), service_data);

  // Set up preferred connection parameters characteristic.
  att::Handle char_handle = 0x0002;
  att::Handle char_value_handle = 0x0003;
  gatt::CharacteristicData char_data(gatt::kRead, /*ext_props=*/std::nullopt, char_handle,
                                     char_value_handle,
                                     kPeripheralPreferredConnectionParametersCharacteristic);
  service_client->set_characteristics({char_data});
  StaticByteBuffer char_value(0x01, 0x00,   // min interval
                              0x02, 0x00,   // max interval
                              0x03, 0x00,   // max latency
                              0x04, 0x00);  // supervision timeout
  service_client->set_read_request_callback(
      [char_value_handle, char_value](att::Handle handle, auto read_cb) {
        if (handle == char_value_handle) {
          read_cb(att::Status(), char_value);
        }
      });

  std::unique_ptr<LowEnergyConnectionHandle> conn_ref;
  auto callback = [&conn_ref](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_ref = result.take_value();
  };

  conn_mgr()->Connect(peer->identifier(), callback, LowEnergyConnectionOptions());

  RunLoopUntilIdle();
  EXPECT_TRUE(conn_ref);
  ASSERT_TRUE(peer->le()->preferred_connection_parameters());
  auto params = peer->le()->preferred_connection_parameters().value();
  EXPECT_EQ(params.min_interval(), 1u);
  EXPECT_EQ(params.max_interval(), 2u);
  EXPECT_EQ(params.max_latency(), 3u);
  EXPECT_EQ(params.supervision_timeout(), 4u);

  std::optional<hci::LEConnectionParameters> conn_params;
  test_device()->set_le_connection_parameters_callback(
      [&](auto address, auto parameters) { conn_params = parameters; });

  RunLoopFor(kLEConnectionPauseCentral);
  ASSERT_TRUE(conn_params.has_value());
  EXPECT_EQ(conn_params->interval(), 1u);  // FakeController will use min interval
  EXPECT_EQ(conn_params->latency(), 3u);
  EXPECT_EQ(conn_params->supervision_timeout(), 4u);
}

TEST_F(GAP_LowEnergyConnectionManagerTest,
       ReadPeripheralPreferredConnectionParametersCharacteristicInvalidValueSize) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  // Set up GAP service
  gatt::ServiceData service_data(gatt::ServiceKind::PRIMARY, /*start=*/0x0001, /*end=*/0x0003,
                                 kGenericAccessService);
  auto [remote_svc, service_client] = fake_gatt()->AddPeerService(peer->identifier(), service_data);

  // Set up preferred connection parameters characteristic.
  att::Handle char_handle = 0x0002;
  att::Handle char_value_handle = 0x0003;
  gatt::CharacteristicData char_data(gatt::kRead, /*ext_props=*/std::nullopt, char_handle,
                                     char_value_handle,
                                     kPeripheralPreferredConnectionParametersCharacteristic);
  service_client->set_characteristics({char_data});
  StaticByteBuffer invalid_char_value(0x01);  // too small
  service_client->set_read_request_callback(
      [char_value_handle, invalid_char_value](auto handle, auto read_cb) {
        if (handle == char_value_handle) {
          read_cb(att::Status(), invalid_char_value);
        }
      });

  std::unique_ptr<LowEnergyConnectionHandle> conn_ref;
  auto callback = [&conn_ref](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_ref = result.take_value();
  };

  conn_mgr()->Connect(peer->identifier(), callback, LowEnergyConnectionOptions());

  RunLoopUntilIdle();
  EXPECT_TRUE(conn_ref);
  EXPECT_FALSE(peer->le()->preferred_connection_parameters());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, GapServiceCharacteristicDiscoveryError) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  // Set up GAP service
  gatt::ServiceData service_data(gatt::ServiceKind::PRIMARY, /*start=*/0x0001, /*end=*/0x0003,
                                 kGenericAccessService);
  auto [remote_svc, service_client] = fake_gatt()->AddPeerService(peer->identifier(), service_data);

  // Set up preferred connection parameters characteristic.
  att::Handle char_handle = 0x0002;
  att::Handle char_value_handle = 0x0003;
  gatt::CharacteristicData char_data(gatt::kRead, /*ext_props=*/std::nullopt, char_handle,
                                     char_value_handle,
                                     kPeripheralPreferredConnectionParametersCharacteristic);
  service_client->set_characteristic_discovery_status(
      att::Status(att::ErrorCode::kReadNotPermitted));

  std::unique_ptr<LowEnergyConnectionHandle> conn_ref;
  auto callback = [&conn_ref](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_ref = result.take_value();
  };

  conn_mgr()->Connect(peer->identifier(), callback, LowEnergyConnectionOptions());

  RunLoopUntilIdle();
  EXPECT_TRUE(conn_ref);
  EXPECT_FALSE(peer->le()->preferred_connection_parameters());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, GapServiceListServicesError) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  fake_gatt()->set_list_services_status(att::Status(HostError::kFailed));

  std::unique_ptr<LowEnergyConnectionHandle> conn_ref;
  auto callback = [&conn_ref](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_ref = result.take_value();
  };

  conn_mgr()->Connect(peer->identifier(), callback, LowEnergyConnectionOptions());

  RunLoopUntilIdle();
  EXPECT_TRUE(conn_ref);
  EXPECT_FALSE(peer->le()->preferred_connection_parameters());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, PeerGapServiceMissingConnectionParameterCharacteristic) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  // Set up GAP service
  gatt::ServiceData service_data(gatt::ServiceKind::PRIMARY, /*start=*/0x0001, /*end=*/0x0003,
                                 kGenericAccessService);
  auto [remote_svc, service_client] = fake_gatt()->AddPeerService(peer->identifier(), service_data);

  std::unique_ptr<LowEnergyConnectionHandle> conn_ref;
  auto callback = [&conn_ref](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_ref = result.take_value();
  };

  conn_mgr()->Connect(peer->identifier(), callback, LowEnergyConnectionOptions());

  RunLoopUntilIdle();
  EXPECT_TRUE(conn_ref);
  EXPECT_FALSE(peer->le()->preferred_connection_parameters());
}

// Listener receives remote initiated connection ref.
TEST_F(GAP_LowEnergyConnectionManagerTest, PassBondableThroughRemoteInitiatedLink) {
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));

  // First create a fake incoming connection.
  test_device()->ConnectLowEnergy(kAddress0);

  RunLoopUntilIdle();

  auto link = MoveLastRemoteInitiated();
  ASSERT_TRUE(link);

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  conn_mgr()->RegisterRemoteInitiatedLink(std::move(link), BondableMode::Bondable,
                                          [&conn_handle](auto result) {
                                            ASSERT_TRUE(result.is_ok());
                                            conn_handle = result.take_value();
                                          });
  RunLoopUntilIdle();

  ASSERT_TRUE(conn_handle);
  EXPECT_TRUE(conn_handle->active());
  EXPECT_EQ(conn_handle->bondable_mode(), BondableMode::Bondable);
}

TEST_F(GAP_LowEnergyConnectionManagerTest, PassNonBondableThroughRemoteInitiatedLink) {
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));

  // First create a fake incoming connection.
  test_device()->ConnectLowEnergy(kAddress0);

  RunLoopUntilIdle();

  auto link = MoveLastRemoteInitiated();
  ASSERT_TRUE(link);

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  conn_mgr()->RegisterRemoteInitiatedLink(std::move(link), BondableMode::NonBondable,
                                          [&conn_handle](auto result) {
                                            ASSERT_TRUE(result.is_ok());
                                            conn_handle = result.take_value();
                                          });
  RunLoopUntilIdle();

  ASSERT_TRUE(conn_handle);
  EXPECT_TRUE(conn_handle->active());
  EXPECT_EQ(conn_handle->bondable_mode(), BondableMode::NonBondable);
}

// Successful connection to single peer
TEST_F(GAP_LowEnergyConnectionManagerTest, PassBondableThroughConnect) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  EXPECT_TRUE(peer->temporary());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto callback = [&conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
    ASSERT_TRUE(conn_handle);
    EXPECT_TRUE(conn_handle->active());
  };

  EXPECT_TRUE(connected_peers().empty());
  conn_mgr()->Connect(peer->identifier(), callback, {.bondable_mode = BondableMode::Bondable});

  RunLoopUntilIdle();

  ASSERT_TRUE(conn_handle);
  EXPECT_EQ(conn_handle->bondable_mode(), BondableMode::Bondable);
}

// Successful connection to single peer
TEST_F(GAP_LowEnergyConnectionManagerTest, PassNonBondableThroughConnect) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  EXPECT_TRUE(peer->temporary());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto callback = [&conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
    ASSERT_TRUE(conn_handle);
    EXPECT_TRUE(conn_handle->active());
  };

  EXPECT_TRUE(connected_peers().empty());
  conn_mgr()->Connect(peer->identifier(), callback, {.bondable_mode = BondableMode::NonBondable});

  RunLoopUntilIdle();

  ASSERT_TRUE(conn_handle);
  EXPECT_EQ(conn_handle->bondable_mode(), BondableMode::NonBondable);
}

// Tests that the connection manager cleans up its connection map correctly following a
// disconnection due to encryption failure.
TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectionCleanUpFollowingEncryptionFailure) {
  // Set up a connection.
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  EXPECT_TRUE(peer->temporary());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  std::unique_ptr<LowEnergyConnectionHandle> conn;
  conn_mgr()->Connect(
      peer->identifier(),
      [&](auto result) {
        ASSERT_TRUE(result.is_ok());
        conn = result.take_value();
      },
      kConnectionOptions);
  RunLoopUntilIdle();
  ASSERT_TRUE(conn);

  hci::ConnectionHandle handle = conn->handle();
  bool ref_cleaned_up = false;
  bool disconnected = false;
  conn->set_closed_callback([&] { ref_cleaned_up = true; });
  conn_mgr()->SetDisconnectCallbackForTesting([&](hci::ConnectionHandle cb_handle) {
    EXPECT_EQ(handle, cb_handle);
    disconnected = true;
  });

  test_device()->SendEncryptionChangeEvent(handle, hci::StatusCode::kConnectionTerminatedMICFailure,
                                           hci::EncryptionStatus::kOff);
  test_device()->SendDisconnectionCompleteEvent(handle);
  RunLoopUntilIdle();

  EXPECT_TRUE(ref_cleaned_up);
  EXPECT_TRUE(disconnected);
}

TEST_F(GAP_LowEnergyConnectionManagerTest, SuccessfulInterrogationSetsPeerVersionAndFeatures) {
  constexpr hci::LESupportedFeatures kLEFeatures{
      static_cast<uint64_t>(hci::LESupportedFeature::kConnectionParametersRequestProcedure)};

  // Set up a connection.
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  ASSERT_TRUE(peer->le());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  fake_peer->set_le_features(kLEFeatures);
  test_device()->AddPeer(std::move(fake_peer));

  std::unique_ptr<LowEnergyConnectionHandle> conn;
  conn_mgr()->Connect(
      peer->identifier(),
      [&](auto result) {
        ASSERT_TRUE(result.is_ok());
        conn = result.take_value();
      },
      kConnectionOptions);

  EXPECT_FALSE(peer->version().has_value());
  EXPECT_FALSE(peer->le()->features().has_value());
  RunLoopUntilIdle();
  EXPECT_TRUE(conn);
  EXPECT_TRUE(peer->version().has_value());
  EXPECT_TRUE(peer->le()->features().has_value());
  EXPECT_EQ(kLEFeatures.le_features, peer->le()->features()->le_features);
  EXPECT_FALSE(peer->temporary());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectInterrogationFailure) {
  // Set up a connection.
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  ASSERT_TRUE(peer->le());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  std::optional<HostError> error;
  conn_mgr()->Connect(
      peer->identifier(),
      [&](auto result) {
        ASSERT_TRUE(result.is_error());
        error = result.error();
      },
      kConnectionOptions);
  ASSERT_FALSE(peer->le()->features().has_value());

  // Remove fake peer so LE Read Remote Features command fails during interrogation.
  test_device()->set_le_read_remote_features_callback(
      [this]() { test_device()->RemovePeer(kAddress0); });

  RunLoopUntilIdle();
  ASSERT_TRUE(error.has_value());
  EXPECT_FALSE(peer->connected());
  EXPECT_FALSE(peer->le()->connected());
  EXPECT_FALSE(peer->temporary());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, RemoteInitiatedLinkInterrogationFailure) {
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));

  // First create a fake incoming connection.
  test_device()->ConnectLowEnergy(kAddress0);

  RunLoopUntilIdle();

  auto link = MoveLastRemoteInitiated();
  ASSERT_TRUE(link);

  std::optional<HostError> error;
  conn_mgr()->RegisterRemoteInitiatedLink(std::move(link), BondableMode::Bondable,
                                          [&](auto result) {
                                            ASSERT_TRUE(result.is_error());
                                            error = result.error();
                                          });

  // Remove fake peer so LE Read Remote Features command fails during interrogation.
  test_device()->set_le_read_remote_features_callback(
      [this]() { test_device()->RemovePeer(kAddress0); });

  RunLoopUntilIdle();
  ASSERT_TRUE(error.has_value());

  // A Peer should now exist in the cache.
  auto* peer = peer_cache()->FindByAddress(kAddress0);
  ASSERT_TRUE(peer);
  EXPECT_FALSE(peer->connected());
  EXPECT_FALSE(peer->le()->connected());
  EXPECT_FALSE(peer->temporary());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, L2capRequestConnParamUpdateAfterInterrogation) {
  const hci::LEPreferredConnectionParameters kConnParams(
      hci::defaults::kLEConnectionIntervalMin, hci::defaults::kLEConnectionIntervalMax,
      /*max_latency=*/0, hci::defaults::kLESupervisionTimeout);

  // Connection Parameter Update procedure NOT supported.
  constexpr hci::LESupportedFeatures kLEFeatures{0};
  auto peer = std::make_unique<FakePeer>(kAddress0);
  peer->set_le_features(kLEFeatures);
  test_device()->AddPeer(std::move(peer));

  // First create a fake incoming connection as peripheral.
  test_device()->ConnectLowEnergy(kAddress0, hci::ConnectionRole::kSlave);

  RunLoopUntilIdle();

  auto link = MoveLastRemoteInitiated();
  ASSERT_TRUE(link);

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  conn_mgr()->RegisterRemoteInitiatedLink(std::move(link), BondableMode::Bondable,
                                          [&](auto result) {
                                            ASSERT_TRUE(result.is_ok());
                                            conn_handle = result.take_value();
                                          });

  size_t l2cap_conn_param_update_count = 0;
  fake_l2cap()->set_connection_parameter_update_request_responder([&](auto handle, auto params) {
    EXPECT_EQ(kConnParams, params);
    l2cap_conn_param_update_count++;
    return true;
  });

  size_t hci_update_conn_param_count = 0;
  test_device()->set_le_connection_parameters_callback(
      [&](auto address, auto parameters) { hci_update_conn_param_count++; });

  RunLoopUntilIdle();
  ASSERT_TRUE(conn_handle);
  EXPECT_TRUE(conn_handle->active());
  EXPECT_EQ(0u, l2cap_conn_param_update_count);
  EXPECT_EQ(0u, hci_update_conn_param_count);

  RunLoopFor(kLEConnectionPausePeripheral);
  EXPECT_EQ(1u, l2cap_conn_param_update_count);
  EXPECT_EQ(0u, hci_update_conn_param_count);
}

// Based on PTS L2CAP/LE/CPU/BV-01-C, in which the LE feature mask indicates support for the
// Connection Parameter Request Procedure, but sending the request results in a
// kUnsupportedRemoteFeature event status. PTS expects the host to retry with a L2cap connection
// parameter request.
//
// Test that this behavior is followed for 2 concurrent connections in order to ensure correct
// command/event handling.
TEST_F(GAP_LowEnergyConnectionManagerTest, PeripheralsRetryLLConnectionUpdateWithL2capRequest) {
  auto peer0 = std::make_unique<FakePeer>(kAddress0);
  auto peer1 = std::make_unique<FakePeer>(kAddress1);

  // Connection Parameter Update procedure supported by controller.
  constexpr hci::LESupportedFeatures kLEFeatures{
      static_cast<uint64_t>(hci::LESupportedFeature::kConnectionParametersRequestProcedure)};

  peer0->set_le_features(kLEFeatures);
  peer1->set_le_features(kLEFeatures);

  // Simulate host rejection by causing FakeController to set LE Connection Update Complete status
  // to kUnsupportedRemoteFeature, as PTS does.
  peer0->set_supports_ll_conn_update_procedure(false);
  peer1->set_supports_ll_conn_update_procedure(false);

  test_device()->AddPeer(std::move(peer0));
  test_device()->AddPeer(std::move(peer1));

  // First create fake incoming connections with local host as peripheral.
  test_device()->ConnectLowEnergy(kAddress0, hci::ConnectionRole::kSlave);
  RunLoopUntilIdle();
  auto link0 = MoveLastRemoteInitiated();
  ASSERT_TRUE(link0);

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle0;
  conn_mgr()->RegisterRemoteInitiatedLink(std::move(link0), BondableMode::Bondable,
                                          [&](auto result) {
                                            ASSERT_TRUE(result.is_ok());
                                            conn_handle0 = result.take_value();
                                          });

  test_device()->ConnectLowEnergy(kAddress1, hci::ConnectionRole::kSlave);
  RunLoopUntilIdle();
  auto link1 = MoveLastRemoteInitiated();
  ASSERT_TRUE(link1);

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle1;
  conn_mgr()->RegisterRemoteInitiatedLink(std::move(link1), BondableMode::Bondable,
                                          [&](auto result) {
                                            ASSERT_TRUE(result.is_ok());
                                            conn_handle1 = result.take_value();
                                          });

  size_t l2cap_conn_param_update_count = 0;
  size_t hci_update_conn_param_count = 0;

  fake_l2cap()->set_connection_parameter_update_request_responder([&](auto handle, auto params) {
    switch (l2cap_conn_param_update_count) {
      case 0:
        EXPECT_EQ(handle, conn_handle0->handle());
        break;
      case 1:
        EXPECT_EQ(handle, conn_handle1->handle());
        break;
      default:
        ADD_FAILURE();
    }

    // connection update commands should be sent before l2cap requests
    EXPECT_EQ(2u, hci_update_conn_param_count);

    l2cap_conn_param_update_count++;
    return true;
  });

  test_device()->set_le_connection_parameters_callback([&](auto address, auto params) {
    switch (hci_update_conn_param_count) {
      case 0:
        EXPECT_EQ(address, kAddress0);
        break;
      case 1:
        EXPECT_EQ(address, kAddress1);
        break;
      default:
        ADD_FAILURE();
    }

    // l2cap requests should not be sent until after failed HCI connection update commands
    EXPECT_EQ(0u, l2cap_conn_param_update_count);

    hci_update_conn_param_count++;
  });

  RunLoopFor(kLEConnectionPausePeripheral);
  ASSERT_TRUE(conn_handle0);
  EXPECT_TRUE(conn_handle0->active());
  ASSERT_TRUE(conn_handle1);
  EXPECT_TRUE(conn_handle1->active());

  EXPECT_EQ(2u, hci_update_conn_param_count);
  EXPECT_EQ(2u, l2cap_conn_param_update_count);

  // l2cap requests should not be sent on subsequent events
  test_device()->SendLEConnectionUpdateCompleteSubevent(conn_handle1->handle(),
                                                        hci::LEConnectionParameters(),
                                                        hci::StatusCode::kUnsupportedRemoteFeature);
  RunLoopUntilIdle();
  EXPECT_EQ(2u, l2cap_conn_param_update_count);
}

// Based on PTS L2CAP/LE/CPU/BV-01-C. When run twice, the controller caches the LE Connection
// Update Complete kUnsupportedRemoteFeature status and returns it directly in future LE
// Connection Update Command Status events. The host should retry with the L2CAP Connection
// Parameter Update Request after receiving this kUnsupportedRemoteFeature command status.
TEST_F(GAP_LowEnergyConnectionManagerTest,
       PeripheralSendsL2capConnParamReqAfterConnUpdateCommandStatusUnsupportedRemoteFeature) {
  auto peer = std::make_unique<FakePeer>(kAddress0);

  // Connection Parameter Update procedure supported by controller.
  constexpr hci::LESupportedFeatures kLEFeatures{
      static_cast<uint64_t>(hci::LESupportedFeature::kConnectionParametersRequestProcedure)};
  peer->set_le_features(kLEFeatures);
  test_device()->AddPeer(std::move(peer));

  // First create a fake incoming connection with local host as peripheral.
  test_device()->ConnectLowEnergy(kAddress0, hci::ConnectionRole::kSlave);
  RunLoopUntilIdle();

  auto link = MoveLastRemoteInitiated();
  ASSERT_TRUE(link);

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  conn_mgr()->RegisterRemoteInitiatedLink(std::move(link), BondableMode::Bondable,
                                          [&](auto result) {
                                            ASSERT_TRUE(result.is_ok());
                                            conn_handle = result.take_value();
                                          });

  size_t l2cap_conn_param_update_count = 0;
  size_t hci_update_conn_param_count = 0;

  fake_l2cap()->set_connection_parameter_update_request_responder([&](auto handle, auto params) {
    l2cap_conn_param_update_count++;
    return true;
  });

  test_device()->set_le_connection_parameters_callback(
      [&](auto address, auto params) { hci_update_conn_param_count++; });

  test_device()->SetDefaultCommandStatus(hci::kLEConnectionUpdate,
                                         hci::StatusCode::kUnsupportedRemoteFeature);

  RunLoopFor(kLEConnectionPausePeripheral);
  ASSERT_TRUE(conn_handle);
  EXPECT_TRUE(conn_handle->active());
  EXPECT_EQ(0u, hci_update_conn_param_count);
  EXPECT_EQ(1u, l2cap_conn_param_update_count);

  test_device()->ClearDefaultCommandStatus(hci::kLEConnectionUpdate);

  // l2cap request should not be called on subsequent events
  test_device()->SendLEConnectionUpdateCompleteSubevent(conn_handle->handle(),
                                                        hci::LEConnectionParameters(),
                                                        hci::StatusCode::kUnsupportedRemoteFeature);

  RunLoopUntilIdle();
  EXPECT_EQ(1u, l2cap_conn_param_update_count);
}

// A peripheral should not attempt to handle the next LE Connection Update Complete event if the
// status of the LE Connection Update command is not success.
TEST_F(GAP_LowEnergyConnectionManagerTest,
       PeripheralDoesNotSendL2capConnParamReqAfterConnUpdateCommandStatusError) {
  auto peer = std::make_unique<FakePeer>(kAddress0);

  // Connection Parameter Update procedure supported by controller.
  constexpr hci::LESupportedFeatures kLEFeatures{
      static_cast<uint64_t>(hci::LESupportedFeature::kConnectionParametersRequestProcedure)};
  peer->set_le_features(kLEFeatures);
  test_device()->AddPeer(std::move(peer));

  // First create a fake incoming connection with local host as peripheral.
  test_device()->ConnectLowEnergy(kAddress0, hci::ConnectionRole::kSlave);
  RunLoopUntilIdle();

  auto link = MoveLastRemoteInitiated();
  ASSERT_TRUE(link);

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  conn_mgr()->RegisterRemoteInitiatedLink(std::move(link), BondableMode::Bondable,
                                          [&](auto result) {
                                            ASSERT_TRUE(result.is_ok());
                                            conn_handle = result.take_value();
                                          });

  size_t l2cap_conn_param_update_count = 0;
  size_t hci_update_conn_param_count = 0;

  fake_l2cap()->set_connection_parameter_update_request_responder([&](auto handle, auto params) {
    l2cap_conn_param_update_count++;
    return true;
  });

  test_device()->set_le_connection_parameters_callback(
      [&](auto address, auto params) { hci_update_conn_param_count++; });

  test_device()->SetDefaultCommandStatus(hci::kLEConnectionUpdate,
                                         hci::StatusCode::kUnspecifiedError);

  RunLoopFor(kLEConnectionPausePeripheral);
  ASSERT_TRUE(conn_handle);
  EXPECT_TRUE(conn_handle->active());
  EXPECT_EQ(0u, hci_update_conn_param_count);
  EXPECT_EQ(0u, l2cap_conn_param_update_count);

  test_device()->ClearDefaultCommandStatus(hci::kLEConnectionUpdate);

  // l2cap request should not be called on subsequent events
  test_device()->SendLEConnectionUpdateCompleteSubevent(conn_handle->handle(),
                                                        hci::LEConnectionParameters(),
                                                        hci::StatusCode::kUnsupportedRemoteFeature);

  RunLoopUntilIdle();
  EXPECT_EQ(0u, l2cap_conn_param_update_count);
}

TEST_F(GAP_LowEnergyConnectionManagerTest, HciUpdateConnParamsAfterInterrogation) {
  constexpr hci::LESupportedFeatures kLEFeatures{
      static_cast<uint64_t>(hci::LESupportedFeature::kConnectionParametersRequestProcedure)};

  auto peer = std::make_unique<FakePeer>(kAddress0);
  peer->set_le_features(kLEFeatures);
  test_device()->AddPeer(std::move(peer));

  // First create a fake incoming connection.
  test_device()->ConnectLowEnergy(kAddress0, hci::ConnectionRole::kSlave);

  RunLoopUntilIdle();

  auto link = MoveLastRemoteInitiated();
  ASSERT_TRUE(link);

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  conn_mgr()->RegisterRemoteInitiatedLink(std::move(link), BondableMode::Bondable,
                                          [&](auto result) {
                                            ASSERT_TRUE(result.is_ok());
                                            conn_handle = result.take_value();
                                          });

  size_t l2cap_conn_param_update_count = 0;
  fake_l2cap()->set_connection_parameter_update_request_responder(
      [&](auto handle, const auto params) {
        l2cap_conn_param_update_count++;
        return true;
      });

  size_t hci_update_conn_param_count = 0;
  test_device()->set_le_connection_parameters_callback(
      [&](auto address, const hci::LEConnectionParameters& params) {
        // FakeController will pick an interval between min and max interval.
        EXPECT_TRUE(params.interval() >= hci::defaults::kLEConnectionIntervalMin &&
                    params.interval() <= hci::defaults::kLEConnectionIntervalMax);
        EXPECT_EQ(0u, params.latency());
        EXPECT_EQ(hci::defaults::kLESupervisionTimeout, params.supervision_timeout());
        hci_update_conn_param_count++;
      });

  RunLoopUntilIdle();
  ASSERT_TRUE(conn_handle);
  EXPECT_TRUE(conn_handle->active());
  EXPECT_EQ(0u, l2cap_conn_param_update_count);
  EXPECT_EQ(0u, hci_update_conn_param_count);

  RunLoopFor(kLEConnectionPausePeripheral);
  EXPECT_EQ(0u, l2cap_conn_param_update_count);
  EXPECT_EQ(1u, hci_update_conn_param_count);
}

TEST_F(GAP_LowEnergyConnectionManagerTest,
       CentralUpdatesConnectionParametersToDefaultsAfterInitialization) {
  // Set up a connection.
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  ASSERT_TRUE(peer->le());

  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));

  size_t hci_update_conn_param_count = 0;
  test_device()->set_le_connection_parameters_callback(
      [&](auto address, const hci::LEConnectionParameters& params) {
        // FakeController will pick an interval between min and max interval.
        EXPECT_TRUE(params.interval() >= hci::defaults::kLEConnectionIntervalMin &&
                    params.interval() <= hci::defaults::kLEConnectionIntervalMax);
        EXPECT_EQ(0u, params.latency());
        EXPECT_EQ(hci::defaults::kLESupervisionTimeout, params.supervision_timeout());
        hci_update_conn_param_count++;
      });

  std::unique_ptr<LowEnergyConnectionHandle> conn;
  conn_mgr()->Connect(
      peer->identifier(),
      [&](auto result) {
        ASSERT_TRUE(result.is_ok());
        conn = result.take_value();
      },
      kConnectionOptions);

  RunLoopUntilIdle();
  EXPECT_EQ(0u, hci_update_conn_param_count);

  RunLoopFor(kLEConnectionPauseCentral);
  EXPECT_EQ(1u, hci_update_conn_param_count);
  EXPECT_TRUE(conn);
}

TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectCalledForPeerBeingInterrogated) {
  // Set up a connection.
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  ASSERT_TRUE(peer->le());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  // Prevent remote features event from being received.
  test_device()->SetDefaultCommandStatus(hci::kLEReadRemoteFeatures, hci::StatusCode::kSuccess);

  conn_mgr()->Connect(
      peer->identifier(), [&](auto result) { ASSERT_TRUE(result.is_error()); }, kConnectionOptions);

  RunLoopUntilIdle();
  // Interrogation should not complete.
  EXPECT_FALSE(peer->le()->features().has_value());

  // Connect to same peer again, before interrogation has completed.
  // No asserts should fail.
  conn_mgr()->Connect(
      peer->identifier(), [&](auto result) { ASSERT_TRUE(result.is_error()); }, kConnectionOptions);
  RunLoopUntilIdle();
}

LowEnergyConnectionManager::ConnectionResultCallback MakeConnectionResultCallback(
    std::unique_ptr<LowEnergyConnectionHandle>& conn_handle) {
  return [&conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
    EXPECT_TRUE(conn_handle);
    EXPECT_TRUE(conn_handle->active());
  };
}

// Test that active connections not meeting the requirements for Secure Connections Only mode are
// disconnected when the security mode is changed to SC Only.
TEST_F(GAP_LowEnergyConnectionManagerTest, SecureConnectionsOnlyDisconnectsInsufficientSecurity) {
  Peer* encrypted_peer = peer_cache()->NewPeer(kAddress0, true);
  Peer* unencrypted_peer = peer_cache()->NewPeer(kAddress1, true);
  Peer* secure_authenticated_peer = peer_cache()->NewPeer(kAddress3, true);
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress0));
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress1));
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress3));

  std::unique_ptr<LowEnergyConnectionHandle> unencrypted_conn_handle, encrypted_conn_handle,
      secure_authenticated_conn_handle;
  EXPECT_TRUE(connected_peers().empty());
  conn_mgr()->Connect(unencrypted_peer->identifier(),
                      MakeConnectionResultCallback(unencrypted_conn_handle), kConnectionOptions);
  conn_mgr()->Connect(encrypted_peer->identifier(),
                      MakeConnectionResultCallback(encrypted_conn_handle), kConnectionOptions);
  conn_mgr()->Connect(secure_authenticated_peer->identifier(),
                      MakeConnectionResultCallback(secure_authenticated_conn_handle),
                      kConnectionOptions);
  RunLoopUntilIdle();
  std::function<void(sm::Status)> pair_cb = [](sm::Status s) { ASSERT_EQ(sm::Status(), s); };
  EXPECT_EQ(3u, connected_peers().size());
  ASSERT_TRUE(unencrypted_conn_handle);
  ASSERT_TRUE(encrypted_conn_handle);
  ASSERT_TRUE(secure_authenticated_conn_handle);
  EXPECT_TRUE(unencrypted_conn_handle->active());
  EXPECT_TRUE(secure_authenticated_conn_handle->active());
  EXPECT_TRUE(encrypted_conn_handle->active());

  // "Pair" to the encrypted peers to get to the correct security level.
  conn_mgr()->Pair(encrypted_peer->identifier(), sm::SecurityLevel::kEncrypted,
                   sm::BondableMode::Bondable, pair_cb);
  conn_mgr()->Pair(secure_authenticated_peer->identifier(), sm::SecurityLevel::kSecureAuthenticated,
                   sm::BondableMode::Bondable, pair_cb);
  RunLoopUntilIdle();
  EXPECT_EQ(sm::SecurityLevel::kNoSecurity, unencrypted_conn_handle->security().level());
  EXPECT_EQ(sm::SecurityLevel::kEncrypted, encrypted_conn_handle->security().level());
  EXPECT_EQ(sm::SecurityLevel::kSecureAuthenticated,
            secure_authenticated_conn_handle->security().level());

  // Setting Secure Connections Only mode causes connections not allowed under this mode to be
  // disconnected (in this case, `encrypted_peer` is encrypted, SC-generated, and with max
  // encryption key size, but not authenticated).
  conn_mgr()->SetSecurityMode(LeSecurityMode::SecureConnectionsOnly);
  RunLoopUntilIdle();
  EXPECT_EQ(LeSecurityMode::SecureConnectionsOnly, conn_mgr()->security_mode());
  EXPECT_EQ(2u, connected_peers().size());
  EXPECT_TRUE(unencrypted_conn_handle->active());
  EXPECT_TRUE(secure_authenticated_conn_handle->active());
  EXPECT_FALSE(encrypted_conn_handle->active());
}

// Test that both existing and new peers pick up on a change to Secure Connections Only mode.
TEST_F(GAP_LowEnergyConnectionManagerTest, SetSecureConnectionsOnlyModeWorks) {
  // LE Connection Manager defaults to Mode 1.
  EXPECT_EQ(LeSecurityMode::Mode1, conn_mgr()->security_mode());

  // This peer will already be connected when we set LE Secure Connections Only mode.
  Peer* existing_peer = peer_cache()->NewPeer(kAddress1, true);
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress1));
  std::unique_ptr<LowEnergyConnectionHandle> existing_conn_handle;
  RunLoopUntilIdle();

  conn_mgr()->Connect(existing_peer->identifier(),
                      MakeConnectionResultCallback(existing_conn_handle), kConnectionOptions);
  RunLoopUntilIdle();
  fxl::WeakPtr<TestSm> existing_peer_sm = TestSmByHandle(existing_conn_handle->handle());
  ASSERT_TRUE(existing_peer_sm);
  EXPECT_EQ(LeSecurityMode::Mode1, existing_peer_sm->security_mode());
  EXPECT_EQ(1u, connected_peers().size());

  conn_mgr()->SetSecurityMode(LeSecurityMode::SecureConnectionsOnly);
  RunLoopUntilIdle();

  EXPECT_EQ(LeSecurityMode::SecureConnectionsOnly, existing_peer_sm->security_mode());

  // This peer is connected after setting LE Secure Connections Only mode.
  Peer* new_peer = peer_cache()->NewPeer(kAddress3, true);
  test_device()->AddPeer(std::make_unique<FakePeer>(kAddress3));
  std::unique_ptr<LowEnergyConnectionHandle> new_conn_handle;

  conn_mgr()->Connect(new_peer->identifier(), MakeConnectionResultCallback(new_conn_handle),
                      kConnectionOptions);
  RunLoopUntilIdle();
  fxl::WeakPtr<TestSm> new_peer_sm = TestSmByHandle(new_conn_handle->handle());
  ASSERT_TRUE(new_peer_sm);
  EXPECT_EQ(2u, connected_peers().size());

  EXPECT_EQ(LeSecurityMode::SecureConnectionsOnly, new_peer_sm->security_mode());
}

TEST_F(GAP_LowEnergyConnectionManagerTest,
       ConnectAndInterrogateSecondPeerDuringInterrogationOfFirstPeer) {
  auto* peer_0 = peer_cache()->NewPeer(kAddress0, true);
  ASSERT_TRUE(peer_0->le());

  auto fake_peer_0 = std::make_unique<FakePeer>(kAddress0);
  auto fake_peer_0_ptr = fake_peer_0.get();
  test_device()->AddPeer(std::move(fake_peer_0));

  // Prevent remote features event from being received.
  test_device()->SetDefaultCommandStatus(hci::kLEReadRemoteFeatures, hci::StatusCode::kSuccess);

  std::unique_ptr<LowEnergyConnectionHandle> conn_0;
  conn_mgr()->Connect(
      peer_0->identifier(),
      [&conn_0](auto result) {
        ASSERT_TRUE(result.is_ok());
        conn_0 = result.take_value();
        ASSERT_TRUE(conn_0);
      },
      kConnectionOptions);

  RunLoopUntilIdle();
  // Interrogation should not complete.
  EXPECT_FALSE(peer_0->le()->connected());
  EXPECT_FALSE(conn_0);

  auto* peer_1 = peer_cache()->NewPeer(kAddress1, true);
  ASSERT_TRUE(peer_1->le());

  auto fake_peer_1 = std::make_unique<FakePeer>(kAddress1);
  auto fake_peer_1_ptr = fake_peer_1.get();
  test_device()->AddPeer(std::move(fake_peer_1));

  // Connect to different peer, before interrogation has completed.
  std::unique_ptr<LowEnergyConnectionHandle> conn_1;
  conn_mgr()->Connect(
      peer_1->identifier(),
      [&conn_1](auto result) {
        ASSERT_TRUE(result.is_ok());
        conn_1 = result.take_value();
        ASSERT_TRUE(conn_1);
      },
      kConnectionOptions);
  RunLoopUntilIdle();

  // Complete interrogation of peer_0
  ASSERT_FALSE(fake_peer_0_ptr->logical_links().empty());
  auto handle_0 = *fake_peer_0_ptr->logical_links().begin();
  hci::LEReadRemoteFeaturesCompleteSubeventParams response;
  response.connection_handle = htole16(handle_0);
  response.status = hci::kSuccess;
  response.le_features = 0u;
  test_device()->SendLEMetaEvent(hci::kLEReadRemoteFeaturesCompleteSubeventCode,
                                 BufferView(&response, sizeof(response)));
  RunLoopUntilIdle();
  EXPECT_TRUE(conn_0);
  EXPECT_TRUE(peer_0->le()->connected());

  // Complete interrogation of peer_1
  ASSERT_FALSE(fake_peer_1_ptr->logical_links().empty());
  auto handle_1 = *fake_peer_0_ptr->logical_links().begin();
  response.connection_handle = htole16(handle_1);
  test_device()->SendLEMetaEvent(hci::kLEReadRemoteFeaturesCompleteSubeventCode,
                                 BufferView(&response, sizeof(response)));
  RunLoopUntilIdle();
  EXPECT_TRUE(conn_1);
  EXPECT_TRUE(peer_1->le()->connected());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectSecondPeerDuringInterrogationOfFirstPeer) {
  auto* peer_0 = peer_cache()->NewPeer(kAddress0, true);
  ASSERT_TRUE(peer_0->le());

  auto fake_peer_0 = std::make_unique<FakePeer>(kAddress0);
  auto fake_peer_0_ptr = fake_peer_0.get();
  test_device()->AddPeer(std::move(fake_peer_0));

  // Prevent remote features event from being received.
  test_device()->SetDefaultCommandStatus(hci::kLEReadRemoteFeatures, hci::StatusCode::kSuccess);

  std::unique_ptr<LowEnergyConnectionHandle> conn_0;
  conn_mgr()->Connect(
      peer_0->identifier(),
      [&](auto result) {
        ASSERT_TRUE(result.is_ok());
        conn_0 = result.take_value();
      },
      kConnectionOptions);

  RunLoopUntilIdle();
  // Interrogation should not complete.
  EXPECT_FALSE(peer_0->le()->connected());
  EXPECT_FALSE(conn_0);

  test_device()->ClearDefaultCommandStatus(hci::kLEReadRemoteFeatures);
  // Stall connection complete for peer 1.
  test_device()->SetDefaultCommandStatus(hci::kLECreateConnection, hci::StatusCode::kSuccess);

  auto* peer_1 = peer_cache()->NewPeer(kAddress1, true);
  ASSERT_TRUE(peer_1->le());

  auto fake_peer_1 = std::make_unique<FakePeer>(kAddress1);
  test_device()->AddPeer(std::move(fake_peer_1));

  // Connect to different peer, before interrogation has completed.
  conn_mgr()->Connect(
      peer_1->identifier(), [&](auto result) { EXPECT_TRUE(result.is_error()); },
      kConnectionOptions);
  RunLoopUntilIdle();

  // Complete interrogation of peer_0. No asserts should fail.
  ASSERT_FALSE(fake_peer_0_ptr->logical_links().empty());
  auto handle_0 = *fake_peer_0_ptr->logical_links().begin();
  hci::LEReadRemoteFeaturesCompleteSubeventParams response;
  response.connection_handle = htole16(handle_0);
  response.status = hci::kSuccess;
  response.le_features = 0u;
  test_device()->SendLEMetaEvent(hci::kLEReadRemoteFeaturesCompleteSubeventCode,
                                 BufferView(&response, sizeof(response)));
  RunLoopUntilIdle();
  EXPECT_TRUE(conn_0);
  EXPECT_TRUE(peer_0->le()->connected());
}

TEST_F(GAP_LowEnergyConnectionManagerTest,
       SynchonousInterrogationAndNoCallbackRetainsConnectionRef) {
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  ASSERT_TRUE(peer->le());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  std::unique_ptr<LowEnergyConnectionHandle> conn;
  conn_mgr()->Connect(
      peer->identifier(),
      [&](auto result) {
        ASSERT_TRUE(result.is_ok());
        conn = result.take_value();
      },
      kConnectionOptions);

  RunLoopUntilIdle();
  EXPECT_TRUE(peer->le()->connected());
  EXPECT_TRUE(conn);

  // Disconnect
  conn = nullptr;
  RunLoopUntilIdle();

  // Second interrogation will complete synchronously because peer has already been interrogated.
  bool conn_cb_called = false;
  conn_mgr()->Connect(
      peer->identifier(),
      [&](auto result) {
        conn_cb_called = true;
        EXPECT_TRUE(result.is_ok());
        // Don't retain ref.
      },
      kConnectionOptions);
  // Wait for connect complete event.
  RunLoopUntilIdle();
  EXPECT_TRUE(conn_cb_called);
}

TEST_F(GAP_LowEnergyConnectionManagerTest, AutoConnectSkipsScanning) {
  auto* peer = peer_cache()->NewPeer(kAddress0, /*connectable=*/true);
  EXPECT_TRUE(peer->temporary());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  size_t scan_cb_count = 0;
  test_device()->set_scan_state_callback([&scan_cb_count](bool enabled) { scan_cb_count++; });

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto callback = [&conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
    EXPECT_TRUE(conn_handle->active());
  };

  EXPECT_TRUE(connected_peers().empty());
  LowEnergyConnectionOptions options{.auto_connect = true};
  conn_mgr()->Connect(peer->identifier(), callback, options);
  ASSERT_TRUE(peer->le());
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());

  RunLoopUntilIdle();

  EXPECT_EQ(1u, connected_peers().size());
  EXPECT_EQ(1u, connected_peers().count(kAddress0));

  ASSERT_TRUE(conn_handle);
  EXPECT_TRUE(conn_handle->active());
  EXPECT_EQ(peer->identifier(), conn_handle->peer_identifier());
  EXPECT_FALSE(peer->temporary());
  EXPECT_EQ(Peer::ConnectionState::kConnected, peer->le()->connection_state());
  EXPECT_EQ(scan_cb_count, 0u);
}

TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectSinglePeerStartDiscoveryFailed) {
  auto* peer = peer_cache()->NewPeer(kAddress0, /*connectable=*/true);
  EXPECT_TRUE(peer->temporary());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  size_t connect_cb_count = 0;
  auto callback = [&connect_cb_count](auto result) {
    EXPECT_TRUE(result.is_error());
    connect_cb_count++;
  };

  // Cause discovery to fail.
  test_device()->SetDefaultCommandStatus(hci::kLESetScanEnable,
                                         hci::StatusCode::kCommandDisallowed);

  EXPECT_TRUE(connected_peers().empty());
  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  ASSERT_TRUE(peer->le());
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());

  RunLoopUntilIdle();
  EXPECT_EQ(connect_cb_count, 1u);
  EXPECT_FALSE(peer->temporary());
  EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer->le()->connection_state());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectSinglePeerDiscoveryFailedDuringScan) {
  auto* peer = peer_cache()->NewPeer(kAddress0, /*connectable=*/true);
  EXPECT_TRUE(peer->temporary());

  // Don't add peer to FakeController to prevent scan from completing.

  size_t connect_cb_count = 0;
  auto callback = [&connect_cb_count](auto result) {
    EXPECT_TRUE(result.is_error());
    connect_cb_count++;
  };

  EXPECT_TRUE(connected_peers().empty());
  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  ASSERT_TRUE(peer->le());
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());
  RunLoopUntilIdle();
  EXPECT_EQ(connect_cb_count, 0u);

  // Cause discovery to fail when attempting to restart scan after scan period ends.
  test_device()->SetDefaultCommandStatus(hci::kLESetScanEnable,
                                         hci::StatusCode::kCommandDisallowed);
  RunLoopFor(kLEGeneralDiscoveryScanMin);
  EXPECT_EQ(connect_cb_count, 1u);
  EXPECT_FALSE(peer->temporary());
  EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer->le()->connection_state());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, PeerDisconnectBeforeInterrogationCompletes) {
  auto* peer = peer_cache()->NewPeer(kAddress0, /*connectable=*/true);
  EXPECT_TRUE(peer->temporary());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  auto fake_peer_ptr = fake_peer.get();
  test_device()->AddPeer(std::move(fake_peer));

  // Cause interrogation to stall by not responding with a Read Remote Version complete event.
  test_device()->SetDefaultCommandStatus(hci::kReadRemoteVersionInfo, hci::StatusCode::kSuccess);

  int connect_count = 0;
  auto callback = [&connect_count](auto result) {
    ASSERT_TRUE(result.is_error());
    connect_count++;
  };

  EXPECT_TRUE(connected_peers().empty());
  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  ASSERT_TRUE(peer->le());
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());

  RunLoopUntilIdle();

  ASSERT_FALSE(fake_peer_ptr->logical_links().empty());
  auto handle = *fake_peer_ptr->logical_links().begin();

  test_device()->Disconnect(peer->address());

  RunLoopUntilIdle();

  // Complete interrogation so that callback gets called.
  hci::ReadRemoteVersionInfoCompleteEventParams response = {};
  response.status = hci::kSuccess;
  response.connection_handle = htole16(handle);
  test_device()->SendEvent(hci::kReadRemoteVersionInfoCompleteEventCode,
                           BufferView(&response, sizeof(response)));

  RunLoopUntilIdle();
  EXPECT_EQ(0u, connected_peers().size());
  EXPECT_EQ(1, connect_count);
  EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer->le()->connection_state());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, LocalDisconnectBeforeInterrogationCompletes) {
  auto* peer = peer_cache()->NewPeer(kAddress0, /*connectable=*/true);
  EXPECT_TRUE(peer->temporary());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  auto fake_peer_ptr = fake_peer.get();
  test_device()->AddPeer(std::move(fake_peer));

  // Cause interrogation to stall by not responding with a Read Remote Version complete event.
  test_device()->SetDefaultCommandStatus(hci::kReadRemoteVersionInfo, hci::StatusCode::kSuccess);

  int connect_count = 0;
  auto callback = [&connect_count](auto result) {
    ASSERT_TRUE(result.is_error());
    connect_count++;
  };

  EXPECT_TRUE(connected_peers().empty());
  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  ASSERT_TRUE(peer->le());
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());

  RunLoopUntilIdle();

  ASSERT_FALSE(fake_peer_ptr->logical_links().empty());
  auto handle = *fake_peer_ptr->logical_links().begin();

  conn_mgr()->Disconnect(peer->identifier());

  RunLoopUntilIdle();

  // Complete interrogation so that callback gets called.
  hci::ReadRemoteVersionInfoCompleteEventParams response = {};
  response.status = hci::kSuccess;
  response.connection_handle = htole16(handle);
  test_device()->SendEvent(hci::kReadRemoteVersionInfoCompleteEventCode,
                           BufferView(&response, sizeof(response)));

  RunLoopUntilIdle();
  EXPECT_EQ(0u, connected_peers().size());
  EXPECT_EQ(1, connect_count);
  EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer->le()->connection_state());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectionFailedToBeEstablishedRetriesTwiceAndFails) {
  auto* peer = peer_cache()->NewPeer(kAddress0, /*connectable=*/true);
  EXPECT_TRUE(peer->temporary());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  size_t connected_count = 0;
  test_device()->set_connection_state_callback([&](auto, auto, bool connected, bool) {
    if (connected) {
      connected_count++;
    }
  });

  int connect_cb_count = 0;
  auto callback = [&connect_cb_count](auto result) {
    ASSERT_TRUE(result.is_error());
    connect_cb_count++;
  };

  EXPECT_TRUE(connected_peers().empty());

  // Cause interrogation to fail.
  test_device()->SetDefaultCommandStatus(hci::kReadRemoteVersionInfo,
                                         hci::StatusCode::kConnectionFailedToBeEstablished);

  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  ASSERT_TRUE(peer->le());
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());

  // Exhaust retries and cause connection to fail.
  for (size_t i = 0; i < kConnectDelays.size(); i++) {
    SCOPED_TRACE(i);
    if (i != 0) {
      RunLoopFor(kConnectDelays[i] - zx::nsec(1));
      EXPECT_EQ(connected_count, i);
      RunLoopFor(zx::nsec(1));
    } else {
      RunLoopFor(kConnectDelays[i]);
    }
    EXPECT_EQ(connected_count, i + 1);
    EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());

    test_device()->Disconnect(kAddress0, hci::StatusCode::kConnectionFailedToBeEstablished);
    RunLoopUntilIdle();
    EXPECT_EQ(connected_count, i + 1);
    // A connect command should be sent in connect_delays[i+1]
  }

  RunLoopUntilIdle();
  EXPECT_TRUE(connected_peers().empty());
  EXPECT_EQ(connect_cb_count, 1);
  EXPECT_FALSE(peer->temporary());
  EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer->le()->connection_state());
}

TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectionFailedToBeEstablishedRetriesAndSucceeds) {
  auto* peer = peer_cache()->NewPeer(kAddress0, /*connectable=*/true);
  EXPECT_TRUE(peer->temporary());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto callback = [&conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
    EXPECT_TRUE(conn_handle->active());
  };

  EXPECT_TRUE(connected_peers().empty());

  // Cause interrogation to fail.
  test_device()->SetDefaultCommandStatus(hci::kReadRemoteVersionInfo,
                                         hci::StatusCode::kConnectionFailedToBeEstablished);

  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  ASSERT_TRUE(peer->le());
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());

  RunLoopUntilIdle();
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());
  EXPECT_FALSE(conn_handle);

  // Allow the next interrogation to succeed.
  test_device()->ClearDefaultCommandStatus(hci::kReadRemoteVersionInfo);

  // Disconnect should initiate retry #2 after a pause.
  test_device()->Disconnect(kAddress0, hci::StatusCode::kConnectionFailedToBeEstablished);
  RunLoopFor(zx::sec(2));
  EXPECT_EQ(1u, connected_peers().size());
  EXPECT_EQ(1u, connected_peers().count(kAddress0));
  ASSERT_TRUE(conn_handle);
  EXPECT_TRUE(conn_handle->active());
  EXPECT_FALSE(peer->temporary());
  EXPECT_EQ(Peer::ConnectionState::kConnected, peer->le()->connection_state());
}

TEST_F(GAP_LowEnergyConnectionManagerTest,
       ConnectionFailedToBeEstablishedAndDisconnectDuringRetryPauseTimeout) {
  auto* peer = peer_cache()->NewPeer(kAddress0, /*connectable=*/true);
  EXPECT_TRUE(peer->temporary());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  int connect_cb_count = 0;
  auto callback = [&](auto result) {
    ASSERT_TRUE(result.is_error());
    EXPECT_EQ(result.error(), HostError::kCanceled);
    connect_cb_count++;
  };

  EXPECT_TRUE(connected_peers().empty());

  // Cause interrogation to fail.
  test_device()->SetDefaultCommandStatus(hci::kReadRemoteVersionInfo,
                                         hci::StatusCode::kConnectionFailedToBeEstablished);

  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  ASSERT_TRUE(peer->le());
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());

  RunLoopUntilIdle();
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());
  EXPECT_EQ(connect_cb_count, 0);

  // Allow the next interrogation to succeed (even though it shouldn't happen).
  test_device()->ClearDefaultCommandStatus(hci::kReadRemoteVersionInfo);

  // Peer disconnection during interrogation should also cause retry (after a pause)
  test_device()->Disconnect(kAddress0, hci::StatusCode::kConnectionFailedToBeEstablished);
  RunLoopUntilIdle();
  // Disconnect will cancel request.
  conn_mgr()->Disconnect(peer->identifier());
  // Ensure timer is canceled.
  RunLoopRepeatedlyFor(zx::sec(1));
  EXPECT_EQ(connect_cb_count, 1);
  EXPECT_EQ(0u, connected_peers().size());
  EXPECT_FALSE(peer->temporary());
  EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer->le()->connection_state());
}

// Tests that receiving a peer kConnectionFailedToBeEstablished disconnect event before
// interrogation fails does not crash.
TEST_F(GAP_LowEnergyConnectionManagerTest,
       ConnectionFailedToBeEstablishedDisconnectionBeforeInterrogationFails) {
  auto* peer = peer_cache()->NewPeer(kAddress0, /*connectable=*/true);
  EXPECT_TRUE(peer->temporary());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  int connect_cb_count = 0;
  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto callback = [&](auto result) {
    ASSERT_TRUE(result.is_ok());
    connect_cb_count++;
    conn_handle = result.take_value();
  };

  EXPECT_TRUE(connected_peers().empty());

  // Cause interrogation to stall waiting for command complete event.
  test_device()->SetDefaultCommandStatus(hci::kReadRemoteVersionInfo, hci::StatusCode::kSuccess);

  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  ASSERT_TRUE(peer->le());
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());

  RunLoopUntilIdle();
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());
  EXPECT_EQ(connect_cb_count, 0);

  // Let retries succeed.
  test_device()->ClearDefaultCommandStatus(hci::kReadRemoteVersionInfo);

  // Peer disconnection during interrogation should also cause retry (after a pause).
  test_device()->Disconnect(kAddress0, hci::StatusCode::kConnectionFailedToBeEstablished);
  RunLoopUntilIdle();

  // Complete interrogation with an error that will be received after the disconnect event.
  // Event params other than status will be ignored because status is an error.
  hci::ReadRemoteVersionInfoCompleteEventParams response{.status = hci::kUnknownConnectionId};
  test_device()->SendEvent(hci::kReadRemoteVersionInfoCompleteEventCode,
                           BufferView(&response, sizeof(response)));
  RunLoopUntilIdle();
  EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());
  EXPECT_EQ(connect_cb_count, 0);

  // Wait for retry.
  RunLoopFor(kConnectDelays[1]);
  EXPECT_EQ(connect_cb_count, 1);
  EXPECT_TRUE(conn_handle);
  EXPECT_EQ(1u, connected_peers().size());
  EXPECT_FALSE(peer->temporary());
  EXPECT_EQ(Peer::ConnectionState::kConnected, peer->le()->connection_state());
}

// Behavior verified in this test:
// 1. After a successful connection + bond to establish auto-connect for a peer, an auto-connect-
//    initiated connection attempt to that peer that fails with any of `statuses_that_disable_
//    autoconnect` disables auto-connect to that peer.
// 2. After a successful ..., NON-autoconnect-inititated connection attempts (inbound or outbound)
//    to that peer that fail with any of `statuses_that_disable_autoconnect` do NOT disable auto-
//    connect to that peer.
TEST_F(GAP_LowEnergyConnectionManagerTest, ConnectSucceedsThenAutoConnectFailsDisablesAutoConnect) {
  // If an auto-connect attempt fails with any of these status codes, we disable the auto-connect
  // behavior until the next successful connection to avoid looping.
  // clang-format off
  std::array statuses_that_disable_autoconnect = {
      hci::StatusCode::kConnectionTimeout,
      hci::StatusCode::kConnectionRejectedSecurity,
      hci::StatusCode::kConnectionAcceptTimeoutExceeded,
      hci::StatusCode::kConnectionTerminatedByLocalHost,
      hci::StatusCode::kConnectionFailedToBeEstablished
  };
  // clang-format on
  // Validate that looping with a uint8_t is safe, it makes the rest of the code simpler.
  static_assert(statuses_that_disable_autoconnect.size() < std::numeric_limits<uint8_t>::max());
  for (uint8_t i = 0; i < static_cast<uint8_t>(statuses_that_disable_autoconnect.size()); ++i) {
    SCOPED_TRACE(hci::StatusCodeToString(statuses_that_disable_autoconnect[i]));
    const DeviceAddress kAddressI(DeviceAddress::Type::kLEPublic, {i});
    auto* peer = peer_cache()->NewPeer(kAddressI, /*connectable=*/true);
    auto fake_peer = std::make_unique<FakePeer>(kAddressI);
    test_device()->AddPeer(std::move(fake_peer));

    std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
    auto success_cb = [&conn_handle](auto result) {
      ASSERT_TRUE(result.is_ok());
      conn_handle = result.take_value();
      EXPECT_TRUE(conn_handle->active());
    };

    conn_mgr()->Connect(peer->identifier(), success_cb, kConnectionOptions);
    RunLoopUntilIdle();
    // Peer needs to be bonded to set auto connect
    peer->MutLe().SetBondData(sm::PairingData{});

    EXPECT_EQ(1u, connected_peers().count(kAddressI));
    ASSERT_TRUE(conn_handle);
    EXPECT_EQ(peer->identifier(), conn_handle->peer_identifier());
    EXPECT_TRUE(peer->le()->should_auto_connect());
    EXPECT_EQ(Peer::ConnectionState::kConnected, peer->le()->connection_state());

    // Disconnect has to be initiated by the "remote" device - locally initiated disconnects will
    // unset auto connect behavior.
    test_device()->Disconnect(peer->address());

    RunLoopUntilIdle();

    EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer->le()->connection_state());
    EXPECT_TRUE(peer->le()->should_auto_connect());

    // Causes interrogation to fail, so inbound connections will fail to establish. This complexity
    // is needed because inbound connections are already HCI-connected when passed to the LECM.
    test_device()->SetDefaultCommandStatus(hci::kReadRemoteVersionInfo,
                                           statuses_that_disable_autoconnect[i]);

    ConnectionResult result;
    auto failure_cb = [&result](auto res) { result = std::move(res); };
    // Create an inbound HCI connection and try to register it with the LECM
    test_device()->ConnectLowEnergy(kAddressI);
    RunLoopUntilIdle();
    auto link = MoveLastRemoteInitiated();
    ASSERT_TRUE(link);
    result = ConnectionResult{};
    conn_mgr()->RegisterRemoteInitiatedLink(std::move(link), BondableMode::Bondable, failure_cb);
    RunLoopUntilIdle();
    // We always wait until the peer disconnects to relay connection failure when dealing with
    // the 0x3e kConnectionFailedToBeEstablished error.
    if (statuses_that_disable_autoconnect[i] == hci::StatusCode::kConnectionFailedToBeEstablished) {
      test_device()->Disconnect(kAddressI, hci::StatusCode::kConnectionFailedToBeEstablished);
      RunLoopUntilIdle();
    }
    // Remote-initiated connection attempts that fail should not disable the auto-connect flag.
    ASSERT_EQ(fit::result_state::error, result.state());
    EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer->le()->connection_state());
    EXPECT_TRUE(peer->le()->should_auto_connect());
    // Allow successful interrogation later in the test
    test_device()->ClearDefaultCommandStatus(hci::kReadRemoteVersionInfo);

    // Set this peer to reject all connections with statuses_that_disable_autoconnect[i]
    FakePeer* peer_ref = test_device()->FindPeer(peer->address());
    ASSERT_TRUE(peer);
    peer_ref->set_connect_response(statuses_that_disable_autoconnect[i]);

    // User-initiated connection attempts that fail should not disable the auto-connect flag.
    const LowEnergyConnectionOptions kNotAutoConnectOptions{.auto_connect = false};
    conn_mgr()->Connect(peer->identifier(), failure_cb, kNotAutoConnectOptions);
    RunLoopUntilIdle();

    ASSERT_EQ(fit::result_state::error, result.state());
    EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer->le()->connection_state());
    EXPECT_TRUE(peer->le()->should_auto_connect());

    // Emulate an auto-connection here, as we disable the auto-connect behavior only for
    // auto-connect-initiated attempts that fail, NOT for user-initiated or remote-initiated
    // connection attempts that fail.
    result = ConnectionResult{};
    const LowEnergyConnectionOptions kAutoConnectOptions{.auto_connect = true};
    conn_mgr()->Connect(peer->identifier(), failure_cb, kAutoConnectOptions);
    ASSERT_TRUE(peer->le());
    EXPECT_EQ(Peer::ConnectionState::kInitializing, peer->le()->connection_state());

    RunLoopUntilIdle();

    ASSERT_EQ(fit::result_state::error, result.state());
    EXPECT_EQ(Peer::ConnectionState::kNotConnected, peer->le()->connection_state());
    EXPECT_FALSE(peer->le()->should_auto_connect());
  }
}

TEST_F(GAP_LowEnergyConnectionManagerTest, Inspect) {
  inspect::Inspector inspector;
  conn_mgr()->AttachInspect(inspector.GetRoot());

  auto* peer = peer_cache()->NewPeer(kAddress0, /*connectable=*/true);
  EXPECT_TRUE(peer->temporary());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  std::unique_ptr<LowEnergyConnectionHandle> conn_handle;
  auto callback = [&conn_handle](auto result) {
    ASSERT_TRUE(result.is_ok());
    conn_handle = result.take_value();
  };
  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);

  auto requests_matcher = AllOf(
      NodeMatches(NameMatches("pending_requests")),
      ChildrenMatch(ElementsAre(NodeMatches(AllOf(
          NameMatches("pending_request_0x0"),
          PropertyList(UnorderedElementsAre(StringIs("peer_id", peer->identifier().ToString()),
                                            IntIs("callbacks", 1))))))));

  auto outbound_connector_matcher_attempt_0 = AllOf(NodeMatches(AllOf(
      NameMatches("outbound_connector"),
      PropertyList(UnorderedElementsAre(StringIs("peer_id", peer->identifier().ToString()),
                                        IntIs("connection_attempt", 0), BoolIs("is_outbound", true),
                                        StringIs("state", "StartingScanning"))))));

  auto empty_connections_matcher =
      AllOf(NodeMatches(NameMatches("connections")), ChildrenMatch(::testing::IsEmpty()));

  auto conn_mgr_property_matcher =
      PropertyList(UnorderedElementsAre(IntIs("recent_connection_failures", 0)));

  auto conn_mgr_during_connecting_matcher =
      AllOf(NodeMatches(AllOf(NameMatches(LowEnergyConnectionManager::kInspectNodeName),
                              conn_mgr_property_matcher)),
            ChildrenMatch(UnorderedElementsAre(requests_matcher, empty_connections_matcher,
                                               outbound_connector_matcher_attempt_0)));

  auto hierarchy = inspect::ReadFromVmo(inspector.DuplicateVmo());
  EXPECT_THAT(hierarchy.value(), ChildrenMatch(ElementsAre(conn_mgr_during_connecting_matcher)));

  // Finish connecting.
  RunLoopUntilIdle();

  auto empty_requests_matcher =
      AllOf(NodeMatches(NameMatches("pending_requests")), ChildrenMatch(::testing::IsEmpty()));

  auto conn_matcher = NodeMatches(
      AllOf(NameMatches("connection_0x1"),
            PropertyList(UnorderedElementsAre(StringIs("peer_id", peer->identifier().ToString()),
                                              StringIs("peer_address", peer->address().ToString()),
                                              IntIs("ref_count", 1)))));

  auto connections_matcher =
      AllOf(NodeMatches(NameMatches("connections")), ChildrenMatch(ElementsAre(conn_matcher)));

  auto conn_mgr_after_connecting_matcher =
      AllOf(NodeMatches(conn_mgr_property_matcher),
            ChildrenMatch(UnorderedElementsAre(empty_requests_matcher, connections_matcher)));

  hierarchy = inspect::ReadFromVmo(inspector.DuplicateVmo());
  EXPECT_THAT(hierarchy.value(), ChildrenMatch(ElementsAre(conn_mgr_after_connecting_matcher)));

  // LECM must be destroyed before the inspector to avoid a page fault on destruction of inspect
  // properties (they try to update the inspect VMO, which is deleted on inspector destruction).
  DeleteConnMgr();
}

TEST_F(GAP_LowEnergyConnectionManagerTest, InspectFailedConnection) {
  inspect::Inspector inspector;
  conn_mgr()->AttachInspect(inspector.GetRoot());

  auto* peer = peer_cache()->NewPeer(kAddress0, /*connectable=*/true);
  EXPECT_TRUE(peer->temporary());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  fake_peer->set_connect_status(hci::StatusCode::kConnectionLimitExceeded);
  test_device()->AddPeer(std::move(fake_peer));

  auto callback = [](auto result) { ASSERT_TRUE(result.is_error()); };
  conn_mgr()->Connect(peer->identifier(), callback, kConnectionOptions);
  RunLoopUntilIdle();

  auto conn_mgr_property_matcher =
      PropertyList(UnorderedElementsAre(IntIs("recent_connection_failures", 1)));

  auto hierarchy = inspect::ReadFromVmo(inspector.DuplicateVmo());
  EXPECT_THAT(hierarchy.value(),
              ChildrenMatch(ElementsAre(NodeMatches(conn_mgr_property_matcher))));

  RunLoopFor(LowEnergyConnectionManager::kInspectRecentConnectionFailuresExpiryDuration -
             zx::nsec(1));
  hierarchy = inspect::ReadFromVmo(inspector.DuplicateVmo());
  EXPECT_THAT(hierarchy.value(),
              ChildrenMatch(ElementsAre(NodeMatches(conn_mgr_property_matcher))));

  // Failures should revert to 0 after expiry duration.
  RunLoopFor(zx::nsec(1));
  conn_mgr_property_matcher =
      PropertyList(UnorderedElementsAre(IntIs("recent_connection_failures", 0)));
  hierarchy = inspect::ReadFromVmo(inspector.DuplicateVmo());
  EXPECT_THAT(hierarchy.value(),
              ChildrenMatch(ElementsAre(NodeMatches(conn_mgr_property_matcher))));

  // LECM must be destroyed before the inspector to avoid a page fault on destruction of inspect
  // properties (they try to update the inspect VMO, which is deleted on inspector destruction).
  DeleteConnMgr();
}

// Tests for assertions that enforce invariants.
class GAP_LowEnergyConnectionManagerDeathTest : public LowEnergyConnectionManagerTest {};

// Tests that a disconnection event that occurs after a peer gets removed is handled gracefully.
TEST_F(GAP_LowEnergyConnectionManagerDeathTest, DisconnectAfterPeerRemovalAsserts) {
  // Set up a connection.
  auto* peer = peer_cache()->NewPeer(kAddress0, true);
  EXPECT_TRUE(peer->temporary());

  auto fake_peer = std::make_unique<FakePeer>(kAddress0);
  test_device()->AddPeer(std::move(fake_peer));

  std::unique_ptr<LowEnergyConnectionHandle> conn;
  conn_mgr()->Connect(peer->identifier(), MakeConnectionResultCallback(conn), kConnectionOptions);
  RunLoopUntilIdle();
  ASSERT_TRUE(conn);

  hci::ConnectionHandle handle = conn->handle();

  EXPECT_DEATH_IF_SUPPORTED(
      {
        // Remove the peer without removing it from the cache. Normally this is not recommended as
        // implied by the name of the function but it is possible for this invariant to be broken
        // due to programmer error. The connection manager should assert this invariant.
        peer->MutLe().SetConnectionState(Peer::ConnectionState::kNotConnected);
        __UNUSED auto _ = peer_cache()->RemoveDisconnectedPeer(peer->identifier());

        test_device()->SendDisconnectionCompleteEvent(handle);
        RunLoopUntilIdle();
      },
      ".*");
}

// Test fixture for tests that disconnect a connection in various ways and expect that
// controller packet counts are not cleared on disconnecting, but are cleared on disconnection
// complete. Tests should disconnect conn_handle0().
class PendingPacketsTest : public LowEnergyConnectionManagerTest {
 public:
  PendingPacketsTest() = default;
  ~PendingPacketsTest() override = default;

  void SetUp() override {
    LowEnergyConnectionManagerTest::SetUp();
    const DeviceAddress kPeerAddr0(DeviceAddress::Type::kLEPublic, {1});
    const DeviceAddress kPeerAddr1(DeviceAddress::Type::kLEPublic, {2});

    peer0_ = peer_cache()->NewPeer(kPeerAddr0, true);
    EXPECT_TRUE(peer0_->temporary());
    test_device()->AddPeer(std::make_unique<FakePeer>(kPeerAddr0));

    peer1_ = peer_cache()->NewPeer(kPeerAddr1, true);
    EXPECT_TRUE(peer1_->temporary());
    test_device()->AddPeer(std::make_unique<FakePeer>(kPeerAddr1));

    // Connect |peer0|
    conn_handle0_.reset();
    auto callback0 = [this](auto result) {
      ASSERT_TRUE(result.is_ok());
      conn_handle0_ = result.take_value();
      EXPECT_TRUE(conn_handle0_->active());
    };
    conn_mgr()->Connect(peer0_->identifier(), callback0, kConnectionOptions);
    RunLoopUntilIdle();

    // Connect |peer1|
    conn_handle1_.reset();
    auto callback1 = [this](auto result) {
      ASSERT_TRUE(result.is_ok());
      conn_handle1_ = result.take_value();
      EXPECT_TRUE(conn_handle1_->active());
    };
    conn_mgr()->Connect(peer1_->identifier(), callback1, kConnectionOptions);
    RunLoopUntilIdle();

    packet_count_ = 0;
    test_device()->SetDataCallback([&](const auto&) { packet_count_++; }, dispatcher());
    test_device()->set_auto_completed_packets_event_enabled(false);
    test_device()->set_auto_disconnection_complete_event_enabled(false);

    // Fill controller buffer by sending |kMaxNumPackets| packets to peer0.
    for (size_t i = 0; i < kLEMaxNumPackets; i++) {
      ASSERT_TRUE(acl_data_channel()->SendPacket(
          hci::ACLDataPacket::New(conn_handle0_->handle(),
                                  hci::ACLPacketBoundaryFlag::kFirstNonFlushable,
                                  hci::ACLBroadcastFlag::kPointToPoint, 1),
          l2cap::kInvalidChannelId, hci::AclDataChannel::PacketPriority::kLow));
    }

    // Queue packet for |peer1|.
    ASSERT_TRUE(acl_data_channel()->SendPacket(
        hci::ACLDataPacket::New(conn_handle1_->handle(),
                                hci::ACLPacketBoundaryFlag::kFirstNonFlushable,
                                hci::ACLBroadcastFlag::kPointToPoint, 1),
        l2cap::kInvalidChannelId, hci::AclDataChannel::PacketPriority::kLow));

    RunLoopUntilIdle();

    // Packet for |peer1| should not have been sent because controller buffer is full.
    EXPECT_EQ(kLEMaxNumPackets, packet_count_);

    handle0_ = conn_handle0_->handle();
  }

  void TearDown() override {
    RunLoopUntilIdle();

    // Packet for |peer1| should not have been sent before Disconnection Complete event.
    EXPECT_EQ(kLEMaxNumPackets, packet_count_);

    // This makes FakeController send us the HCI Disconnection Complete event.
    test_device()->SendDisconnectionCompleteEvent(handle0_);
    RunLoopUntilIdle();

    // |peer0|'s link should have been unregistered.
    ASSERT_FALSE(acl_data_channel()->SendPacket(
        hci::ACLDataPacket::New(handle0_, hci::ACLPacketBoundaryFlag::kFirstNonFlushable,
                                hci::ACLBroadcastFlag::kPointToPoint, 1),
        l2cap::kInvalidChannelId, hci::AclDataChannel::PacketPriority::kLow));

    // Packet for |peer1| should have been sent.
    EXPECT_EQ(kLEMaxNumPackets + 1, packet_count_);

    peer0_ = nullptr;
    peer1_ = nullptr;
    conn_handle0_.reset();
    conn_handle1_.reset();

    LowEnergyConnectionManagerTest::TearDown();
  }

  Peer* peer0() { return peer0_; }
  std::unique_ptr<LowEnergyConnectionHandle>& conn_handle0() { return conn_handle0_; }

 private:
  size_t packet_count_;
  Peer* peer0_;
  Peer* peer1_;
  hci::ConnectionHandle handle0_;
  std::unique_ptr<LowEnergyConnectionHandle> conn_handle0_;
  std::unique_ptr<LowEnergyConnectionHandle> conn_handle1_;
};

using GAP_LowEnergyConnectionManagerPendingPacketsTest = PendingPacketsTest;

TEST_F(GAP_LowEnergyConnectionManagerPendingPacketsTest, Disconnect) {
  // Send HCI Disconnect to controller.
  EXPECT_TRUE(conn_mgr()->Disconnect(peer0()->identifier()));
}

TEST_F(GAP_LowEnergyConnectionManagerPendingPacketsTest, ReleaseRef) {
  // Releasing ref should send HCI Disconnect to controller.
  conn_handle0().reset();
}

}  // namespace
}  // namespace bt::gap
