// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// +build !build_with_native_toolchain

package netstack

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"math"
	"net"
	"os"
	"sort"
	"sync/atomic"
	"syscall/zx"
	"testing"
	"time"

	"fidl/fuchsia/hardware/ethernet"
	"fidl/fuchsia/io"
	fidlnet "fidl/fuchsia/net"
	"fidl/fuchsia/net/interfaces"
	"fidl/fuchsia/net/stack"
	"fidl/fuchsia/netstack"

	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/dhcp"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/dns"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/fidlconv"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/filter"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/link/fifo/testutil"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/routes"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/util"
	"go.fuchsia.dev/fuchsia/src/lib/component"
	syslog "go.fuchsia.dev/fuchsia/src/lib/syslog/go"

	"github.com/google/go-cmp/cmp"
	"gvisor.dev/gvisor/pkg/tcpip"
	"gvisor.dev/gvisor/pkg/tcpip/faketime"
	"gvisor.dev/gvisor/pkg/tcpip/header"
	"gvisor.dev/gvisor/pkg/tcpip/link/sniffer"
	"gvisor.dev/gvisor/pkg/tcpip/network/arp"
	"gvisor.dev/gvisor/pkg/tcpip/network/ipv4"
	"gvisor.dev/gvisor/pkg/tcpip/network/ipv6"
	tcpipstack "gvisor.dev/gvisor/pkg/tcpip/stack"
	"gvisor.dev/gvisor/pkg/tcpip/transport/tcp"
	"gvisor.dev/gvisor/pkg/tcpip/transport/udp"
	"gvisor.dev/gvisor/pkg/waiter"
)

const (
	testTopoPath         string        = "/fake/ethernet/device"
	testV4Address        tcpip.Address = "\xc0\xa8\x2a\x10"
	testV6Address        tcpip.Address = "\xc0\xa8\x2a\x10\xc0\xa8\x2a\x10\xc0\xa8\x2a\x10\xc0\xa8\x2a\x10"
	testLinkLocalV6Addr1 tcpip.Address = "\xfe\x80\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01"
	testLinkLocalV6Addr2 tcpip.Address = "\xfe\x80\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02"
	dadResolutionTimeout               = dadRetransmitTimer*dadTransmits + time.Second
)

func TestMain(m *testing.M) {
	flag.Parse()
	if testing.Verbose() {
		appCtx := component.NewContextFromStartupInfo()
		s, err := syslog.ConnectToLogger(appCtx.Connector())
		if err != nil {
			panic(fmt.Sprintf("syslog.ConnectToLogger() = %s", err))
		}
		options := syslog.LogInitOptions{
			LogLevel:                      syslog.AllLevel,
			MinSeverityForFileAndLineInfo: syslog.AllLevel,
			Socket:                        s,
		}
		l, err := syslog.NewLogger(options)
		if err != nil {
			panic(fmt.Sprintf("syslog.NewLogger(%#v) = %s", options, err))
		}
		syslog.SetDefaultLogger(l)

		// As of this writing we set this value to 0 in netstack/main.go.
		atomic.StoreUint32(&sniffer.LogPackets, 1)
	}

	os.Exit(m.Run())
}

func TestDelRouteErrors(t *testing.T) {
	ns, _ := newNetstack(t)

	ifs := addNoopEndpoint(t, ns, "")
	t.Cleanup(ifs.Remove)

	rt := tcpip.Route{
		Destination: header.IPv4EmptySubnet,
		Gateway:     "\x01\x02\x03\x04",
		NIC:         ifs.nicid,
	}

	// Deleting a route we never added should result in an error.
	if err := ns.DelRoute(rt); err != routes.ErrNoSuchRoute {
		t.Errorf("got DelRoute(%s) = %v, want = %s", rt, err, routes.ErrNoSuchRoute)
	}

	if err := ns.AddRoute(rt, metricNotSet, false); err != nil {
		t.Fatalf("AddRoute(%s, metricNotSet, false): %s", rt, err)
	}
	// Deleting a route we added should not result in an error.
	if err := ns.DelRoute(rt); err != nil {
		t.Fatalf("got DelRoute(%s) = %s, want = nil", rt, err)
	}
	// Deleting a route we just deleted should result in an error.
	if err := ns.DelRoute(rt); err != routes.ErrNoSuchRoute {
		t.Errorf("got DelRoute(%s) = %v, want = %s", rt, err, routes.ErrNoSuchRoute)
	}
}

// TestStackNICEnableDisable tests that the NIC in stack.Stack is enabled or
// disabled when the underlying link is brought up or down, respectively.
func TestStackNICEnableDisable(t *testing.T) {
	ns, _ := newNetstack(t)
	ifs := addNoopEndpoint(t, ns, "")
	t.Cleanup(ifs.Remove)

	// The NIC should initially be disabled in stack.Stack.
	if enabled := ns.stack.CheckNIC(ifs.nicid); enabled {
		t.Fatalf("got ns.stack.CheckNIC(%d) = true, want = false", ifs.nicid)
	}

	isUp := func() bool {
		ifs.mu.Lock()
		defer ifs.mu.Unlock()
		return ifs.IsUpLocked()
	}

	if isUp() {
		t.Fatal("got initial link state up, want down")
	}

	// Bringing the link up should enable the NIC in stack.Stack.
	if err := ifs.Up(); err != nil {
		t.Fatal("ifs.Up(): ", err)
	}
	if enabled := ns.stack.CheckNIC(ifs.nicid); !enabled {
		t.Fatalf("got ns.stack.CheckNIC(%d) = false, want = true", ifs.nicid)
	}

	if !isUp() {
		t.Fatal("got post-up link state down, want up")
	}

	// Bringing the link down should disable the NIC in stack.Stack.
	if err := ifs.Down(); err != nil {
		t.Fatal("ifs.Down(): ", err)
	}
	if enabled := ns.stack.CheckNIC(ifs.nicid); enabled {
		t.Fatalf("got ns.stack.CheckNIC(%d) = true, want = false", ifs.nicid)
	}

	if isUp() {
		t.Fatal("got post-down link state up, want down")
	}
}

var _ NICRemovedHandler = (*testNicRemovedHandler)(nil)

type testNicRemovedHandler struct {
	removedNICID tcpip.NICID
}

func (h *testNicRemovedHandler) RemovedNIC(nicID tcpip.NICID) {
	h.removedNICID = nicID
}

