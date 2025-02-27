// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// +build !build_with_native_toolchain

package netstack

import (
	"context"
	"fmt"
	"testing"
	"time"

	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/dns"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/fidlconv"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/util"

	"fidl/fuchsia/net/interfaces"

	"github.com/google/go-cmp/cmp"
	"github.com/google/go-cmp/cmp/cmpopts"
	"gvisor.dev/gvisor/pkg/tcpip"
	"gvisor.dev/gvisor/pkg/tcpip/faketime"
	"gvisor.dev/gvisor/pkg/tcpip/header"
	"gvisor.dev/gvisor/pkg/tcpip/network/ipv6"
	"gvisor.dev/gvisor/pkg/tcpip/stack"
)

const (
	shortLifetime         = time.Nanosecond
	shortLifetimeTimeout  = time.Second
	middleLifetime        = 500 * time.Millisecond
	middleLifetimeTimeout = middleLifetime * time.Second
	incrementalTimeout    = 100 * time.Millisecond
	defaultLifetime       = time.Hour
)

var (
	subnet1           = newSubnet(util.Parse("abcd:1234::"), tcpip.AddressMask(util.Parse("ffff:ffff::")))
	subnet2           = newSubnet(util.Parse("abcd:1236::"), tcpip.AddressMask(util.Parse("ffff:ffff::")))
	testProtocolAddr1 = tcpip.ProtocolAddress{
		Protocol: ipv6.ProtocolNumber,
		AddressWithPrefix: tcpip.AddressWithPrefix{
			Address:   util.Parse("abcd:ee00::1"),
			PrefixLen: 64,
		},
	}
	testProtocolAddr2 = tcpip.ProtocolAddress{
		Protocol: ipv6.ProtocolNumber,
		AddressWithPrefix: tcpip.AddressWithPrefix{
			Address:   util.Parse("abcd:ef00::1"),
			PrefixLen: 64,
		},
	}
)

func newSubnet(addr tcpip.Address, mask tcpip.AddressMask) tcpip.Subnet {
	subnet, err := tcpip.NewSubnet(addr, mask)
	if err != nil {
		panic(fmt.Sprintf("NewSubnet(%s, %s): %s", addr, mask, err))
	}
	return subnet
}

// newNDPDispatcherForTest returns a new ndpDispatcher with a channel used to
// notify tests when its event queue is emptied.
func newNDPDispatcherForTest() *ndpDispatcher {
	n := newNDPDispatcher()
	n.testNotifyCh = make(chan struct{}, 1)
	return n
}

// waitForEmptyQueue returns after the event queue is emptied.
//
// If n's event queue is empty when waitForEmptyQueue is called, then
// waitForEmptyQueue returns immediately.
func waitForEmptyQueue(n *ndpDispatcher) {
	// Wait for an empty event queue.
	for {
		n.mu.Lock()
		empty := len(n.mu.events) == 0
		n.mu.Unlock()
		if empty {
			break
		}
		<-n.testNotifyCh
	}
}

// Test that interface state watchers receive an event on DAD and SLAAC address
// invalidation events.
func TestInterfacesChangeEvent(t *testing.T) {
	tests := []struct {
		name       string
		ndpEventFn func(id tcpip.NICID, ndpDisp *ndpDispatcher)
		addr       tcpip.AddressWithPrefix
	}{
		{
			name: "On DAD event",
			ndpEventFn: func(id tcpip.NICID, ndpDisp *ndpDispatcher) {
				ndpDisp.OnDuplicateAddressDetectionResult(id, testLinkLocalV6Addr1, &stack.DADSucceeded{})
			},
			addr: tcpip.AddressWithPrefix{
				Address:   testLinkLocalV6Addr1,
				PrefixLen: 10,
			},
		},
		{
			name: "On SLAAC address invalidated event",
			ndpEventFn: func(id tcpip.NICID, ndpDisp *ndpDispatcher) {
				ndpDisp.OnAutoGenAddress(id, testProtocolAddr1.AddressWithPrefix)
				ndpDisp.OnAutoGenAddressInvalidated(id, testProtocolAddr1.AddressWithPrefix)
			},
			addr: testProtocolAddr1.AddressWithPrefix,
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()

			ndpDisp := newNDPDispatcherForTest()
			ns, _ := newNetstackWithNDPDispatcher(t, ndpDisp)
			si := &interfaceStateImpl{ns: ns}
			ndpDisp.start(ctx)

			ifs := addNoopEndpoint(t, ns, "")
			t.Cleanup(ifs.Remove)
			if err := ifs.Up(); err != nil {
				t.Fatalf("ifs.Up(): %s", err)
			}
			if nicFound, err := ns.addInterfaceAddress(ifs.nicid, tcpip.ProtocolAddress{
				Protocol:          ipv6.ProtocolNumber,
				AddressWithPrefix: test.addr,
			}); !nicFound || err != nil {
				t.Fatalf("failed to add address: nicFound=%t, err=%s", nicFound, err)
			}

			request, watcher, err := interfaces.NewWatcherWithCtxInterfaceRequest()
			if err != nil {
				t.Fatalf("failed to create interface watcher protocol channel pair: %s", err)
			}
			if err := si.GetWatcher(context.Background(), interfaces.WatcherOptions{}, request); err != nil {
				t.Fatalf("failed to initialize interface watcher: %s", err)
			}
			hasAddr := func(addresses []interfaces.Address) bool {
				for _, a := range addresses {
					if a.HasAddr() && fidlconv.ToTCPIPSubnet(a.GetAddr()) == test.addr.Subnet() {
						return true
					}
				}
				return false
			}
			if event, err := watcher.Watch(context.Background()); err != nil {
				t.Fatalf("failed to watch: %s", err)
			} else if event.Which() != interfaces.EventExisting || event.Existing.GetId() != uint64(ifs.nicid) || !hasAddr(event.Existing.GetAddresses()) {
				t.Fatalf("got: %+v, expected interface %d exists event with address %s", event, ifs.nicid, test.addr)
			}

			if event, err := watcher.Watch(context.Background()); err != nil {
				t.Fatalf("failed to watch: %s", err)
			} else if event.Which() != interfaces.EventIdle {
				t.Fatalf("got: %+v, expected Idle event", event)
			}

			// Remove address directly without causing a watcher state update so that
			// the watcher event must be induced by the NDP event.
			if err := ns.stack.RemoveAddress(ifs.nicid, test.addr.Address); err != nil {
				t.Fatalf("error removing address: %s", err)
			}
			// Send an event to ndpDisp that should trigger a watcher event from the netstack.
			test.ndpEventFn(ifs.nicid, ndpDisp)
			waitForEmptyQueue(ndpDisp)

			if event, err := watcher.Watch(context.Background()); err != nil {
				t.Fatalf("failed to watch: %s", err)
			} else if event.Which() != interfaces.EventChanged || event.Changed.GetId() != uint64(ifs.nicid) || hasAddr(event.Changed.GetAddresses()) {
				t.Fatalf("got: %+v, expected interface %d changed event without address %s", event, ifs.nicid, test.addr)
			}
		})
	}
}

