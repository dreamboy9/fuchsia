// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// +build !build_with_native_toolchain

package netstack

import (
	"context"
	"crypto/rand"
	"encoding/base64"
	"errors"
	"flag"
	"fmt"
	"io"
	"log"
	"os"
	"path/filepath"
	"reflect"
	"strconv"
	"sync/atomic"
	"syscall/zx"
	"time"

	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/dns"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/filter"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/pprof"

	"go.fuchsia.dev/fuchsia/src/lib/component"
	syslog "go.fuchsia.dev/fuchsia/src/lib/syslog/go"

	"fidl/fuchsia/device"
	"fidl/fuchsia/net/interfaces"
	"fidl/fuchsia/net/neighbor"
	"fidl/fuchsia/net/routes"
	"fidl/fuchsia/net/stack"
	"fidl/fuchsia/netstack"
	"fidl/fuchsia/posix/socket"
	"fidl/fuchsia/stash"

	glog "gvisor.dev/gvisor/pkg/log"
	"gvisor.dev/gvisor/pkg/tcpip"
	"gvisor.dev/gvisor/pkg/tcpip/header"
	"gvisor.dev/gvisor/pkg/tcpip/link/sniffer"
	"gvisor.dev/gvisor/pkg/tcpip/network/arp"
	"gvisor.dev/gvisor/pkg/tcpip/network/ipv4"
	"gvisor.dev/gvisor/pkg/tcpip/network/ipv6"
	tcpipstack "gvisor.dev/gvisor/pkg/tcpip/stack"
	"gvisor.dev/gvisor/pkg/tcpip/transport/icmp"
	"gvisor.dev/gvisor/pkg/tcpip/transport/tcp"
	"gvisor.dev/gvisor/pkg/tcpip/transport/udp"
)

const (
	// stashStoreIdentificationName is the name used to identify this (netstack)
	// component to the fuchsia.stash.SecureStore service.
	stashStoreIdentificationName = "netstack-stash"

	// opaqueIIDSecretKeyName is the name of the key used to access the secret key
	// for opaque IIDs from the secure stash store.
	opaqueIIDSecretKeyName = "opaque-iid-secret-key"

	// dadTransmits is the number of consecutive NDP Neighbor Solicitation
	// messages sent while performing Duplicate Address Detection on a IPv6
	// tentative address.
	//
	// As per RFC 4862 section 5.1, 1 is the default number of messages to send.
	dadTransmits = 1

	// dadRetransmitTimer is the time between retransmissions of NDP Neighbor
	// Solicitation messages to a neighbor.
	//
	// As per RFC 4861 section 10, 1s is the default time between retransmissions.
	dadRetransmitTimer = time.Second

	// maxRtrSolicitations is the maximum number of Router Solicitation messages
	// to send when a NIC becomes enabled.
	//
	// As per RFC 4861 section 10, 3 is the default number of messages.
	maxRtrSolicitations = 3

	// rtrSolicitationInterval is the amount of time between sending Router
	// Solicitation messages.
	//
	// As per RFC 4861 section 10, 4s is the default time between transmissions.
	rtrSolicitationInterval = 4 * time.Second

	// maxRtrSolicitationDelay is the maximum amount of time to wait before
	// sending the first Router Solicitation message.
	//
	// As per RFC 4861 section 10, 1s is the default maximum time to wait.
	maxRtrSolicitationDelay = time.Second

	// autoGenAddressConflictRetries is the maximum number of times to attempt
	// SLAAC address regeneration in response to DAD conflicts.
	//
	// As per RFC 7217 section, 3 is the default maximum number of retries.
	autoGenAddressConflictRetries = 3

	// maxTempAddrValidLifetime is the maximum amount of time a temporary SLAAC
	// address may be valid for from creation.
	//
	// As per RFC 4941 section 5, 7 days is the default max valid lifetime.
	maxTempAddrValidLifetime = 7 * 24 * time.Hour

	// maxTempAddrPreferredLifetime is the maximum amount of time a temporary
	// SLAAC address may be preferred for from creation.
	//
	// As per RFC 4941 section 5, 1 day is the default max preferred lifetime.
	maxTempAddrPreferredLifetime = 24 * time.Hour

	// regenAdvanceDuration is duration before the deprecation of a temporary
	// address when a new address will be generated.
	//
	// As per RFC 4941 section 5, 5s is the default duration. We make the duration
	// the default duration plus the maximum amount of time for an address to
	// resolve DAD if all but the last regeneration attempts fail. This is to
	// guarantee that if a new address is generated, it will be assigned for at
	// least 5s before the original address is deprecated.
	regenAdvanceDuration = 5*time.Second + dadTransmits*dadRetransmitTimer*(1+autoGenAddressConflictRetries)

	// handleRAs is the configuration for when Router Advertisements should be
	// handled.
	//
	// We want to handle router advertisements even when operating as a router
	// so that we can perform router/prefix discovery and SLAAC.
	handleRAs = ipv6.HandlingRAsAlwaysEnabled
)