// TestStackNICRemove tests that the NIC in stack.Stack is removed when the
// underlying link is closed.
func TestStackNICRemove(t *testing.T) {
	nicRemovedHandler := testNicRemovedHandler{}
	ns, _ := newNetstackWithNicRemovedHandler(t, &nicRemovedHandler)
	var obs noopObserver

	ifs, err := ns.addEndpoint(
		func(tcpip.NICID) string { return t.Name() },
		&noopEndpoint{},
		&noopController{},
		&obs,
		0, /* metric */
	)
	if err != nil {
		t.Fatal(err)
	}

	// The NIC should initially be disabled in stack.Stack.
	if enabled := ns.stack.CheckNIC(ifs.nicid); enabled {
		t.Errorf("got ns.stack.CheckNIC(%d) = true, want = false", ifs.nicid)
	}
	if _, ok := ns.stack.NICInfo()[ifs.nicid]; !ok {
		t.Errorf("missing NICInfo for NIC %d", ifs.nicid)
	}
	if _, err := ns.stack.GetMainNICAddress(ifs.nicid, header.IPv6ProtocolNumber); err != nil {
		t.Errorf("GetMainNICAddress(%d, header.IPv6ProtocolNumber): %s", ifs.nicid, err)
	}

	nicName := ns.stack.FindNICNameFromID(ifs.nicid)
	if disabled := ns.filter.IsInterfaceDisabled(nicName); !disabled {
		t.Errorf("got ns.filter.IsInterfaceDisabled(%s) = false, want = true", nicName)
	}
	ns.filter.EnableInterface(ifs.nicid)
	if disabled := ns.filter.IsInterfaceDisabled(nicName); disabled {
		t.Errorf("got ns.filter.IsInterfaceDisabled(%s) = true, want = false", nicName)
	}

	if t.Failed() {
		t.FailNow()
	}

	// Closing the link should remove the NIC from stack.Stack.
	obs.onLinkClosed()

	if enabled := ns.stack.CheckNIC(ifs.nicid); enabled {
		t.Errorf("got ns.stack.CheckNIC(%d) = false, want = true", ifs.nicid)
	}
	if nicInfo, ok := ns.stack.NICInfo()[ifs.nicid]; ok {
		t.Errorf("unexpected NICInfo found for NIC %d = %+v", ifs.nicid, nicInfo)
	}
	{
		addr, err := ns.stack.GetMainNICAddress(ifs.nicid, header.IPv6ProtocolNumber)
		if _, ok := err.(*tcpip.ErrUnknownNICID); !ok {
			t.Errorf("got GetMainNICAddress(%d, header.IPv6ProtocolNumber) = (%s, %T), want = (_, *tcpip.ErrUnknownNICID)", ifs.nicid, addr, err)
		}
	}
	if nicRemovedHandler.removedNICID != ifs.nicid {
		t.Errorf("got nicRemovedHandler.removedNICID = %d, want = %d", nicRemovedHandler.removedNICID, ifs.nicid)
	}

	// Removing the NIC should disable the filter on its nicName.
	if disabled := ns.filter.IsInterfaceDisabled(nicName); !disabled {
		t.Errorf("got ns.filter.IsInterfaceDisabled(%s) = false, want = true", nicName)
	}

	// Wait for the controller to stop and free up its resources.
	ifs.endpoint.Wait()
}

func containsRoute(rs []tcpip.Route, r tcpip.Route) bool {
	for _, i := range rs {
		if i == r {
			return true
		}
	}

	return false
}

func TestEndpoint_Close(t *testing.T) {
	ns, _ := newNetstack(t)
	var wq waiter.Queue
	// Avoid polluting everything with err of type tcpip.Error.
	ep := func() tcpip.Endpoint {
		ep, err := ns.stack.NewEndpoint(tcp.ProtocolNumber, ipv6.ProtocolNumber, &wq)
		if err != nil {
			t.Fatalf("NewEndpoint(tcp.ProtocolNumber, ipv6.ProtocolNumber, _) = %s", err)
		}
		return ep
	}()
	defer ep.Close()

	eps, err := newEndpointWithSocket(ep, &wq, tcp.ProtocolNumber, ipv6.ProtocolNumber, ns)
	if err != nil {
		t.Fatal(err)
	}
	defer eps.close()

	eps.mu.Lock()
	channels := []struct {
		ch   <-chan struct{}
		name string
	}{
		{ch: eps.closing, name: "closing"},
		{ch: eps.mu.loopPollDone, name: "loopPollDone"},
		{ch: eps.mu.loopReadDone, name: "loopReadDone"},
		{ch: eps.mu.loopWriteDone, name: "loopWriteDone"},
	}
	eps.mu.Unlock()

	// Check starting conditions.
	for _, ch := range channels {
		select {
		case <-ch.ch:
			t.Errorf("%s cleaned up prematurely", ch.name)
		default:
		}
	}

	if _, ok := eps.ns.endpoints.Load(eps.endpoint.key); !ok {
		var keys []uint64
		eps.ns.endpoints.Range(func(key uint64, _ tcpip.Endpoint) bool {
			keys = append(keys, key)
			return true
		})
		t.Errorf("got endpoints map = %d at creation, want %d", keys, eps.endpoint.key)
	}

	if t.Failed() {
		t.FailNow()
	}

	// Create a referent.
	s, err := newStreamSocket(eps)
	if err != nil {
		t.Fatalf("newStreamSocket() = %s", err)
	}
	defer func() {
		func() {
			status, err := s.Close(context.Background())
			if err, ok := err.(*zx.Error); ok && err.Status == zx.ErrPeerClosed {
				return
			}
			t.Errorf("s.Close() = (%s, %v)", zx.Status(status), err)
		}()
		if err := s.Channel.Close(); err != nil {
			t.Errorf("s.Channel.Close() = %s", err)
		}
	}()

	// Create another referent.
	localC, peerC, err := zx.NewChannel(0)
	if err != nil {
		t.Fatalf("zx.NewChannel() = %s", err)
	}
	defer func() {
		// Already closed below.
		if err := localC.Close(); err != nil {
			t.Errorf("localC.Close() = %s", err)
		}

		// By-value copy already closed by the server when we closed the peer.
		err := peerC.Close()
		if err, ok := err.(*zx.Error); ok && err.Status == zx.ErrBadHandle {
			return
		}
		t.Errorf("peerC.Close() = %v", err)
	}()

	if err := s.Clone(context.Background(), 0, io.NodeWithCtxInterfaceRequest{Channel: peerC}); err != nil {
		t.Fatalf("s.Clone() = %s", err)
	}

	// Close the original referent.
	if status, err := s.Close(context.Background()); err != nil {
		t.Fatalf("s.Close() = %s", err)
	} else if status := zx.Status(status); status != zx.ErrOk {
		t.Fatalf("s.Close() = %s", status)
	}

	// There's still a referent.
	for _, ch := range channels {
		select {
		case <-ch.ch:
			t.Errorf("%s cleaned up prematurely", ch.name)
		default:
		}
	}

	if _, ok := eps.ns.endpoints.Load(eps.endpoint.key); !ok {
		var keys []uint64
		eps.ns.endpoints.Range(func(key uint64, _ tcpip.Endpoint) bool {
			keys = append(keys, key)
			return true
		})
		t.Errorf("got endpoints map prematurely = %d, want %d", keys, eps.endpoint.key)
	}

	if t.Failed() {
		t.FailNow()
	}

	// Close the last reference.
	if err := localC.Close(); err != nil {
		t.Fatalf("localC.Close() = %s", err)
	}

	// Give a generous timeout for the closed channel to be detected.
	timeout := make(chan struct{})
	time.AfterFunc(5*time.Second, func() { close(timeout) })
	for _, ch := range channels {
		if ch.ch != nil {
			select {
			case <-ch.ch:
			case <-timeout:
				t.Errorf("%s not cleaned up", ch.name)
			}
		}
	}

	for {
		if _, ok := eps.ns.endpoints.Load(eps.endpoint.key); ok {
			select {
			case <-timeout:
				var keys []uint64
				eps.ns.endpoints.Range(func(key uint64, _ tcpip.Endpoint) bool {
					keys = append(keys, key)
					return true
				})
				t.Errorf("got endpoints map = %d after closure, want *not* %d", keys, eps.endpoint.key)
			default:
				continue
			}
		}
		break
	}

	if t.Failed() {
		t.FailNow()
	}
}