// Test that attempting to invalidate a default router which we do not have a
// route for is not an issue.
func TestNDPInvalidateUnknownIPv6Router(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	ndpDisp := newNDPDispatcherForTest()
	ns, _ := newNetstackWithNDPDispatcher(t, ndpDisp)
	ndpDisp.start(ctx)

	ifs := addNoopEndpoint(t, ns, "")
	t.Cleanup(ifs.Remove)
	if err := ifs.Up(); err != nil {
		t.Fatalf("ifs.Up(): %s", err)
	}

	// Invalidate the router with IP testLinkLocalV6Addr1 from eth (even
	// though we do not yet know about it).
	ndpDisp.OnDefaultRouterInvalidated(ifs.nicid, testLinkLocalV6Addr1)
	waitForEmptyQueue(ndpDisp)
	if rt, rts := defaultV6Route(ifs.nicid, testLinkLocalV6Addr1), ns.stack.GetRouteTable(); containsRoute(rts, rt) {
		t.Fatalf("should not have route = %s in the route table, got = %s", rt, rts)
	}
}

// Test that ndpDispatcher properly handles the discovery and invalidation of
// default IPv6 routers.
func TestNDPIPv6RouterDiscovery(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	ndpDisp := newNDPDispatcherForTest()
	ns, _ := newNetstackWithNDPDispatcher(t, ndpDisp)
	ndpDisp.start(ctx)

	ifs1 := addNoopEndpoint(t, ns, "")
	t.Cleanup(ifs1.Remove)
	if err := ifs1.Up(); err != nil {
		t.Fatalf("ifs1.Up(): %s", err)
	}
	ifs2 := addNoopEndpoint(t, ns, "")
	t.Cleanup(ifs2.Remove)
	if err := ifs2.Up(); err != nil {
		t.Fatalf("ifs2.Up(): %s", err)
	}

	// Test discovering a new default router on eth1.
	accept := ndpDisp.OnDefaultRouterDiscovered(ifs1.nicid, testLinkLocalV6Addr1)
	if !accept {
		t.Fatalf("got OnDefaultRouterDiscovered(%d, %s) = false, want = true", ifs1.nicid, testLinkLocalV6Addr1)
	}
	waitForEmptyQueue(ndpDisp)
	nic1Rtr1Rt := defaultV6Route(ifs1.nicid, testLinkLocalV6Addr1)
	if rts := ns.stack.GetRouteTable(); !containsRoute(rts, nic1Rtr1Rt) {
		t.Fatalf("missing route = %s from route table, got = %s", nic1Rtr1Rt, rts)
	}

	// Test discovering a new default router on eth2 (with the same
	// link-local IP as the one discovered as eth1).
	accept = ndpDisp.OnDefaultRouterDiscovered(ifs2.nicid, testLinkLocalV6Addr1)
	if !accept {
		t.Fatalf("got OnDefaultRouterDiscovered(%d, %s) = false, want = true", ifs2.nicid, testLinkLocalV6Addr1)
	}
	waitForEmptyQueue(ndpDisp)
	nic2Rtr1Rt := defaultV6Route(ifs2.nicid, testLinkLocalV6Addr1)
	rts := ns.stack.GetRouteTable()
	if !containsRoute(rts, nic2Rtr1Rt) {
		t.Fatalf("missing route = %s from route table, got = %s", nic2Rtr1Rt, rts)
	}
	// Should still have the route from before.
	if !containsRoute(rts, nic1Rtr1Rt) {
		t.Fatalf("missing route = %s from route table, got = %s", nic1Rtr1Rt, rts)
	}

	// Test discovering another default router on eth2.
	accept = ndpDisp.OnDefaultRouterDiscovered(ifs2.nicid, testLinkLocalV6Addr2)
	if !accept {
		t.Fatalf("got OnDefaultRouterDiscovered(%d, %s) = false, want = true", ifs2.nicid, testLinkLocalV6Addr1)
	}
	waitForEmptyQueue(ndpDisp)
	nic2Rtr2Rt := defaultV6Route(ifs2.nicid, testLinkLocalV6Addr2)
	rts = ns.stack.GetRouteTable()
	if !containsRoute(rts, nic2Rtr2Rt) {
		t.Fatalf("missing route = %s from route table, got = %s", nic2Rtr2Rt, rts)
	}
	// Should still have the routes from before.
	if !containsRoute(rts, nic2Rtr1Rt) {
		t.Fatalf("missing route = %s from route table, got = %s", nic2Rtr1Rt, rts)
	}
	if !containsRoute(rts, nic1Rtr1Rt) {
		t.Fatalf("missing route = %s from route table, got = %s", nic1Rtr1Rt, rts)
	}

	// Invalidate the router with IP testLinkLocalV6Addr1 from eth2.
	ndpDisp.OnDefaultRouterInvalidated(ifs2.nicid, testLinkLocalV6Addr1)
	waitForEmptyQueue(ndpDisp)
	rts = ns.stack.GetRouteTable()
	if containsRoute(rts, nic2Rtr1Rt) {
		t.Fatalf("should not have route = %s in the route table, got = %s", nic2Rtr1Rt, rts)
	}
	// Should still have default routes through the non-invalidated
	// routers.
	if !containsRoute(rts, nic2Rtr2Rt) {
		t.Fatalf("missing route = %s from route table, got = %s", nic2Rtr2Rt, rts)
	}
	if !containsRoute(rts, nic1Rtr1Rt) {
		t.Fatalf("missing route = %s from route table, got = %s", nic1Rtr1Rt, rts)
	}

	// Invalidate the router with IP testLinkLocalV6Addr1 from eth1.
	ndpDisp.OnDefaultRouterInvalidated(ifs1.nicid, testLinkLocalV6Addr1)
	waitForEmptyQueue(ndpDisp)
	rts = ns.stack.GetRouteTable()
	if containsRoute(rts, nic1Rtr1Rt) {
		t.Fatalf("should not have route = %s in the route table, got = %s", nic1Rtr1Rt, rts)
	}
	// Should still not have the other invalidated route.
	if containsRoute(rts, nic2Rtr1Rt) {
		t.Fatalf("should not have route = %s in the route table, got = %s", nic2Rtr1Rt, rts)
	}
	// Should still have default route through the non-invalidated router.
	if !containsRoute(rts, nic2Rtr2Rt) {
		t.Fatalf("missing route = %s from route table, got = %s", nic2Rtr2Rt, rts)
	}

	// Invalidate the router with IP testLinkLocalV6Addr2 from eth2.
	ndpDisp.OnDefaultRouterInvalidated(ifs2.nicid, testLinkLocalV6Addr2)
	waitForEmptyQueue(ndpDisp)
	rts = ns.stack.GetRouteTable()
	if containsRoute(rts, nic2Rtr2Rt) {
		t.Fatalf("should not have route = %s in the route table, got = %s", nic2Rtr2Rt, rts)
	}
	// Should still not have the other invalidated route.
	if containsRoute(rts, nic1Rtr1Rt) {
		t.Fatalf("should not have route = %s in the route table, got = %s", nic1Rtr1Rt, rts)
	}
	if containsRoute(rts, nic2Rtr1Rt) {
		t.Fatalf("should not have route = %s in the route table, got = %s", nic2Rtr1Rt, rts)
	}
}