type atomicBool uint32

// IsBoolFlag implements flag.boolFlag.IsBoolFlag.
//
// See the flag.Value documentation for more information.
func (*atomicBool) IsBoolFlag() bool {
	return true
}

// Set implements flag.Value.Set.
func (a *atomicBool) Set(s string) error {
	v, err := strconv.ParseBool(s)
	if err != nil {
		return err
	}
	var val uint32
	if v {
		val = 1
	}
	atomic.StoreUint32((*uint32)(a), val)
	return nil
}

// String implements flag.Value.String.
func (a *atomicBool) String() string {
	return strconv.FormatBool(atomic.LoadUint32((*uint32)(a)) != 0)
}

func init() {
	// As of this writing the default is 1.
	atomic.StoreUint32(&sniffer.LogPackets, 0)
}

type glogEmitter struct{}

func (*glogEmitter) Emit(depth int, level glog.Level, timestamp time.Time, format string, v ...interface{}) {
	switch level {
	case glog.Warning:
		syslog.Warnf(format, v...)
	case glog.Info:
		syslog.Infof(format, v...)
	case glog.Debug:
		syslog.Debugf(format, v...)
	}
}

func Main() {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	logLevel := syslog.InfoLevel

	flags := flag.NewFlagSet(os.Args[0], flag.ContinueOnError)
	flags.Var((*atomicBool)(&sniffer.LogPackets), "log-packets", "enable packet logging")
	flags.Var(&logLevel, "verbosity", "set the logging verbosity")

	var socketStatsTimerPeriod time.Duration
	flags.DurationVar(&socketStatsTimerPeriod, "socket-stats-sampling-interval", time.Minute, "set the interval at which socket stats will be sampled")

	noOpaqueIID := false
	flags.BoolVar(&noOpaqueIID, "no-opaque-iids", false, "disable opaque IIDs")

	if err := flags.Parse(os.Args[1:]); err != nil {
		panic(err)
	}

	appCtx := component.NewContextFromStartupInfo()

	s, err := syslog.ConnectToLogger(appCtx.Connector())
	if err != nil {
		panic(fmt.Sprintf("failed to connect to syslog: %s", err))
	}
	l, err := syslog.NewLogger(syslog.LogInitOptions{
		LogLevel:                      logLevel,
		MinSeverityForFileAndLineInfo: logLevel,
		Socket:                        s,
		Tags:                          []string{"netstack"},
	})
	if err != nil {
		panic(err)
	}
	syslog.SetDefaultLogger(l)
	log.SetOutput(&syslog.Writer{Logger: l})
	log.SetFlags(log.Lshortfile)
	glog.SetTarget(&glogEmitter{})

	_ = syslog.Infof("starting...")

	var opaqueIIDOpts ipv6.OpaqueInterfaceIdentifierOptions
	if !noOpaqueIID {
		secretKeyForOpaqueIID, err := getSecretKeyForOpaqueIID(appCtx)
		if err != nil {
			panic(fmt.Sprintf("failed to get secret key for opaque IIDs: %s", err))
		}
		opaqueIIDOpts = ipv6.OpaqueInterfaceIdentifierOptions{
			NICNameFromID: func(nicID tcpip.NICID, nicName string) string {
				// As of writing, Netstack creates NICs with names so we return the name
				// the NIC was created with. Just in case, we have a default NIC name
				// format for NICs that were not created with a name.
				if nicName != "" {
					return nicName
				}
				return fmt.Sprintf("opaqueIIDNIC%d", nicID)
			},
			SecretKey: secretKeyForOpaqueIID,
		}
	}

	tempIIDSeed, err := newSecretKey(header.IIDSize)
	if err != nil {
		panic(fmt.Sprintf("failed to get temp IID seed: %s", err))
	}

	ndpDisp := newNDPDispatcher()
	var nudDisp nudDispatcher

	dadConfigs := tcpipstack.DADConfigurations{
		DupAddrDetectTransmits: dadTransmits,
		RetransmitTimer:        dadRetransmitTimer,
	}

	stk := tcpipstack.New(tcpipstack.Options{
		NetworkProtocols: []tcpipstack.NetworkProtocolFactory{
			arp.NewProtocolWithOptions(arp.Options{
				DADConfigs: dadConfigs,
			}),
			ipv4.NewProtocolWithOptions(ipv4.Options{
				IGMP: ipv4.IGMPOptions{
					Enabled: true,
				},
			}),
			ipv6.NewProtocolWithOptions(ipv6.Options{
				DADConfigs: dadConfigs,
				NDPConfigs: ipv6.NDPConfigurations{
					MaxRtrSolicitations:           maxRtrSolicitations,
					RtrSolicitationInterval:       rtrSolicitationInterval,
					MaxRtrSolicitationDelay:       maxRtrSolicitationDelay,
					HandleRAs:                     handleRAs,
					DiscoverDefaultRouters:        true,
					DiscoverOnLinkPrefixes:        true,
					AutoGenGlobalAddresses:        true,
					AutoGenAddressConflictRetries: autoGenAddressConflictRetries,
					AutoGenTempGlobalAddresses:    true,
					MaxTempAddrValidLifetime:      maxTempAddrValidLifetime,
					MaxTempAddrPreferredLifetime:  maxTempAddrPreferredLifetime,
					RegenAdvanceDuration:          regenAdvanceDuration,
				},
				AutoGenLinkLocal: true,
				NDPDisp:          ndpDisp,
				OpaqueIIDOpts:    opaqueIIDOpts,
				TempIIDSeed:      tempIIDSeed,
				MLD: ipv6.MLDOptions{
					Enabled: true,
				},
			}),
		},
		TransportProtocols: []tcpipstack.TransportProtocolFactory{
			icmp.NewProtocol4,
			icmp.NewProtocol6,
			tcp.NewProtocol,
			udp.NewProtocol,
		},
		HandleLocal: true,
		NUDDisp:     &nudDisp,

		// Raw sockets are typically used for implementing custom protocols. We intend
		// to support custom protocols through structured FIDL APIs in the future, so
		// disable raw sockets to prevent them from accidentally becoming load-bearing.
		RawFactory: nil,
	})

	delayEnabled := tcpip.TCPDelayEnabled(true)
	sackEnabled := tcpip.TCPSACKEnabled(true)
	moderateReceiveBufferOption := tcpip.TCPModerateReceiveBufferOption(true)
	for _, opt := range []tcpip.SettableTransportProtocolOption{
		&delayEnabled,
		&sackEnabled,
		&moderateReceiveBufferOption,
	} {
		if err := stk.SetTransportProtocolOption(tcp.ProtocolNumber, opt); err != nil {
			syslog.Fatalf("SetTransportProtocolOption(%d, %#v) failed: %s", tcp.ProtocolNumber, opt, err)
		}
	}

	req, np, err := device.NewNameProviderWithCtxInterfaceRequest()
	if err != nil {
		syslog.Fatalf("could not connect to device name provider service: %s", err)
	}
	appCtx.ConnectToEnvService(req)

	f := filter.New(stk)

	ns := &Netstack{
		dnsConfig:          dns.MakeServersConfig(stk.Clock()),
		nameProvider:       np,
		stack:              stk,
		nicRemovedHandlers: []NICRemovedHandler{&ndpDisp.dynamicAddressSourceTracker, f},
	}

	ns.interfaceWatchers.mu.watchers = make(map[*interfaceWatcherImpl]struct{})
	ns.interfaceWatchers.mu.lastObserved = make(map[tcpip.NICID]interfaces.Properties)

	nudDisp.ns = ns
	ndpDisp.ns = ns
	ndpDisp.dynamicAddressSourceTracker.init(ns)
	ndpDisp.start(ctx)

	ns.filter = f
	filter.AddOutgoingService(appCtx, f)

	if err := ns.addLoopback(); err != nil {
		syslog.Fatalf("loopback: %s", err)
	}

	dnsWatchers := newDnsServerWatcherCollection(ns.dnsConfig.GetServersCacheAndChannel)

	socketProviderImpl := providerImpl{ns: ns}
	ns.stats.init(ns)
	appCtx.OutgoingService.AddDiagnostics("counters", &component.DirectoryWrapper{
		Directory: &inspectDirectory{
			asService: (&inspectImpl{
				inner: &statCounterInspectImpl{
					name:  "Networking Stat Counters",
					value: reflect.ValueOf(&ns.stats).Elem(),
				},
			}).asService,
		},
	})
	appCtx.OutgoingService.AddDiagnostics("interfaces", &component.DirectoryWrapper{
		Directory: &inspectDirectory{
			// asService is late-bound so that each call retrieves fresh NIC info.
			asService: func() *component.Service {
				return (&inspectImpl{
					inner: &nicInfoMapInspectImpl{value: ns.getIfStateInfo(stk.NICInfo())},
				}).asService()
			},
		},
	})
	appCtx.OutgoingService.AddDiagnostics("sockets", &component.DirectoryWrapper{
		Directory: &inspectDirectory{
			asService: (&inspectImpl{
				inner: &socketInfoMapInspectImpl{
					value: &ns.endpoints,
				},
			}).asService,
		},
	})
	appCtx.OutgoingService.AddDiagnostics("routes", &component.DirectoryWrapper{
		Directory: &inspectDirectory{
			// asService is late-bound so that each call retrieves fresh routing table info.
			asService: func() *component.Service {
				return (&inspectImpl{
					inner: &routingTableInspectImpl{value: ns.GetExtendedRouteTable()},
				}).asService()
			},
		},
	})

	// Minimal support for the inspect VMO format allows our profile protos to be
	// picked up by bug reports.
	//
	// To extract these serialized protos from inspect.json, jq can be used:
	//
	// cat iquery.json | \
	// jq '.[] | select(.path | contains("/pprof/")) | .contents.root.pprof.goroutine[4:]' | \
	// xargs echo | base64 --decode > goroutine
	func() {
		isolatedCache := filepath.Join("", "cache")
		if _, err := os.Stat(isolatedCache); err != nil {
			if os.IsNotExist(err) {
				_ = syslog.Warnf("isolated-cache-storage is not available; snapshots will not include pprof data: %s", err)
				return
			}
			_ = syslog.Fatalf("%s", err)
		}
		pprofCache := filepath.Join(isolatedCache, "pprof")
		if err := os.Mkdir(pprofCache, os.ModePerm); err != nil && !os.IsExist(err) {
			var zxError *zx.Error
			if errors.As(err, &zxError) && zxError.Status == zx.ErrNoSpace {
				_ = syslog.Warnf("isolated-cache-storage is full; snapshots will not include pprof data: %s", err)
				return
			}
			_ = syslog.Fatalf("%s", err)
		}
		dir, run, err := pprof.Setup(pprofCache)
		if err != nil {
			_ = syslog.Fatalf("%s", err)
		}
		appCtx.OutgoingService.AddDiagnostics("pprof", dir)
		go func() {
			if err := run(); err != nil {
				_ = syslog.Errorf("pprof directory serving error; snapshots will not include pprof data: %s", err)
			}
		}()
	}()

	{
		stub := netstack.NetstackWithCtxStub{Impl: &netstackImpl{ns: ns}}
		appCtx.OutgoingService.AddService(
			netstack.NetstackName,
			func(ctx context.Context, c zx.Channel) error {
				go component.ServeExclusive(ctx, &stub, c, func(err error) {
					_ = syslog.WarnTf(netstack.NetstackName, "%s", err)
				})

				return nil
			},
		)
	}

	{
		stub := stack.StackWithCtxStub{Impl: &stackImpl{
			ns:          ns,
			dnsWatchers: dnsWatchers,
		}}
		appCtx.OutgoingService.AddService(
			stack.StackName,
			func(ctx context.Context, c zx.Channel) error {
				go component.ServeExclusive(ctx, &stub, c, func(err error) {
					_ = syslog.WarnTf(stack.StackName, "%s", err)
				})
				return nil
			},
		)
	}

	{
		stub := stack.LogWithCtxStub{Impl: &logImpl{
			logger:     l,
			logPackets: &sniffer.LogPackets,
		}}
		appCtx.OutgoingService.AddService(
			stack.LogName,
			func(ctx context.Context, c zx.Channel) error {
				go component.ServeExclusive(ctx, &stub, c, func(err error) {
					_ = syslog.WarnTf(stack.LogName, "%s", err)
				})
				return nil
			})
	}

	{
		stub := socket.ProviderWithCtxStub{Impl: &socketProviderImpl}
		appCtx.OutgoingService.AddService(
			socket.ProviderName,
			func(ctx context.Context, c zx.Channel) error {
				go component.ServeExclusive(ctx, &stub, c, func(err error) {
					_ = syslog.WarnTf(socket.ProviderName, "%s", err)
				})
				return nil
			},
		)
	}

	{
		stub := routes.StateWithCtxStub{Impl: &routesImpl{ns.stack}}
		appCtx.OutgoingService.AddService(
			routes.StateName,
			func(ctx context.Context, c zx.Channel) error {
				go component.ServeExclusive(ctx, &stub, c, func(err error) {
					_ = syslog.WarnTf(routes.StateName, "%s", err)
				})
				return nil
			},
		)
	}

	{
		stub := interfaces.StateWithCtxStub{Impl: &interfaceStateImpl{ns: ns}}
		appCtx.OutgoingService.AddService(
			interfaces.StateName,
			func(ctx context.Context, c zx.Channel) error {
				go component.ServeExclusive(ctx, &stub, c, func(err error) {
					_ = syslog.WarnTf(interfaces.StateName, "%s", err)
				})
				return nil
			},
		)
	}

	{
		impl := &neighborImpl{stack: stk}

		viewStub := neighbor.ViewWithCtxStub{Impl: impl}
		appCtx.OutgoingService.AddService(
			neighbor.ViewName,
			func(ctx context.Context, c zx.Channel) error {
				go component.ServeExclusive(ctx, &viewStub, c, func(err error) {
					_ = syslog.WarnTf(neighbor.ViewName, "%s", err)
				})
				return nil
			},
		)

		controllerStub := neighbor.ControllerWithCtxStub{Impl: impl}
		appCtx.OutgoingService.AddService(
			neighbor.ControllerName,
			func(ctx context.Context, c zx.Channel) error {
				go component.ServeExclusive(ctx, &controllerStub, c, func(err error) {
					_ = syslog.WarnTf(neighbor.ControllerName, "%s", err)
				})
				return nil
			},
		)
	}

	appCtx.BindStartupHandle(context.Background())
}