// TestTCPEndpointMapAcceptAfterReset tests that an already-reset endpoint
// isn't added to the endpoints map, since such an endpoint wouldn't receive a
// hangup notification and its reference in the map would leak.
func TestTCPEndpointMapAcceptAfterReset(t *testing.T) {
	ns, _ := newNetstack(t)
	if err := ns.addLoopback(); err != nil {
		t.Fatalf("ns.addLoopback() = %s", err)
	}

	listener := createEP(t, ns, new(waiter.Queue))

	if err := listener.ep.Bind(tcpip.FullAddress{}); err != nil {
		t.Fatalf("Bind({}) = %s", err)
	}
	if err := listener.ep.Listen(1); err != nil {
		t.Fatalf("Listen(1) = %s", err)
	}

	client := createEP(t, ns, new(waiter.Queue))

	// Connect and wait for the incoming connection.
	func() {
		connectAddr, err := listener.ep.GetLocalAddress()
		if err != nil {
			t.Fatalf("GetLocalAddress() = %s", err)
		}

		waitEntry, notifyCh := waiter.NewChannelEntry(nil)
		listener.wq.EventRegister(&waitEntry, waiter.EventIn)
		defer listener.wq.EventUnregister(&waitEntry)

		switch err := client.ep.Connect(connectAddr); err.(type) {
		case *tcpip.ErrConnectStarted:
		default:
			t.Fatalf("Connect(%#v) = %s", connectAddr, err)
		}
		<-notifyCh
	}()

	// Initiate a RST of the connection sitting in the accept queue.
	client.ep.SocketOptions().SetLinger(tcpip.LingerOption{
		Enabled: true,
		Timeout: 0,
	})
	client.ep.Close()

	// Wait for the RST to be processed by the stack.
	time.Sleep(100 * time.Millisecond)

	_, _, eps, err := listener.Accept(false)
	if err != nil {
		t.Fatalf("ep.Accept(nil) = %s", err)
	}
	defer eps.close()

	// Expect the `Accept` to have removed the endpoint from the map.
	if _, ok := ns.endpoints.Load(eps.endpoint.key); ok {
		t.Fatalf("got endpoints.Load(%d) = (_, true)", eps.endpoint.key)
	}

	eps.mu.Lock()
	channels := []struct {
		ch   <-chan struct{}
		name string
	}{
		{ch: eps.closing, name: "closing"},
		{ch: eps.mu.loopPollDone, name: "loopPollDone"},
		{ch: eps.mu.loopReadDone, name: "loopReadDone"},
		{ch: eps.mu.loopWriteDone, name: "loopWriteDone"},
	}
	eps.mu.Unlock()

	// Give a generous timeout for the closed channel to be detected.
	timeout := make(chan struct{})
	time.AfterFunc(5*time.Second, func() { close(timeout) })
	for _, ch := range channels {
		if ch.ch != nil {
			select {
			case <-ch.ch:
			case <-timeout:
				t.Errorf("%s not cleaned up", ch.name)
			}
		}
	}
}

func createEP(t *testing.T, ns *Netstack, wq *waiter.Queue) *endpointWithSocket {
	// Avoid polluting the scope with err of type tcpip.Error.
	ep := func() tcpip.Endpoint {
		ep, err := ns.stack.NewEndpoint(tcp.ProtocolNumber, ipv4.ProtocolNumber, wq)
		if err != nil {
			t.Fatalf("NewEndpoint(tcp.ProtocolNumber, ipv4.ProtocolNumber, _) = %s", err)
		}
		return ep
	}()
	t.Cleanup(ep.Close)
	eps, err := newEndpointWithSocket(ep, wq, tcp.ProtocolNumber, ipv4.ProtocolNumber, ns)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(eps.close)
	return eps
}

func TestTCPEndpointMapClose(t *testing.T) {
	ns, _ := newNetstack(t)
	eps := createEP(t, ns, new(waiter.Queue))

	// Closing the endpoint should remove it from the endpoints map.
	if _, ok := ns.endpoints.Load(eps.endpoint.key); !ok {
		t.Fatalf("got endpoints.Load(%d) = (_, false)", eps.endpoint.key)
	}
	eps.close()
	if _, ok := ns.endpoints.Load(eps.endpoint.key); ok {
		t.Fatalf("got endpoints.Load(%d) = (_, true)", eps.endpoint.key)
	}
}

func TestTCPEndpointMapConnect(t *testing.T) {
	ns, clock := newNetstack(t)

	var linkEP tcpipstack.LinkEndpoint = &noopEndpoint{
		capabilities: tcpipstack.CapabilityResolutionRequired,
	}
	if testing.Verbose() {
		linkEP = sniffer.New(linkEP)
	}
	ifs, err := ns.addEndpoint(
		func(tcpip.NICID) string { return t.Name() },
		linkEP,
		nil,
		nil,
		0, /* metric */
	)
	if err != nil {
		t.Fatal(err)
	}
	if err := ns.stack.EnableNIC(ifs.nicid); err != nil {
		t.Fatal(err)
	}

	address := tcpip.Address([]byte{1, 2, 3, 4})
	destination := tcpip.FullAddress{
		Addr: address,
		Port: 1,
	}
	source := tcpip.Address([]byte{5, 6, 7, 8})
	if err := ns.stack.AddAddress(ifs.nicid, ipv4.ProtocolNumber, source); err != nil {
		t.Fatalf("AddAddress(%d, %d, %s) = %s", ifs.nicid, ipv4.ProtocolNumber, source, err)
	}

	ns.stack.SetRouteTable([]tcpip.Route{
		{
			Destination: address.WithPrefix().Subnet(),
			NIC:         ifs.nicid,
		},
	})

	var wq waiter.Queue
	eps := createEP(t, ns, &wq)

	events := make(chan waiter.EventMask)
	waitEntry := waiter.Entry{Callback: callback(func(_ *waiter.Entry, m waiter.EventMask) {
		events <- m
	})}
	wq.EventRegister(&waitEntry, math.MaxUint64)
	defer wq.EventUnregister(&waitEntry)

	switch err := eps.ep.Connect(destination); err.(type) {
	case *tcpip.ErrConnectStarted:
	default:
		t.Fatalf("got Connect(%#v) = %v, want %s", destination, err, &tcpip.ErrConnectStarted{})
	}

	{
		nudConfig, err := ns.stack.NUDConfigurations(ifs.nicid, ipv4.ProtocolNumber)
		if err != nil {
			t.Fatalf("stack.NUDConfigurations(): %s", err)
		}
		clock.Advance(time.Duration(nudConfig.MaxMulticastProbes) * nudConfig.RetransmitTimer)
	}

	if got, want := <-events, waiter.ReadableEvents|waiter.WritableEvents|waiter.EventErr|waiter.EventHUp; got != want {
		t.Fatalf("got event = %b, want %b", got, want)
	}

	// The callback on HUp should have removed the endpoint from the map.
	if _, ok := ns.endpoints.Load(eps.endpoint.key); ok {
		t.Fatalf("got endpoints.Load(%d) = (_, true)", eps.endpoint.key)
	}
}