// Test that attempting to invalidate an on-link prefix which we do not have a
// route for is not an issue.
func TestNDPInvalidateUnknownIPv6Prefix(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	ndpDisp := newNDPDispatcherForTest()
	ns, _ := newNetstackWithNDPDispatcher(t, ndpDisp)
	ndpDisp.start(ctx)

	ifs := addNoopEndpoint(t, ns, "")
	t.Cleanup(ifs.Remove)
	if err := ifs.Up(); err != nil {
		t.Fatalf("ifs.Up(): %s", err)
	}

	// Invalidate the prefix subnet1 from eth (even though we do not yet know
	// about it).
	ndpDisp.OnOnLinkPrefixInvalidated(ifs.nicid, subnet1)
	waitForEmptyQueue(ndpDisp)
	if rt, rts := onLinkV6Route(ifs.nicid, subnet1), ns.stack.GetRouteTable(); containsRoute(rts, rt) {
		t.Fatalf("should not have route = %s in the route table, got = %s", rt, rts)
	}
}

// Test that ndpDispatcher properly handles the discovery and invalidation of
// on-link IPv6 prefixes.
func TestNDPIPv6PrefixDiscovery(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	ndpDisp := newNDPDispatcherForTest()
	ns, _ := newNetstackWithNDPDispatcher(t, ndpDisp)
	ndpDisp.start(ctx)

	ifs1 := addNoopEndpoint(t, ns, "")
	t.Cleanup(ifs1.Remove)
	if err := ifs1.Up(); err != nil {
		t.Fatalf("ifs1.Up(): %s", err)
	}
	ifs2 := addNoopEndpoint(t, ns, "")
	t.Cleanup(ifs2.Remove)
	if err := ifs2.Up(); err != nil {
		t.Fatalf("ifs2.Up(): %s", err)
	}

	// Test discovering a new on-link prefix on eth1.
	accept := ndpDisp.OnOnLinkPrefixDiscovered(ifs1.nicid, subnet1)
	if !accept {
		t.Fatalf("got OnOnLinkPrefixDiscovered(%d, %s) = false, want = true", ifs1.nicid, subnet1)
	}
	waitForEmptyQueue(ndpDisp)
	nic1Sub1Rt := onLinkV6Route(ifs1.nicid, subnet1)
	if rts := ns.stack.GetRouteTable(); !containsRoute(rts, nic1Sub1Rt) {
		t.Fatalf("missing route = %s from route table, got = %s", nic1Sub1Rt, rts)
	}

	// Test discovering the same on-link prefix on eth2.
	accept = ndpDisp.OnOnLinkPrefixDiscovered(ifs2.nicid, subnet1)
	if !accept {
		t.Fatalf("got OnOnLinkPrefixDiscovered(%d, %s) = false, want = true", ifs2.nicid, subnet1)
	}
	waitForEmptyQueue(ndpDisp)
	nic2Sub1Rt := onLinkV6Route(ifs2.nicid, subnet1)
	rts := ns.stack.GetRouteTable()
	if !containsRoute(rts, nic2Sub1Rt) {
		t.Fatalf("missing route = %s from route table, got = %s", nic2Sub1Rt, rts)
	}
	// Should still have the route from before.
	if !containsRoute(rts, nic1Sub1Rt) {
		t.Fatalf("missing route = %s from route table, got = %s", nic1Sub1Rt, rts)
	}

	// Test discovering another on-link prefix on eth2.
	accept = ndpDisp.OnOnLinkPrefixDiscovered(ifs2.nicid, subnet2)
	if !accept {
		t.Fatalf("got OnOnLinkPrefixDiscovered(%d, %s) = false, want = true", ifs2.nicid, subnet2)
	}
	waitForEmptyQueue(ndpDisp)
	nic2Sub2Rt := onLinkV6Route(ifs2.nicid, subnet2)
	rts = ns.stack.GetRouteTable()
	if !containsRoute(rts, nic2Sub2Rt) {
		t.Fatalf("missing route = %s from route table, got = %s", nic2Sub2Rt, rts)
	}
	// Should still have the routes from before.
	if !containsRoute(rts, nic2Sub1Rt) {
		t.Fatalf("missing route = %s from route table, got = %s", nic2Sub1Rt, rts)
	}
	if !containsRoute(rts, nic1Sub1Rt) {
		t.Fatalf("missing route = %s from route table, got = %s", nic1Sub1Rt, rts)
	}

	// Invalidate the prefix subnet1 from eth2.
	ndpDisp.OnOnLinkPrefixInvalidated(ifs2.nicid, subnet1)
	waitForEmptyQueue(ndpDisp)
	rts = ns.stack.GetRouteTable()
	if containsRoute(rts, nic2Sub1Rt) {
		t.Fatalf("should not have route = %s in the route table, got = %s", nic2Sub1Rt, rts)
	}
	// Should still have default routes through the non-invalidated
	// routers.
	if !containsRoute(rts, nic2Sub2Rt) {
		t.Fatalf("missing route = %s from route table, got = %s", nic2Sub2Rt, rts)
	}
	if !containsRoute(rts, nic1Sub1Rt) {
		t.Fatalf("missing route = %s from route table, got = %s", nic1Sub1Rt, rts)
	}

	// Invalidate the prefix subnet1 from eth1.
	ndpDisp.OnOnLinkPrefixInvalidated(ifs1.nicid, subnet1)
	waitForEmptyQueue(ndpDisp)
	rts = ns.stack.GetRouteTable()
	if containsRoute(rts, nic1Sub1Rt) {
		t.Fatalf("should not have route = %s in the route table, got = %s", nic1Sub1Rt, rts)
	}
	// Should still not have the other invalidated route.
	if containsRoute(rts, nic2Sub1Rt) {
		t.Fatalf("should not have route = %s in the route table, got = %s", nic2Sub1Rt, rts)
	}
	// Should still have default route through the non-invalidated router.
	if !containsRoute(rts, nic2Sub2Rt) {
		t.Fatalf("missing route = %s from route table, got = %s", nic2Sub2Rt, rts)
	}

	// Invalidate the prefix subnet2 from eth2.
	ndpDisp.OnOnLinkPrefixInvalidated(ifs2.nicid, subnet2)
	waitForEmptyQueue(ndpDisp)
	rts = ns.stack.GetRouteTable()
	if containsRoute(rts, nic2Sub2Rt) {
		t.Fatalf("should not have route = %s in the route table, got = %s", nic2Sub2Rt, rts)
	}
	// Should still not have the other invalidated route.
	if containsRoute(rts, nic1Sub1Rt) {
		t.Fatalf("should not have route = %s in the route table, got = %s", nic1Sub1Rt, rts)
	}
	if containsRoute(rts, nic2Sub1Rt) {
		t.Fatalf("should not have route = %s in the route table, got = %s", nic2Sub1Rt, rts)
	}
}