// newSecretKey returns a new secret key.
func newSecretKey(keyLen int) ([]byte, error) {
	secretKey := make([]byte, keyLen)
	if _, err := io.ReadFull(rand.Reader, secretKey); err != nil {
		return nil, fmt.Errorf("failed to populate a new secret key of %d bytes: %s", keyLen, err)
	}
	return secretKey, nil
}

// newSecretKeyForOpaqueIID returns a new secret key for opaque IID generation.
func newSecretKeyForOpaqueIID() ([]byte, error) {
	return newSecretKey(header.OpaqueIIDSecretKeyMinBytes)
}

// getSecretKeyForOpaqueIID gets a secret key for opaque IID generation from the
// secure stash store service, or attempts to create one. If the stash service
// is unavailable, a temporary secret key will be returned.
func getSecretKeyForOpaqueIID(appCtx *component.Context) ([]byte, error) {
	syslog.VLogf(syslog.DebugVerbosity, "getting or creating secret key for opaque IID from secure stash store")

	// Connect to the secure stash store service.
	storeReq, store, err := stash.NewSecureStoreWithCtxInterfaceRequest()
	if err != nil {
		syslog.Errorf("could not create the request to connect to the %s service: %s", stash.SecureStoreName, err)
		return newSecretKeyForOpaqueIID()
	}
	defer func() {
		_ = store.Close()
	}()
	appCtx.ConnectToEnvService(storeReq)

	// Use our secure stash.
	if err := store.Identify(context.Background(), stashStoreIdentificationName); err != nil {
		syslog.Warnf("failed to identify as %s to the secure stash store: %s", stashStoreIdentificationName, err)
		return newSecretKeyForOpaqueIID()
	}
	storeAccessorReq, storeAccessor, err := stash.NewStoreAccessorWithCtxInterfaceRequest()
	if err != nil {
		syslog.Errorf("could not create the secure stash store accessor request: %s", err)
		return newSecretKeyForOpaqueIID()
	}
	defer func() {
		_ = storeAccessor.Close()
	}()
	if err := store.CreateAccessor(context.Background(), false /* readOnly */, storeAccessorReq); err != nil {
		syslog.Warnf("failed to create accessor to the secure stash store: %s", err)
		return newSecretKeyForOpaqueIID()
	}

	// Attempt to get the existing secret key.
	opaqueIIDSecretKeyValue, err := storeAccessor.GetValue(context.Background(), opaqueIIDSecretKeyName)
	if err != nil {
		syslog.Warnf("failed to get opaque IID secret key from secure stash store: %s", err)
		return newSecretKeyForOpaqueIID()
	}

	// If a key exists, make sure it is valid before returning it.
	//
	// The value should be stored as a base64 encoded string.
	//
	// We use a string because stash.Value.Bytesval uses a fuchsia.mem.Buffer
	// which uses a VMO (which uses memory in page size increments). This is
	// wasteful as the key only uses 16 bytes. We base64 encode the string
	// because stash.Value.Stringval expects an ascii string; the store returns an
	// error when flushing the store with a stash.Value.Stringval of raw bytes.
	if opaqueIIDSecretKeyValue != nil && opaqueIIDSecretKeyValue.Which() == stash.ValueStringval {
		syslog.VLogf(syslog.DebugVerbosity, "found a secret key for opaque IIDs in the secure stash store")

		if secretKey, err := base64.StdEncoding.DecodeString(opaqueIIDSecretKeyValue.Stringval); err != nil {
			syslog.Errorf("failed to decode the secret key string: %s", err)
		} else if l := len(secretKey); l != header.OpaqueIIDSecretKeyMinBytes {
			syslog.Errorf("invalid secret key for opaque IIDs; got length = %d, want = %d", l, header.OpaqueIIDSecretKeyMinBytes)
		} else {
			syslog.VLogf(syslog.DebugVerbosity, "using existing secret key for opaque IIDs")
			return secretKey, nil
		}
	}

	// Generate a new secret key as we either do not have one or the one we have
	// is invalid.
	syslog.VLogf(syslog.DebugVerbosity, "generating a new secret key for opaque IIDs")
	secretKey, err := newSecretKeyForOpaqueIID()
	if err != nil {
		return nil, err
	}

	// Store the newly generated key to the secure stash store as a base64
	// encoded string.
	if err := storeAccessor.SetValue(context.Background(), opaqueIIDSecretKeyName, stash.ValueWithStringval(base64.StdEncoding.EncodeToString(secretKey))); err != nil {
		syslog.Errorf("failed to set newly created secret key for opaque IID to secure stash store: %s", err)
		return secretKey, nil
	}
	flushResp, err := storeAccessor.Flush(context.Background())
	if err != nil {
		syslog.Errorf("failed to flush secure stash store with updated secret key for opaque IID: %s", err)
		return secretKey, nil
	}
	switch w := flushResp.Which(); w {
	case stash.StoreAccessorFlushResultErr:
		syslog.Errorf("got error response when flushing secure stash store with updated secret key for opaque IID: %s", flushResp.Err)
		return secretKey, nil

	case stash.StoreAccessorFlushResultResponse:
		syslog.VLogf(syslog.DebugVerbosity, "saved newly generated secret key for opaque IIDs to secure stash store")
		return secretKey, nil

	default:
		panic(fmt.Sprintf("unexpected store accessor flush result type: %d", w))
	}
}