// TestTCPEndpointMapClosing validates the endpoint in a closing state like
// FIN_WAIT2 to be present in the endpoints map and is deleted when the
// endpoint transitions to CLOSED state.
func TestTCPEndpointMapClosing(t *testing.T) {
	ns, _ := newNetstack(t)
	if err := ns.addLoopback(); err != nil {
		t.Fatalf("ns.addLoopback() = %s", err)
	}
	listener := createEP(t, ns, new(waiter.Queue))

	if err := listener.ep.Bind(tcpip.FullAddress{}); err != nil {
		t.Fatalf("ep.Bind({}) = %s", err)
	}
	if err := listener.ep.Listen(1); err != nil {
		t.Fatalf("ep.Listen(1) = %s", err)
	}
	connectAddr, err := listener.ep.GetLocalAddress()
	if err != nil {
		t.Fatalf("ep.GetLocalAddress() = %s", err)
	}
	client := createEP(t, ns, new(waiter.Queue))

	waitEntry, inCh := waiter.NewChannelEntry(nil)
	listener.wq.EventRegister(&waitEntry, waiter.EventIn)
	defer listener.wq.EventUnregister(&waitEntry)

	switch err := client.ep.Connect(connectAddr); err.(type) {
	case *tcpip.ErrConnectStarted:
	default:
		t.Fatalf("ep.Connect(%#v) = %s", connectAddr, err)
	}
	// Wait for the newly established connection to show up as acceptable by
	// the peer.
	<-inCh

	server, _, err := listener.ep.Accept(nil)
	if err != nil {
		t.Fatalf("ep.Accept(nil) = %s", err)
	}

	// Ensure that the client endpoint is present in our internal map.
	if _, ok := ns.endpoints.Load(client.endpoint.key); !ok {
		t.Fatalf("got endpoints.Load(%d) = (_,false)", client.endpoint.key)
	}

	// Trigger an active close from the client.
	client.close()

	// The client endpoint should not be removed from endpoints map even after
	// an endpoint close.
	if _, ok := ns.endpoints.Load(client.endpoint.key); !ok {
		t.Fatalf("got endpoints.Load(%d) = (_,false)", client.endpoint.key)
	}

	ticker := time.NewTicker(10 * time.Millisecond)
	defer ticker.Stop()
	// Wait and check for the client active close to reach FIN_WAIT2 state.
	for {
		if tcp.EndpointState(client.ep.State()) == tcp.StateFinWait2 {
			break
		}
		<-ticker.C
	}

	// Lookup for the client once more in the endpoints map, it should still not
	// be removed.
	if _, ok := ns.endpoints.Load(client.endpoint.key); !ok {
		t.Fatalf("got endpoints.Load(%d) = (_,false)", client.endpoint.key)
	}

	timeWaitOpt := tcpip.TCPTimeWaitTimeoutOption(time.Duration(0))
	if err := ns.stack.SetTransportProtocolOption(tcp.ProtocolNumber, &timeWaitOpt); err != nil {
		t.Fatalf("SetTransportProtocolOption(%d, &%T(%d)) = %s", tcp.ProtocolNumber, timeWaitOpt, timeWaitOpt, err)
	}

	// Trigger server close, so that client enters TIME_WAIT.
	server.Close()

	// gVisor stack notifies EventHUp on entering TIME_WAIT. Wait for some time
	// for the EventHUp to be processed by netstack.
	for {
		// The client endpoint would be removed from the endpoints map as a result
		// of processing EventHUp.
		if _, ok := ns.endpoints.Load(client.endpoint.key); !ok {
			break
		}
		<-ticker.C
	}
}

func TestEndpointsMapKey(t *testing.T) {
	ns, _ := newNetstack(t)
	if ns.endpoints.nextKey != 0 {
		t.Fatalf("got ns.endpoints.nextKey = %d, want 0", ns.endpoints.nextKey)
	}

	tcpipEP := func() (*waiter.Queue, tcpip.Endpoint) {
		var wq waiter.Queue
		ep, err := ns.stack.NewEndpoint(tcp.ProtocolNumber, ipv6.ProtocolNumber, &wq)
		if err != nil {
			t.Fatalf("NewEndpoint(tcp.ProtocolNumber, ipv6.ProtocolNumber, _) = %s", err)
		}
		t.Cleanup(ep.Close)
		return &wq, ep
	}
	// Test if we always skip key value 0 while adding to the map.
	for _, key := range []uint64{0, math.MaxUint64} {
		wq, ep := tcpipEP()

		// Set a test value to nextKey which is used to compute the endpoint key.
		ns.endpoints.nextKey = key
		eps, err := newEndpointWithSocket(ep, wq, tcp.ProtocolNumber, ipv6.ProtocolNumber, ns)
		if err != nil {
			t.Fatal(err)
		}
		t.Cleanup(eps.close)
		if ns.endpoints.nextKey != 1 {
			t.Fatalf("got ns.endpoints.nextKey = %d, want 1", ns.endpoints.nextKey)
		}
		if eps.endpoint.key != 1 {
			t.Fatalf("got eps.endpoint.key = %d, want 1", eps.endpoint.key)
		}
		if _, ok := ns.endpoints.Load(eps.endpoint.key); !ok {
			t.Fatalf("got endpoints.Load(%d) = (_,false)", eps.endpoint.key)
		}
		// Closing the endpoint should remove the endpoint with key value 1
		// from the endpoints map. This lets the subsequent iteration to reuse
		// key value 1 to add a new endpoint to the map.
		eps.close()
	}

	// Key value 0 is not expected to be removed from the map.
	_, ep := tcpipEP()
	ns.endpoints.Store(0, ep)
	if ns.onRemoveEndpoint(0) {
		t.Errorf("got ns.onRemoveEndpoint(0) = true, want false")
	}
	if _, ok := ns.endpoints.Load(0); !ok {
		t.Fatal("got endpoints.Load(0) = (_,false)")
	}
}

func TestNICName(t *testing.T) {
	ns, _ := newNetstack(t)

	if want, got := "unknown(NICID=0)", ns.name(0); got != want {
		t.Fatalf("got ns.name(0) = %q, want %q", got, want)
	}

	{
		ifs := addNoopEndpoint(t, ns, "")
		t.Cleanup(ifs.Remove)
		if got, want := ifs.ns.name(ifs.nicid), t.Name()+"1"; got != want {
			t.Fatalf("got ifs.mu.name = %q, want = %q", got, want)
		}
	}

	{
		const name = "VerySpecialName"
		ifs := addNoopEndpoint(t, ns, name)
		t.Cleanup(ifs.Remove)
		if got, want := ifs.ns.name(ifs.nicid), name; got != want {
			t.Fatalf("got ifs.mu.name = %q, want = %q", got, want)
		}
	}
}

func TestNotStartedByDefault(t *testing.T) {
	ns, _ := newNetstack(t)

	startCalled := false
	controller := noopController{
		onUp: func() { startCalled = true },
	}
	if _, err := ns.addEndpoint(
		func(tcpip.NICID) string { return t.Name() },
		&noopEndpoint{},
		&controller,
		nil, /* observer */
		0,   /* metric */
	); err != nil {
		t.Fatal(err)
	}

	if startCalled {
		t.Error("unexpected call to Controller.Up")
	}
}

type ndpDADEvent struct {
	nicID  tcpip.NICID
	addr   tcpip.Address
	result tcpipstack.DADResult
}

var _ ipv6.NDPDispatcher = (*testNDPDispatcher)(nil)

// testNDPDispatcher is a tcpip.NDPDispatcher that sends an NDP DAD event on
// dadC when OnDuplicateAddressDetectionResult gets called.
type testNDPDispatcher struct {
	dadC chan ndpDADEvent
}

// OnDuplicateAddressDetectionResult implements ipv6.NDPDispatcher.
func (n *testNDPDispatcher) OnDuplicateAddressDetectionResult(nicID tcpip.NICID, addr tcpip.Address, result tcpipstack.DADResult) {
	if c := n.dadC; c != nil {
		c <- ndpDADEvent{
			nicID:  nicID,
			addr:   addr,
			result: result,
		}
	}
}

// OnDefaultRouterDiscovered implements ipv6.NDPDispatcher.
//
// Adds the event to the event queue and returns true so Stack remembers the
// discovered default router.
func (*testNDPDispatcher) OnDefaultRouterDiscovered(tcpip.NICID, tcpip.Address) bool {
	return false
}

// OnDefaultRouterInvalidated implements ipv6.NDPDispatcher.
func (*testNDPDispatcher) OnDefaultRouterInvalidated(tcpip.NICID, tcpip.Address) {
}

// OnOnLinkPrefixDiscovered implements ipv6.NDPDispatcher.
func (*testNDPDispatcher) OnOnLinkPrefixDiscovered(tcpip.NICID, tcpip.Subnet) bool {
	return false
}

// OnOnLinkPrefixInvalidated implements ipv6.NDPDispatcher.
func (*testNDPDispatcher) OnOnLinkPrefixInvalidated(tcpip.NICID, tcpip.Subnet) {
}