// TestLinkDown tests that Recursive DNS Servers learned from NDP are
// invalidated when a NIC is brought down.
func TestLinkDown(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	ndpDisp := newNDPDispatcherForTest()
	ns, _ := newNetstackWithNDPDispatcher(t, ndpDisp)
	ndpDisp.start(ctx)

	ifs1 := addNoopEndpoint(t, ns, "")
	t.Cleanup(ifs1.Remove)
	if err := ifs1.Up(); err != nil {
		t.Fatalf("ifs1.Up(): %s", err)
	}
	ifs2 := addNoopEndpoint(t, ns, "")
	t.Cleanup(ifs2.Remove)
	if err := ifs2.Up(); err != nil {
		t.Fatalf("ifs2.Up(): %s", err)
	}

	addr1NIC1 := tcpip.FullAddress{
		Addr: "\xfe\x80\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01",
		Port: 53,
		NIC:  ifs1.nicid,
	}
	addr1NIC2 := tcpip.FullAddress{
		Addr: "\xfe\x80\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01",
		Port: 53,
		NIC:  ifs2.nicid,
	}
	addr2NIC1 := tcpip.FullAddress{
		Addr: "\xfe\x80\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02",
		Port: 53,
		NIC:  ifs1.nicid,
	}
	addr3NIC2 := tcpip.FullAddress{
		Addr: "\xfe\x80\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x03",
		Port: 53,
		NIC:  ifs2.nicid,
	}

	ndpDisp.OnRecursiveDNSServerOption(ifs1.nicid, []tcpip.Address{addr1NIC1.Addr, addr2NIC1.Addr}, defaultLifetime)
	ndpDisp.OnRecursiveDNSServerOption(ifs2.nicid, []tcpip.Address{addr1NIC2.Addr, addr3NIC2.Addr}, defaultLifetime)
	waitForEmptyQueue(ndpDisp)
	{
		want := []tcpip.FullAddress{addr1NIC1, addr2NIC1, addr1NIC2, addr3NIC2}
		got := ns.dnsConfig.GetServersCache()
		if diff := cmp.Diff(want, got, dnsServerTcpIpFullAddressOpts...); diff != "" {
			t.Fatalf("GetServerCache() mismatch (-want +got):\n%s", diff)
		}
	}

	// Bring eth2 down and make sure the DNS servers learned from that NIC are
	// invalidated.
	if err := ifs2.Down(); err != nil {
		t.Fatalf("ifs2.Down(): %s", err)
	}
	waitForEmptyQueue(ndpDisp)
	{
		want := []tcpip.FullAddress{addr1NIC1, addr2NIC1}
		got := ns.dnsConfig.GetServersCache()
		if diff := cmp.Diff(want, got, dnsServerTcpIpFullAddressOpts...); diff != "" {
			t.Fatalf("GetServerCache() mismatch (-want +got):\n%s", diff)
		}
	}

	// Bring eth2 up and make sure the DNS servers learned from that NIC do not
	// reappear.
	if err := ifs2.Up(); err != nil {
		t.Fatalf("ifs2.Up(): %s", err)
	}
	waitForEmptyQueue(ndpDisp)
	{
		want := []tcpip.FullAddress{addr1NIC1, addr2NIC1}
		got := ns.dnsConfig.GetServersCache()
		if diff := cmp.Diff(want, got, dnsServerTcpIpFullAddressOpts...); diff != "" {
			t.Errorf("GetServerCache() mismatch (-want +got):\n%s", diff)
		}
	}
}

var dnsServerTcpIpFullAddressOpts = []cmp.Option{
	// Asymmetric application of transformers is not supported in go-cmp. IOW we
	// write this asymmetric transformer of dns.Server -> tcpip.FullAddress as a
	// symmetric transformer that handles both input types; go-cmp internally
	// upcasts invariant operands to interface{}.
	cmp.FilterValues(func(x, y interface{}) bool {
		for _, v := range []interface{}{x, y} {
			switch v.(type) {
			case []dns.Server:
			case []tcpip.FullAddress:
			default:
				return false
			}
		}
		return true
	}, cmp.Transformer("ToTcpIpAddress", func(v interface{}) []tcpip.FullAddress {
		switch v := v.(type) {
		case []dns.Server:
			if v == nil {
				return nil
			}
			out := make([]tcpip.FullAddress, len(v))
			for i := range v {
				out[i] = v[i].Address
			}
			return out
		case []tcpip.FullAddress:
			return v
		default:
			panic(fmt.Sprintf("value of unexpected type %#v", v))
		}
	})),
	cmpopts.SortSlices(func(left, right tcpip.FullAddress) bool {
		if left, right := left.NIC, right.NIC; left != right {
			return left < right
		}
		if left, right := left.Addr, right.Addr; left != right {
			return left < right
		}
		if left, right := left.Port, right.Port; left != right {
			return left < right
		}
		return false
	}),
}

func TestRecursiveDNSServers(t *testing.T) {
	t.Parallel()

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	ndpDisp := newNDPDispatcherForTest()
	ns, clock := newNetstackWithNDPDispatcher(t, ndpDisp)
	ndpDisp.start(ctx)

	ifs1 := addNoopEndpoint(t, ns, "")
	t.Cleanup(ifs1.Remove)
	if err := ifs1.Up(); err != nil {
		t.Fatalf("ifs1.Up(): %s", err)
	}
	ifs2 := addNoopEndpoint(t, ns, "")
	t.Cleanup(ifs2.Remove)
	if err := ifs2.Up(); err != nil {
		t.Fatalf("ifs2.Up(): %s", err)
	}

	addr1NIC1 := tcpip.FullAddress{
		Addr: "\xfe\x80\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01",
		Port: 53,
		NIC:  ifs1.nicid,
	}
	addr1NIC2 := tcpip.FullAddress{
		Addr: "\xfe\x80\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01",
		Port: 53,
		NIC:  ifs2.nicid,
	}
	addr2NIC1 := tcpip.FullAddress{
		Addr: "\xfe\x80\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02",
		Port: 53,
		NIC:  ifs1.nicid,
	}
	addr3NIC2 := tcpip.FullAddress{
		Addr: "\xfe\x80\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x03",
		Port: 53,
		NIC:  ifs2.nicid,
	}

	ndpDisp.OnRecursiveDNSServerOption(ifs1.nicid, []tcpip.Address{addr1NIC1.Addr, addr2NIC1.Addr}, 0)
	waitForEmptyQueue(ndpDisp)
	{
		want := []tcpip.FullAddress(nil)
		got := ns.dnsConfig.GetServersCache()
		if diff := cmp.Diff(want, got, dnsServerTcpIpFullAddressOpts...); diff != "" {
			t.Errorf("GetServerCache() mismatch (-want +got):\n%s", diff)
		}
	}

	ndpDisp.OnRecursiveDNSServerOption(ifs1.nicid, []tcpip.Address{addr1NIC1.Addr, addr2NIC1.Addr}, defaultLifetime)
	waitForEmptyQueue(ndpDisp)
	{
		want := []tcpip.FullAddress{addr1NIC1, addr2NIC1}
		got := ns.dnsConfig.GetServersCache()
		if diff := cmp.Diff(want, got, dnsServerTcpIpFullAddressOpts...); diff != "" {
			t.Errorf("GetServerCache() mismatch (-want +got):\n%s", diff)
		}
	}

	ndpDisp.OnRecursiveDNSServerOption(ifs2.nicid, []tcpip.Address{addr1NIC2.Addr, addr3NIC2.Addr}, defaultLifetime)
	waitForEmptyQueue(ndpDisp)
	{
		want := []tcpip.FullAddress{addr1NIC1, addr2NIC1, addr1NIC2, addr3NIC2}
		got := ns.dnsConfig.GetServersCache()
		if diff := cmp.Diff(want, got, dnsServerTcpIpFullAddressOpts...); diff != "" {
			t.Errorf("GetServerCache() mismatch (-want +got):\n%s", diff)
		}
	}

	ndpDisp.OnRecursiveDNSServerOption(ifs2.nicid, []tcpip.Address{addr1NIC2.Addr, addr3NIC2.Addr}, 0)
	waitForEmptyQueue(ndpDisp)
	{
		want := []tcpip.FullAddress{addr1NIC1, addr2NIC1}
		got := ns.dnsConfig.GetServersCache()
		if diff := cmp.Diff(want, got, dnsServerTcpIpFullAddressOpts...); diff != "" {
			t.Errorf("GetServerCache() mismatch (-want +got):\n%s", diff)
		}
	}

	ndpDisp.OnRecursiveDNSServerOption(ifs1.nicid, []tcpip.Address{addr1NIC1.Addr, addr2NIC1.Addr}, shortLifetime)
	waitForEmptyQueue(ndpDisp)
	for elapsedTime := time.Duration(0); elapsedTime <= shortLifetimeTimeout; elapsedTime += incrementalTimeout {
		clock.Advance(incrementalTimeout)
		want := []tcpip.FullAddress(nil)
		got := ns.dnsConfig.GetServersCache()
		if diff := cmp.Diff(want, got, dnsServerTcpIpFullAddressOpts...); diff != "" {
			if elapsedTime < shortLifetimeTimeout {
				continue
			}

			t.Errorf("GetServerCache() mismatch (-want +got):\n%s", diff)
		}

		break
	}
}