// OnAutoGenAddress implements ipv6.NDPDispatcher.
func (*testNDPDispatcher) OnAutoGenAddress(tcpip.NICID, tcpip.AddressWithPrefix) bool {
	return false
}

// OnAutoGenAddressDeprecated implements ipv6.NDPDispatcher.
func (*testNDPDispatcher) OnAutoGenAddressDeprecated(tcpip.NICID, tcpip.AddressWithPrefix) {
}

// OnAutoGenAddressInvalidated implements ipv6.NDPDispatcher.
func (*testNDPDispatcher) OnAutoGenAddressInvalidated(tcpip.NICID, tcpip.AddressWithPrefix) {
}

// OnRecursiveDNSServerOption implements ipv6.NDPDispatcher.
func (*testNDPDispatcher) OnRecursiveDNSServerOption(tcpip.NICID, []tcpip.Address, time.Duration) {
}

// OnDNSSearchListOption implements ipv6.NDPDispatcher.
func (*testNDPDispatcher) OnDNSSearchListOption(tcpip.NICID, []string, time.Duration) {
}

// OnDHCPv6Configuration implements ipv6.NDPDispatcher.
func (*testNDPDispatcher) OnDHCPv6Configuration(tcpip.NICID, ipv6.DHCPv6ConfigurationFromNDPRA) {
}

func TestIpv6LinkLocalOnLinkRoute(t *testing.T) {
	if got, want := ipv6LinkLocalOnLinkRoute(6), (tcpip.Route{Destination: header.IPv6LinkLocalPrefix.Subnet(), NIC: 6}); got != want {
		t.Fatalf("got ipv6LinkLocalOnLinkRoute(6) = %s, want = %s", got, want)
	}
}

// Test that NICs get an on-link route to the IPv6 link-local subnet when it is
// brought up.
func TestIpv6LinkLocalOnLinkRouteOnUp(t *testing.T) {
	ns, _ := newNetstack(t)

	ep := noopEndpoint{
		linkAddress: tcpip.LinkAddress([]byte{2, 3, 4, 5, 6, 7}),
	}
	ifs, err := ns.addEndpoint(
		func(tcpip.NICID) string { return t.Name() },
		&ep,
		&noopController{},
		nil, /* observer */
		0,   /* metric */
	)
	if err != nil {
		t.Fatal(err)
	}

	linkLocalRoute := ipv6LinkLocalOnLinkRoute(ifs.nicid)

	// Initially should not have the link-local route.
	rt := ns.stack.GetRouteTable()
	if containsRoute(rt, linkLocalRoute) {
		t.Fatalf("got GetRouteTable() = %+v, don't want = %s", rt, linkLocalRoute)
	}

	// Bringing the ethernet device up should result in the link-local
	// route being added.
	if err := ifs.Up(); err != nil {
		t.Fatalf("eth.Up(): %s", err)
	}
	rt = ns.stack.GetRouteTable()
	if !containsRoute(rt, linkLocalRoute) {
		t.Fatalf("got GetRouteTable() = %+v, want = %s", rt, linkLocalRoute)
	}

	// Bringing the ethernet device down should result in the link-local
	// route being removed.
	if err := ifs.Down(); err != nil {
		t.Fatalf("eth.Down(): %s", err)
	}
	rt = ns.stack.GetRouteTable()
	if containsRoute(rt, linkLocalRoute) {
		t.Fatalf("got GetRouteTable() = %+v, don't want = %s", rt, linkLocalRoute)
	}
}

func TestDefaultV6Route(t *testing.T) {
	if got, want := defaultV6Route(6, testLinkLocalV6Addr1), (tcpip.Route{Destination: header.IPv6EmptySubnet, Gateway: testLinkLocalV6Addr1, NIC: 6}); got != want {
		t.Fatalf("got defaultV6Route(6, %s) = %s, want = %s", testLinkLocalV6Addr1, got, want)
	}
}

func TestOnLinkV6Route(t *testing.T) {
	subAddr := util.Parse("abcd:1234::")
	subMask := tcpip.AddressMask(util.Parse("ffff:ffff::"))
	subnet, err := tcpip.NewSubnet(subAddr, subMask)
	if err != nil {
		t.Fatalf("NewSubnet(%s, %s): %s", subAddr, subMask, err)
	}

	if got, want := onLinkV6Route(6, subnet), (tcpip.Route{Destination: subnet, NIC: 6}); got != want {
		t.Fatalf("got onLinkV6Route(6, %s) = %s, want = %s", subnet, got, want)
	}
}

func TestMulticastPromiscuousModeEnabledByDefault(t *testing.T) {
	ns, _ := newNetstack(t)

	multicastPromiscuousModeEnabled := false
	eth, _ := testutil.MakeEthernetDevice(t, ethernet.Info{}, 1)
	eth.ConfigMulticastSetPromiscuousModeImpl = func(enabled bool) (int32, error) {
		multicastPromiscuousModeEnabled = enabled
		return int32(zx.ErrOk), nil
	}

	if _, err := ns.addEth(testTopoPath, netstack.InterfaceConfig{Name: t.Name()}, &eth); err != nil {
		t.Fatal(err)
	}

	if !multicastPromiscuousModeEnabled {
		t.Error("expected a call to ConfigMulticastSetPromiscuousMode(true) by addEth")
	}
}

func TestUniqueFallbackNICNames(t *testing.T) {
	ns, _ := newNetstack(t)

	ifs1 := addNoopEndpoint(t, ns, "")
	t.Cleanup(ifs1.Remove)
	ifs2 := addNoopEndpoint(t, ns, "")
	t.Cleanup(ifs2.Remove)

	nicInfos := ns.stack.NICInfo()

	nicInfo1, ok := nicInfos[ifs1.nicid]
	if !ok {
		t.Fatalf("stack.NICInfo()[%d] = (_, false)", ifs1.nicid)
	}
	nicInfo2, ok := nicInfos[ifs2.nicid]
	if !ok {
		t.Fatalf("stack.NICInfo()[%d]: (_, false)", ifs2.nicid)
	}

	if nicInfo1.Name == nicInfo2.Name {
		t.Fatalf("got (%+v).Name == (%+v).Name, want non-equal", nicInfo1, nicInfo2)
	}
}