func TestRecursiveDNSServersWithInfiniteLifetime(t *testing.T) {
	const newInfiniteLifetime = time.Second
	const newInfiniteLifetimeTimeout = 2 * time.Second
	saved := header.NDPInfiniteLifetime
	defer func() { header.NDPInfiniteLifetime = saved }()
	header.NDPInfiniteLifetime = newInfiniteLifetime

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	ndpDisp := newNDPDispatcherForTest()
	ns, clock := newNetstackWithNDPDispatcher(t, ndpDisp)
	ndpDisp.start(ctx)

	ifs := addNoopEndpoint(t, ns, "")
	t.Cleanup(ifs.Remove)
	if err := ifs.Up(); err != nil {
		t.Fatalf("ifs.Up(): %s", err)
	}

	addr1 := tcpip.FullAddress{
		Addr: "\xfe\x80\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01",
		Port: 53,
		NIC:  ifs.nicid,
	}
	addr2 := tcpip.FullAddress{
		Addr: "\xfe\x80\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02",
		Port: 53,
		NIC:  ifs.nicid,
	}
	addr3 := tcpip.FullAddress{
		Addr: "\xfe\x80\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x03",
		Port: 53,
		NIC:  ifs.nicid,
	}
	ndpDisp.OnRecursiveDNSServerOption(ifs.nicid, []tcpip.Address{addr1.Addr, addr2.Addr, addr3.Addr}, newInfiniteLifetime)
	waitForEmptyQueue(ndpDisp)
	{
		want := []tcpip.FullAddress{addr1, addr2, addr3}
		got := ns.dnsConfig.GetServersCache()
		if diff := cmp.Diff(want, got, dnsServerTcpIpFullAddressOpts...); diff != "" {
			t.Fatalf("GetServerCache() mismatch (-want +got):\n%s", diff)
		}
	}

	// All addresses to expire after middleLifetime.
	ndpDisp.OnRecursiveDNSServerOption(ifs.nicid, []tcpip.Address{addr1.Addr, addr2.Addr, addr3.Addr}, middleLifetime)
	// Update addr2 and addr3 to be valid forever.
	ndpDisp.OnRecursiveDNSServerOption(ifs.nicid, []tcpip.Address{addr2.Addr, addr3.Addr}, newInfiniteLifetime)
	waitForEmptyQueue(ndpDisp)
	for elapsedTime := time.Duration(0); elapsedTime <= middleLifetimeTimeout; elapsedTime += incrementalTimeout {
		clock.Advance(incrementalTimeout)
		want := []tcpip.FullAddress{addr2, addr3}
		got := ns.dnsConfig.GetServersCache()
		if diff := cmp.Diff(want, got, dnsServerTcpIpFullAddressOpts...); diff != "" {
			if elapsedTime < middleLifetimeTimeout {
				continue
			}

			t.Fatalf("GetServerCache() mismatch (-want +got):\n%s", diff)
		}

		break
	}

	// addr2 and addr3 should not expire after newInfiniteLifetime (since it
	// represents infinity).
	for elapsedTime := time.Duration(0); elapsedTime <= newInfiniteLifetimeTimeout; elapsedTime += incrementalTimeout {
		clock.Advance(incrementalTimeout)
		want := []tcpip.FullAddress{addr2, addr3}
		got := ns.dnsConfig.GetServersCache()
		if diff := cmp.Diff(want, got, dnsServerTcpIpFullAddressOpts...); diff != "" {
			t.Fatalf("GetServerCache() mismatch (-want +got):\n%s", diff)
		}
	}
}

func TestDHCPv6Stats(t *testing.T) {
	type statsSnapshot struct {
		NoConfiguration    uint64
		ManagedAddress     uint64
		OtherConfiguration uint64
	}

	type step struct {
		run  func(*ndpDispatcher)
		want statsSnapshot
	}

	getSnapshot := func(ns *Netstack) statsSnapshot {
		d := ns.stats.DHCPv6
		return statsSnapshot{
			NoConfiguration:    d.NoConfiguration.Value(),
			ManagedAddress:     d.ManagedAddress.Value(),
			OtherConfiguration: d.OtherConfiguration.Value(),
		}
	}

	tests := []struct {
		name  string
		steps []step
	}{
		{
			name: "one configuration",
			steps: []step{
				{
					run: func(ndpDisp *ndpDispatcher) {
						ndpDisp.OnDHCPv6Configuration(0, ipv6.DHCPv6NoConfiguration)
					},
					want: statsSnapshot{
						NoConfiguration:    1,
						ManagedAddress:     0,
						OtherConfiguration: 0,
					},
				},
			},
		},
		{
			name: "multiple configurations",
			steps: []step{
				{
					run: func(ndpDisp *ndpDispatcher) {
						ndpDisp.OnDHCPv6Configuration(0, ipv6.DHCPv6NoConfiguration)
						ndpDisp.OnDHCPv6Configuration(0, ipv6.DHCPv6ManagedAddress)
					},
					want: statsSnapshot{
						NoConfiguration:    1,
						ManagedAddress:     1,
						OtherConfiguration: 0,
					},
				},
			},
		},
		{
			name: "pull between configurations",
			steps: []step{
				{
					run: func(ndpDisp *ndpDispatcher) {
						ndpDisp.OnDHCPv6Configuration(0, ipv6.DHCPv6NoConfiguration)
					},
					want: statsSnapshot{
						NoConfiguration:    1,
						ManagedAddress:     0,
						OtherConfiguration: 0,
					},
				},
				{
					run: func(ndpDisp *ndpDispatcher) {
						ndpDisp.OnDHCPv6Configuration(0, ipv6.DHCPv6NoConfiguration)
					},
					want: statsSnapshot{
						NoConfiguration:    2,
						ManagedAddress:     0,
						OtherConfiguration: 0,
					},
				},
			},
		},
		{
			name: "duplicated configurations",
			steps: []step{
				{
					run: func(ndpDisp *ndpDispatcher) {
						ndpDisp.OnDHCPv6Configuration(0, ipv6.DHCPv6NoConfiguration)
						ndpDisp.OnDHCPv6Configuration(0, ipv6.DHCPv6NoConfiguration)
					},
					want: statsSnapshot{
						NoConfiguration:    2,
						ManagedAddress:     0,
						OtherConfiguration: 0,
					},
				},
			},
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			ns := &Netstack{stack: stack.New(stack.Options{Clock: &faketime.NullClock{}})}
			ndpDisp := newNDPDispatcher()
			ndpDisp.ns = ns
			ndpDisp.dynamicAddressSourceTracker.init(ns)

			for i, step := range test.steps {
				step.run(ndpDisp)
				if diff := cmp.Diff(step.want, getSnapshot(ns)); diff != "" {
					t.Errorf("%d-th step: mismatch (-want +got):\n%s", i, diff)
				}
			}
		})
	}
}

func TestIPv6AddressConfigTracker(t *testing.T) {
	const (
		nicID1 = 1
		nicID2 = 2
	)

	linkLocalAddr := tcpip.AddressWithPrefix{
		Address:   util.Parse("fe80::1"),
		PrefixLen: 64,
	}

	type statsSnapshot struct {
		NoGlobalSLAACOrDHCPv6ManagedAddress uint64
		GlobalSLAACOnly                     uint64
		DHCPv6ManagedAddressOnly            uint64
		GlobalSLAACAndDHCPv6ManagedAddress  uint64
	}

	type step struct {
		run  func(*ndpDispatcher, *faketime.ManualClock)
		want statsSnapshot
	}

	getSnapshot := func(ns *Netstack) statsSnapshot {
		c := ns.stats.IPv6AddressConfig
		return statsSnapshot{
			NoGlobalSLAACOrDHCPv6ManagedAddress: c.NoGlobalSLAACOrDHCPv6ManagedAddress.Value(),
			GlobalSLAACOnly:                     c.GlobalSLAACOnly.Value(),
			DHCPv6ManagedAddressOnly:            c.DHCPv6ManagedAddressOnly.Value(),
			GlobalSLAACAndDHCPv6ManagedAddress:  c.GlobalSLAACAndDHCPv6ManagedAddress.Value(),
		}
	}

	tests := []struct {
		name  string
		steps []step
	}{
		{
			name: "dynamic address config with no config",
			steps: []step{
				{
					run: func(ndpDisp *ndpDispatcher, clock *faketime.ManualClock) {
						clock.Advance(ipv6AddressConfigTrackerInitialDelay)
					},
					want: statsSnapshot{
						NoGlobalSLAACOrDHCPv6ManagedAddress: 0,
						GlobalSLAACOnly:                     0,
						DHCPv6ManagedAddressOnly:            0,
						GlobalSLAACAndDHCPv6ManagedAddress:  0,
					},
				},
				{
					run: func(ndpDisp *ndpDispatcher, clock *faketime.ManualClock) {
						// Link local addresses should not increment global SLAAC address
						// count.
						ndpDisp.OnAutoGenAddress(nicID1, linkLocalAddr)
						clock.Advance(ipv6AddressConfigTrackerInterval)
					},
					want: statsSnapshot{
						NoGlobalSLAACOrDHCPv6ManagedAddress: 0,
						GlobalSLAACOnly:                     0,
						DHCPv6ManagedAddressOnly:            0,
						GlobalSLAACAndDHCPv6ManagedAddress:  0,
					},
				},
				{
					run: func(ndpDisp *ndpDispatcher, clock *faketime.ManualClock) {
						ndpDisp.OnDHCPv6Configuration(nicID1, ipv6.DHCPv6NoConfiguration)
						clock.Advance(ipv6AddressConfigTrackerInterval)
					},
					want: statsSnapshot{
						NoGlobalSLAACOrDHCPv6ManagedAddress: 1,
						GlobalSLAACOnly:                     0,
						DHCPv6ManagedAddressOnly:            0,
						GlobalSLAACAndDHCPv6ManagedAddress:  0,
					},
				},
				{
					run: func(ndpDisp *ndpDispatcher, clock *faketime.ManualClock) {
						ndpDisp.OnDHCPv6Configuration(nicID1, ipv6.DHCPv6OtherConfigurations)
						clock.Advance(ipv6AddressConfigTrackerInterval)
					},
					want: statsSnapshot{
						NoGlobalSLAACOrDHCPv6ManagedAddress: 2,
						GlobalSLAACOnly:                     0,
						DHCPv6ManagedAddressOnly:            0,
						GlobalSLAACAndDHCPv6ManagedAddress:  0,
					},
				},
				{
					run: func(ndpDisp *ndpDispatcher, clock *faketime.ManualClock) {
						ndpDisp.OnAutoGenAddress(nicID1, testProtocolAddr1.AddressWithPrefix)
						ndpDisp.OnAutoGenAddressInvalidated(nicID1, testProtocolAddr1.AddressWithPrefix)
						clock.Advance(ipv6AddressConfigTrackerInterval)
					},
					want: statsSnapshot{
						NoGlobalSLAACOrDHCPv6ManagedAddress: 3,
						GlobalSLAACOnly:                     0,
						DHCPv6ManagedAddressOnly:            0,
						GlobalSLAACAndDHCPv6ManagedAddress:  0,
					},
				},
			},
		},
		{
			name: "dynamic address config with slaac only",
			steps: []step{
				{
					run: func(ndpDisp *ndpDispatcher, clock *faketime.ManualClock) {
						ndpDisp.OnAutoGenAddress(nicID1, testProtocolAddr1.AddressWithPrefix)
						clock.Advance(ipv6AddressConfigTrackerInitialDelay)
					},
					want: statsSnapshot{
						NoGlobalSLAACOrDHCPv6ManagedAddress: 0,
						GlobalSLAACOnly:                     1,
						DHCPv6ManagedAddressOnly:            0,
						GlobalSLAACAndDHCPv6ManagedAddress:  0,
					},
				},
				{
					run: func(ndpDisp *ndpDispatcher, clock *faketime.ManualClock) {
						// Link local addresses should not decrement global SLAAC address
						// count.
						ndpDisp.OnAutoGenAddressInvalidated(nicID1, linkLocalAddr)
						clock.Advance(ipv6AddressConfigTrackerInterval)
					},
					want: statsSnapshot{
						NoGlobalSLAACOrDHCPv6ManagedAddress: 0,
						GlobalSLAACOnly:                     2,
						DHCPv6ManagedAddressOnly:            0,
						GlobalSLAACAndDHCPv6ManagedAddress:  0,
					},
				},
				{
					run: func(ndpDisp *ndpDispatcher, clock *faketime.ManualClock) {
						ndpDisp.OnAutoGenAddress(nicID1, testProtocolAddr2.AddressWithPrefix)
						clock.Advance(ipv6AddressConfigTrackerInterval)
					},
					want: statsSnapshot{
						NoGlobalSLAACOrDHCPv6ManagedAddress: 0,
						GlobalSLAACOnly:                     3,
						DHCPv6ManagedAddressOnly:            0,
						GlobalSLAACAndDHCPv6ManagedAddress:  0,
					},
				},
				{
					run: func(ndpDisp *ndpDispatcher, clock *faketime.ManualClock) {
						ndpDisp.OnAutoGenAddressInvalidated(nicID1, testProtocolAddr2.AddressWithPrefix)
						clock.Advance(ipv6AddressConfigTrackerInterval)
					},
					want: statsSnapshot{
						NoGlobalSLAACOrDHCPv6ManagedAddress: 0,
						GlobalSLAACOnly:                     4,
						DHCPv6ManagedAddressOnly:            0,
						GlobalSLAACAndDHCPv6ManagedAddress:  0,
					},
				},
			},
		},
		{
			name: "dynamic address config with dhcpv6 only",
			steps: []step{
				{
					run: func(ndpDisp *ndpDispatcher, clock *faketime.ManualClock) {
						ndpDisp.OnDHCPv6Configuration(nicID1, ipv6.DHCPv6ManagedAddress)
						clock.Advance(ipv6AddressConfigTrackerInitialDelay)
					},
					want: statsSnapshot{
						NoGlobalSLAACOrDHCPv6ManagedAddress: 0,
						GlobalSLAACOnly:                     0,
						DHCPv6ManagedAddressOnly:            1,
						GlobalSLAACAndDHCPv6ManagedAddress:  0,
					},
				},
				// Only the last configuration should be used.
				{
					run: func(ndpDisp *ndpDispatcher, clock *faketime.ManualClock) {
						ndpDisp.OnDHCPv6Configuration(nicID1, ipv6.DHCPv6NoConfiguration)
						ndpDisp.OnDHCPv6Configuration(nicID1, ipv6.DHCPv6ManagedAddress)
						clock.Advance(ipv6AddressConfigTrackerInterval)
					},
					want: statsSnapshot{
						NoGlobalSLAACOrDHCPv6ManagedAddress: 0,
						GlobalSLAACOnly:                     0,
						DHCPv6ManagedAddressOnly:            2,
						GlobalSLAACAndDHCPv6ManagedAddress:  0,
					},
				},
			},
		},
		{
			name: "dynamic address config with dhcpv6 and slaac",
			steps: []step{
				{
					run: func(ndpDisp *ndpDispatcher, clock *faketime.ManualClock) {
						ndpDisp.OnDHCPv6Configuration(nicID1, ipv6.DHCPv6ManagedAddress)
						ndpDisp.OnAutoGenAddress(nicID1, testProtocolAddr2.AddressWithPrefix)
						clock.Advance(ipv6AddressConfigTrackerInitialDelay)
					},
					want: statsSnapshot{
						NoGlobalSLAACOrDHCPv6ManagedAddress: 0,
						GlobalSLAACOnly:                     0,
						DHCPv6ManagedAddressOnly:            0,
						GlobalSLAACAndDHCPv6ManagedAddress:  1,
					},
				},
			},
		},
		{
			name: "dynamic address config with dhcpv6 and slaac on different NICs",
			steps: []step{
				{
					run: func(ndpDisp *ndpDispatcher, clock *faketime.ManualClock) {
						ndpDisp.OnDHCPv6Configuration(nicID1, ipv6.DHCPv6ManagedAddress)
						ndpDisp.OnAutoGenAddress(nicID2, testProtocolAddr2.AddressWithPrefix)
						clock.Advance(ipv6AddressConfigTrackerInitialDelay)
					},
					want: statsSnapshot{
						NoGlobalSLAACOrDHCPv6ManagedAddress: 0,
						GlobalSLAACOnly:                     1,
						DHCPv6ManagedAddressOnly:            1,
						GlobalSLAACAndDHCPv6ManagedAddress:  0,
					},
				},
				{
					run: func(ndpDisp *ndpDispatcher, clock *faketime.ManualClock) {
						ndpDisp.dynamicAddressSourceTracker.RemovedNIC(nicID1)
						clock.Advance(ipv6AddressConfigTrackerInterval)
					},
					want: statsSnapshot{
						NoGlobalSLAACOrDHCPv6ManagedAddress: 0,
						GlobalSLAACOnly:                     2,
						DHCPv6ManagedAddressOnly:            1,
						GlobalSLAACAndDHCPv6ManagedAddress:  0,
					},
				},
				{
					run: func(ndpDisp *ndpDispatcher, clock *faketime.ManualClock) {
						ndpDisp.dynamicAddressSourceTracker.RemovedNIC(nicID2)
						clock.Advance(ipv6AddressConfigTrackerInterval)
					},
					want: statsSnapshot{
						NoGlobalSLAACOrDHCPv6ManagedAddress: 0,
						GlobalSLAACOnly:                     2,
						DHCPv6ManagedAddressOnly:            1,
						GlobalSLAACAndDHCPv6ManagedAddress:  0,
					},
				},
			},
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			clock := faketime.NewManualClock()
			ns := &Netstack{stack: stack.New(stack.Options{Clock: clock})}
			ndpDisp := newNDPDispatcher()
			ndpDisp.ns = ns
			ndpDisp.dynamicAddressSourceTracker.init(ns)

			for i, step := range test.steps {
				step.run(ndpDisp, clock)
				if diff := cmp.Diff(step.want, getSnapshot(ns)); diff != "" {
					t.Errorf("%d-th step: mismatch (-want +got):\n%s", i, diff)
				}
			}
		})
	}
}