func TestStaticIPConfiguration(t *testing.T) {
	ns, _ := newNetstack(t)
	ns.filter = filter.New(ns.stack)

	addr := fidlconv.ToNetIpAddress(testV4Address)
	ifAddr := fidlnet.Subnet{Addr: addr, PrefixLen: 32}
	for _, test := range []struct {
		name     string
		features ethernet.Features
	}{
		{name: "default"},
		{name: "wlan", features: ethernet.FeaturesWlan},
	} {
		t.Run(test.name, func(t *testing.T) {
			d, _ := testutil.MakeEthernetDevice(t, ethernet.Info{
				Features: test.features,
				Mtu:      1400,
			}, 1)
			ifs, err := ns.addEth(testTopoPath, netstack.InterfaceConfig{Name: t.Name()}, &d)
			if err != nil {
				t.Fatal(err)
			}
			defer ifs.Remove()

			onlineChanged := make(chan bool, 1)
			ifs.observer.SetOnLinkOnlineChanged(func(linkOnline bool) {
				ifs.onLinkOnlineChanged(linkOnline)
				onlineChanged <- linkOnline
			})

			name := ifs.ns.name(ifs.nicid)
			result := ns.addInterfaceAddr(uint64(ifs.nicid), ifAddr)
			if result != stack.StackAddInterfaceAddressResultWithResponse(stack.StackAddInterfaceAddressResponse{}) {
				t.Fatalf("got ns.addInterfaceAddr(%d, %#v) = %#v, want = Response()", ifs.nicid, ifAddr, result)
			}

			if mainAddr, err := ns.stack.GetMainNICAddress(ifs.nicid, ipv4.ProtocolNumber); err != nil {
				t.Errorf("stack.GetMainNICAddress(%d, ipv4.ProtocolNumber): %s", ifs.nicid, err)
			} else if got := mainAddr.Address; got != testV4Address {
				t.Errorf("got stack.GetMainNICAddress(%d, ipv4.ProtocolNumber).Addr = %s, want = %s", ifs.nicid, got, testV4Address)
			}

			ifs.mu.Lock()
			if ifs.mu.dhcp.enabled {
				t.Error("expected dhcp state to be disabled initially")
			}
			ifs.mu.Unlock()

			if err := ifs.Down(); err != nil {
				t.Fatal(err)
			}

			ifs.mu.Lock()
			if ifs.mu.dhcp.enabled {
				t.Error("expected dhcp state to remain disabled after bringing interface down")
			}
			if ifs.mu.dhcp.running() {
				t.Error("expected dhcp state to remain stopped after bringing interface down")
			}
			ifs.mu.Unlock()

			if err := ifs.Up(); err != nil {
				t.Fatal(err)
			}
			if got, want := <-onlineChanged; got != want {
				t.Errorf("got state = %t, want %t", got, want)
			}

			ifs.mu.Lock()
			if ifs.mu.dhcp.enabled {
				t.Error("expected dhcp state to remain disabled after restarting interface")
			}
			ifs.mu.Unlock()

			ifs.setDHCPStatus(name, true)

			ifs.mu.Lock()
			if !ifs.mu.dhcp.enabled {
				t.Error("expected dhcp state to become enabled after manually enabling it")
			}
			if !ifs.mu.dhcp.running() {
				t.Error("expected dhcp state running")
			}
			ifs.mu.Unlock()
		})
	}
}

var _ NICRemovedHandler = (*noopNicRemovedHandler)(nil)

type noopNicRemovedHandler struct{}

func (*noopNicRemovedHandler) RemovedNIC(tcpip.NICID) {}

func newNetstack(t *testing.T) (*Netstack, *faketime.ManualClock) {
	t.Helper()
	return newNetstackWithNDPDispatcher(t, nil)
}

func newNetstackWithNicRemovedHandler(t *testing.T, h NICRemovedHandler) (*Netstack, *faketime.ManualClock) {
	t.Helper()
	return newNetstackWithStackNDPDispatcherAndNICRemovedHandler(t, nil, h)
}

func newNetstackWithNDPDispatcher(t *testing.T, ndpDisp *ndpDispatcher) (*Netstack, *faketime.ManualClock) {
	t.Helper()

	// ndpDispatcher should never be called with a nil receiver.
	//
	// From https://golang.org/doc/faq#nil_error:
	//
	// Under the covers, interfaces are implemented as two elements, a type T and
	// a value V.
	//
	// An interface value is nil only if the V and T are both unset, (T=nil, V is
	// not set), In particular, a nil interface will always hold a nil type. If we
	// store a nil pointer of type *int inside an interface value, the inner type
	// will be *int regardless of the value of the pointer: (T=*int, V=nil). Such
	// an interface value will therefore be non-nil even when the pointer value V
	// inside is nil.
	if ndpDisp == nil {
		return newNetstackWithStackNDPDispatcher(t, nil)
	}

	ns, clock := newNetstackWithStackNDPDispatcher(t, ndpDisp)
	ndpDisp.ns = ns
	ndpDisp.dynamicAddressSourceTracker.init(ns)
	return ns, clock
}

func newNetstackWithStackNDPDispatcher(t *testing.T, ndpDisp ipv6.NDPDispatcher) (*Netstack, *faketime.ManualClock) {
	t.Helper()
	return newNetstackWithStackNDPDispatcherAndNICRemovedHandler(t, ndpDisp, &noopNicRemovedHandler{})
}

func newNetstackWithStackNDPDispatcherAndNICRemovedHandler(t *testing.T, ndpDisp ipv6.NDPDispatcher, h NICRemovedHandler) (*Netstack, *faketime.ManualClock) {
	t.Helper()

	clock := faketime.NewManualClock()

	stk := tcpipstack.New(tcpipstack.Options{
		NetworkProtocols: []tcpipstack.NetworkProtocolFactory{
			arp.NewProtocol,
			ipv4.NewProtocol,
			ipv6.NewProtocolWithOptions(ipv6.Options{
				NDPDisp: ndpDisp,
			}),
		},
		TransportProtocols: []tcpipstack.TransportProtocolFactory{
			tcp.NewProtocol,
			udp.NewProtocol,
		},
		Clock: clock,
	})
	f := filter.New(stk)
	ns := &Netstack{
		stack: stk,
		// Required initialization because adding/removing interfaces interacts with
		// DNS configuration.
		dnsConfig:          dns.MakeServersConfig(stk.Clock()),
		nicRemovedHandlers: []NICRemovedHandler{h, f},
	}
	ns.stats.init(ns)
	ns.interfaceWatchers.mu.watchers = make(map[*interfaceWatcherImpl]struct{})
	ns.interfaceWatchers.mu.lastObserved = make(map[tcpip.NICID]interfaces.Properties)

	// TODO(https://fxbug.dev/68274): Remove this after moving all
	// filter methods to fuchsia.net.filter.
	ns.filter = f

	t.Cleanup(func() {
		for _, nic := range ns.stack.NICInfo() {
			ifs := nic.Context.(*ifState)
			ifs.Remove()
			ifs.endpoint.Wait()
		}
	})
	return ns, clock
}

func getInterfaceAddresses(t *testing.T, ni *stackImpl, nicid tcpip.NICID) []tcpip.AddressWithPrefix {
	t.Helper()

	ifaces, err := ni.ListInterfaces(context.Background())
	if err != nil {
		t.Fatalf("ni.ListInterfaces() failed: %s", err)
	}

	info, found := stack.InterfaceInfo{}, false
	for _, i := range ifaces {
		if tcpip.NICID(i.Id) == nicid {
			info = i
			found = true
			break
		}
	}
	if !found {
		t.Fatalf("couldn't find NICID=%d in %#v", nicid, ifaces)
	}

	addrs := make([]tcpip.AddressWithPrefix, 0, len(info.Properties.Addresses))
	for _, a := range info.Properties.Addresses {
		addrs = append(addrs, tcpip.AddressWithPrefix{
			Address:   fidlconv.ToTCPIPAddress(a.Addr),
			PrefixLen: int(a.PrefixLen),
		})
	}
	return addrs
}

func compareInterfaceAddresses(t *testing.T, got, want []tcpip.AddressWithPrefix) {
	t.Helper()
	sort.Slice(got, func(i, j int) bool { return got[i].Address < got[j].Address })
	sort.Slice(want, func(i, j int) bool { return want[i].Address < want[j].Address })
	if diff := cmp.Diff(got, want); diff != "" {
		t.Errorf("Interface addresses mismatch (-want +got):\n%s", diff)
	}
}

func TestNetstackImpl_GetInterfaces(t *testing.T) {
	ns, _ := newNetstack(t)
	ni := &netstackImpl{ns: ns}

	t.Cleanup(addNoopEndpoint(t, ns, "").Remove)

	ifaces, err := ni.GetInterfaces(context.Background())
	if err != nil {
		t.Fatal(err)
	}

	if l := len(ifaces); l == 0 {
		t.Fatalf("got len(GetInterfaces()) = %d, want != %d", l, l)
	}

	var expectedAddr fidlnet.IpAddress
	expectedAddr.SetIpv4(fidlnet.Ipv4Address{})
	for _, iface := range ifaces {
		if iface.Addr != expectedAddr {
			t.Errorf("got interface %+v, want Addr = %+v", iface, expectedAddr)
		}
		if iface.Netmask != expectedAddr {
			t.Errorf("got interface %+v, want NetMask = %+v", iface, expectedAddr)
		}
	}
}

// Test adding a list of both IPV4 and IPV6 addresses and then removing them
// again one-by-one.
func TestListInterfaceAddresses(t *testing.T) {
	ndpDisp := testNDPDispatcher{
		dadC: make(chan ndpDADEvent, 1),
	}
	ns, clock := newNetstackWithStackNDPDispatcher(t, &ndpDisp)
	ni := &stackImpl{ns: ns}

	ep := noopEndpoint{
		linkAddress: tcpip.LinkAddress([]byte{2, 3, 4, 5, 6, 7}),
	}
	ifState, err := ns.addEndpoint(
		func(tcpip.NICID) string { return t.Name() },
		&ep,
		&noopController{},
		nil, /* observer */
		0,   /* metric */
	)
	if err != nil {
		t.Fatal(err)
	}
	if err := ifState.Up(); err != nil {
		t.Fatal("ifState.Up(): ", err)
	}

	// Wait for and account for any addresses added automatically.
	clock.Advance(dadResolutionTimeout)
	for {
		select {
		case d := <-ndpDisp.dadC:
			t.Logf("startup DAD event: %#v", d)
			continue
		default:
		}
		break
	}

	wantAddrs := getInterfaceAddresses(t, ni, ifState.nicid)

	testAddresses := []tcpip.AddressWithPrefix{
		{"\x01\x01\x01\x01", 32},
		{"\x02\x02\x02\x02", 24},
		{"\x03\x03\x03\x03", 16},
		{"\x04\x04\x04\x04", 8},
		{"\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01", 128},
		{"\x02\x02\x02\x02\x02\x02\x02\x02\x02\x02\x02\x02\x02\x02\x02\x02", 64},
		{"\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03", 32},
		{"\x04\x04\x04\x04\x04\x04\x04\x04\x04\x04\x04\x04\x04\x04\x04\x04", 8},
	}

	t.Run("Add", func(t *testing.T) {
		for _, addr := range testAddresses {
			t.Run(addr.String(), func(t *testing.T) {
				ifAddr := fidlnet.Subnet{
					Addr:      fidlconv.ToNetIpAddress(addr.Address),
					PrefixLen: uint8(addr.PrefixLen),
				}

				result, err := ni.AddInterfaceAddress(context.Background(), uint64(ifState.nicid), ifAddr)
				AssertNoError(t, err)
				if result != stack.StackAddInterfaceAddressResultWithResponse(stack.StackAddInterfaceAddressResponse{}) {
					t.Fatalf("got ni.AddInterfaceAddress(%d, %#v) = %#v, want = Response()", ifState.nicid, ifAddr, result)
				}
				expectDad := header.IsV6UnicastAddress(addr.Address)
				clock.Advance(dadResolutionTimeout)
				select {
				case d := <-ndpDisp.dadC:
					if !expectDad {
						t.Fatalf("unexpected DAD event: %#v", d)
					}
					if diff := cmp.Diff(ndpDADEvent{nicID: ifState.nicid, addr: addr.Address, result: &tcpipstack.DADSucceeded{}}, d, cmp.AllowUnexported(d)); diff != "" {
						t.Fatalf("ndp DAD event mismatch (-want +got):\n%s", diff)
					}
				default:
					if expectDad {
						t.Fatal("timed out waiting for DAD event")
					}
				}
				wantAddrs = append(wantAddrs, addr)
				gotAddrs := getInterfaceAddresses(t, ni, ifState.nicid)

				compareInterfaceAddresses(t, gotAddrs, wantAddrs)
			})
		}
	})

	t.Run("Remove", func(t *testing.T) {
		for _, addr := range testAddresses {
			t.Run(addr.String(), func(t *testing.T) {
				ifAddr := fidlnet.Subnet{
					Addr:      fidlconv.ToNetIpAddress(addr.Address),
					PrefixLen: uint8(addr.PrefixLen),
				}

				result, err := ni.DelInterfaceAddress(context.Background(), uint64(ifState.nicid), ifAddr)
				AssertNoError(t, err)
				if result != stack.StackDelInterfaceAddressResultWithResponse(stack.StackDelInterfaceAddressResponse{}) {
					t.Fatalf("got ni.DelInterfaceAddress(%d, %#v) = %#v, want = Response()", ifState.nicid, ifAddr, result)
				}

				// Remove address from list.
				for i, a := range wantAddrs {
					if a == addr {
						wantAddrs = append(wantAddrs[:i], wantAddrs[i+1:]...)
						break
					}
				}
				gotAddrs := getInterfaceAddresses(t, ni, ifState.nicid)
				compareInterfaceAddresses(t, gotAddrs, wantAddrs)
			})
		}
	})
}

// Test that adding an address with one prefix and then adding the same address
// but with a different prefix will simply replace the first address.
func TestAddAddressesThenChangePrefix(t *testing.T) {
	ns, _ := newNetstack(t)
	ni := &stackImpl{ns: ns}
	ifState := addNoopEndpoint(t, ns, "")
	t.Cleanup(ifState.Remove)

	// The call to ns.addEndpoint() added addresses to the stack. Make sure we include
	// those in our want list.
	initialAddrs := getInterfaceAddresses(t, ni, ifState.nicid)

	// Add address.
	addr := tcpip.AddressWithPrefix{Address: "\x01\x01\x01\x01", PrefixLen: 8}
	ifAddr := fidlnet.Subnet{
		Addr:      fidlconv.ToNetIpAddress(addr.Address),
		PrefixLen: uint8(addr.PrefixLen),
	}

	result, err := ni.AddInterfaceAddress(context.Background(), uint64(ifState.nicid), ifAddr)
	AssertNoError(t, err)
	if result != stack.StackAddInterfaceAddressResultWithResponse(stack.StackAddInterfaceAddressResponse{}) {
		t.Fatalf("got ni.AddInterfaceAddress(%d, %#v) = %#v, want = Response()", ifState.nicid, ifAddr, result)
	}

	wantAddrs := append(initialAddrs, addr)
	gotAddrs := getInterfaceAddresses(t, ni, ifState.nicid)
	compareInterfaceAddresses(t, gotAddrs, wantAddrs)

	// Add the same address with a different prefix.
	addr.PrefixLen *= 2
	ifAddr.PrefixLen *= 2

	result, err = ni.AddInterfaceAddress(context.Background(), uint64(ifState.nicid), ifAddr)
	AssertNoError(t, err)
	if result != stack.StackAddInterfaceAddressResultWithResponse(stack.StackAddInterfaceAddressResponse{}) {
		t.Fatalf("got ni.AddInterfaceAddress(%d, %#v) = %#v, want = Response()", ifState.nicid, ifAddr, result)
	}

	wantAddrs = append(initialAddrs, addr)
	gotAddrs = getInterfaceAddresses(t, ni, ifState.nicid)

	compareInterfaceAddresses(t, gotAddrs, wantAddrs)
}

func TestAddRouteParameterValidation(t *testing.T) {
	ns, _ := newNetstack(t)
	addr := tcpip.ProtocolAddress{
		Protocol: ipv4.ProtocolNumber,
		AddressWithPrefix: tcpip.AddressWithPrefix{
			Address:   tcpip.Address("\xf0\xf0\xf0\xf0"),
			PrefixLen: 24,
		},
	}
	subnetLocalAddress := tcpip.Address("\xf0\xf0\xf0\xf1")
	ifState := addNoopEndpoint(t, ns, "")
	t.Cleanup(ifState.Remove)

	found, err := ns.addInterfaceAddress(ifState.nicid, addr)
	if err != nil {
		t.Fatalf("ns.addInterfaceAddress(%d, %s) = _, %s", ifState.nicid, addr.AddressWithPrefix, err)
	}
	if !found {
		t.Fatalf("ns.addInterfaceAddress(%d, %s) = %t, _", ifState.nicid, addr.AddressWithPrefix, found)
	}

	tests := []struct {
		name    string
		route   tcpip.Route
		metric  routes.Metric
		dynamic bool
		err     error
	}{
		{
			name: "IPv4 destination no NIC invalid gateway",
			route: tcpip.Route{
				Destination: util.PointSubnet(testV4Address),
				Gateway:     testV4Address,
				NIC:         0,
			},
			metric: routes.Metric(0),
			err:    routes.ErrNoSuchNIC,
		},
		{
			name: "IPv6 destination no NIC invalid gateway",
			route: tcpip.Route{
				Destination: util.PointSubnet(testV6Address),
				Gateway:     testV6Address,
				NIC:         0,
			},
			metric: routes.Metric(0),
			err:    routes.ErrNoSuchNIC,
		},
		{
			name: "IPv4 destination no NIC valid gateway",
			route: tcpip.Route{
				Destination: util.PointSubnet(testV4Address),
				Gateway:     subnetLocalAddress,
				NIC:         0,
			},
		},
		{
			name: "zero length gateway",
			route: tcpip.Route{
				Destination: util.PointSubnet(testV4Address),
				NIC:         ifState.nicid,
			},
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			if err := ns.AddRoute(test.route, test.metric, test.dynamic); !errors.Is(err, test.err) {
				t.Errorf("got ns.AddRoute(...) = %v, want %v", err, test.err)
			}
		})
	}
}

func TestDHCPAcquired(t *testing.T) {
	ns, _ := newNetstack(t)
	ifState := addNoopEndpoint(t, ns, "")
	t.Cleanup(ifState.Remove)

	addressBytes := []byte(testV4Address)
	nextAddress := func() tcpip.Address {
		addressBytes[len(addressBytes)-1]++
		return tcpip.Address(addressBytes)
	}

	serverAddress := nextAddress()
	router1Address := nextAddress()
	router2Address := nextAddress()
	multicastAddress := net.IPv4(0xe8, 0x2b, 0xd3, 0xea)
	if !multicastAddress.IsMulticast() {
		t.Fatalf("%s is not a multicast address", multicastAddress)
	}

	defaultMask := net.IP(testV4Address).DefaultMask()
	prefixLen, _ := defaultMask.Size()

	destination1, err := tcpip.NewSubnet(util.Parse("192.168.42.0"), tcpip.AddressMask(util.Parse("255.255.255.0")))
	if err != nil {
		t.Fatal(err)
	}
	destination2, err := tcpip.NewSubnet(util.Parse("0.0.0.0"), tcpip.AddressMask(util.Parse("0.0.0.0")))
	if err != nil {
		t.Fatal(err)
	}

	tests := []struct {
		name               string
		oldAddr, newAddr   tcpip.AddressWithPrefix
		config             dhcp.Config
		expectedRouteTable []routes.ExtendedRoute
	}{
		{
			name:    "subnet mask provided",
			oldAddr: tcpip.AddressWithPrefix{},
			newAddr: tcpip.AddressWithPrefix{
				Address:   testV4Address,
				PrefixLen: prefixLen,
			},
			config: dhcp.Config{
				ServerAddress: serverAddress,
				Router: []tcpip.Address{
					router1Address,
					router2Address,
					header.IPv4Any,
					header.IPv4Broadcast,
					tcpip.Address(multicastAddress),
				},
				SubnetMask: tcpip.AddressMask(defaultMask),
				DNS: []tcpip.Address{
					router1Address,
					router2Address,
				},
				LeaseLength: dhcp.Seconds(60),
			},
			expectedRouteTable: []routes.ExtendedRoute{
				{
					Route: tcpip.Route{
						Destination: destination1,
						NIC:         1,
					},
					Metric:                0,
					MetricTracksInterface: true,
					Dynamic:               true,
					Enabled:               false,
				},
				{
					Route: tcpip.Route{
						Destination: destination2,
						Gateway:     util.Parse("192.168.42.18"),
						NIC:         1,
					},
					Metric:                0,
					MetricTracksInterface: true,
					Dynamic:               true,
					Enabled:               false,
				},
				{
					Route: tcpip.Route{
						Destination: destination2,
						Gateway:     util.Parse("192.168.42.19"),
						NIC:         1,
					},
					Metric:                0,
					MetricTracksInterface: true,
					Dynamic:               true,
					Enabled:               false,
				},
			},
		},
		{
			name:    "no routers",
			oldAddr: tcpip.AddressWithPrefix{},
			newAddr: tcpip.AddressWithPrefix{
				Address:   testV4Address,
				PrefixLen: prefixLen,
			},
			config: dhcp.Config{},
			expectedRouteTable: []routes.ExtendedRoute{
				{
					Route: tcpip.Route{
						Destination: destination1,
						NIC:         1,
					},
					Metric:                0,
					MetricTracksInterface: true,
					Dynamic:               true,
					Enabled:               false,
				},
			},
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			// save current route table for later
			originalRouteTable := ifState.ns.GetExtendedRouteTable()

			// Update the DHCP address to the given test values and verify it took
			// effect.
			ifState.dhcpAcquired(test.oldAddr, test.newAddr, test.config)

			if diff := cmp.Diff(ifState.dns.mu.servers, test.config.DNS); diff != "" {
				t.Errorf("ifState.mu.dnsServers mismatch (-want +got):\n%s", diff)
			}

			if diff := cmp.Diff(ifState.ns.GetExtendedRouteTable(), test.expectedRouteTable, cmp.AllowUnexported(tcpip.Subnet{})); diff != "" {
				t.Errorf("GetExtendedRouteTable() mismatch (-want +got):\n%s", diff)
			}

			infoMap := ns.stack.NICInfo()
			if info, ok := infoMap[ifState.nicid]; ok {
				found := false
				for _, address := range info.ProtocolAddresses {
					if address.Protocol == ipv4.ProtocolNumber {
						switch address.AddressWithPrefix {
						case test.oldAddr:
							t.Errorf("expired address %s was not removed from NIC addresses %v", test.oldAddr, info.ProtocolAddresses)
						case test.newAddr:
							found = true
						}
					}
				}

				if !found {
					t.Errorf("new address %s was not added to NIC addresses %v", test.newAddr, info.ProtocolAddresses)
				}
			} else {
				t.Errorf("NIC %d not found in %v", ifState.nicid, infoMap)
			}

			// Remove the address and verify everything is cleaned up correctly.
			remAddr := test.newAddr
			ifState.dhcpAcquired(remAddr, tcpip.AddressWithPrefix{}, dhcp.Config{})

			if diff := cmp.Diff(ifState.dns.mu.servers, ifState.dns.mu.servers[:0]); diff != "" {
				t.Errorf("ifState.mu.dnsServers mismatch (-want +got):\n%s", diff)
			}

			if diff := cmp.Diff(ifState.ns.GetExtendedRouteTable(), originalRouteTable); diff != "" {
				t.Errorf("GetExtendedRouteTable() mismatch (-want +got):\n%s", diff)
			}

			infoMap = ns.stack.NICInfo()
			if info, ok := infoMap[ifState.nicid]; ok {
				for _, address := range info.ProtocolAddresses {
					if address.Protocol == ipv4.ProtocolNumber {
						if address.AddressWithPrefix == remAddr {
							t.Errorf("address %s/%d was not removed from NIC addresses %v", remAddr.Address, remAddr.PrefixLen, info.ProtocolAddresses)
						}
					}
				}
			} else {
				t.Errorf("NIC %d not found in %v", ifState.nicid, infoMap)
			}
		})
	}
}
