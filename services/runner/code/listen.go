package main

import (
	"context"
	"fmt"
	"net"

	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/adapters"
)

// requireLoopbackListeners refuses to start a Runner whose gRPC, Connect or
// REST listener would answer off this machine's loopback. The Runner serves
// this machine only, bearer or not: nothing on the LAN may reach its ports.
//
// Codefly decides the host each listener binds: the generated main sets it
// from the service's own endpoint carrier, which a native run names as
// localhost. This check does not pick a host; it refuses one that is not
// loopback, including the empty host, which binds every interface. That is
// what a render from a go-grpc agent older than 0.1.70, or a launch without
// Codefly's native runtime context, would bind.
func requireLoopbackListeners(ctx context.Context, config *adapters.Configuration) error {
	type listener struct{ name, host string }
	var listeners []listener
	if config.EndpointGrpcPort != 0 {
		listeners = append(listeners, listener{"gRPC", config.EndpointGrpcHost})
	}
	if config.EndpointConnectPort != nil {
		listeners = append(listeners, listener{"Connect", config.EndpointConnectHost})
	}
	if config.EndpointHttpPort != nil {
		listeners = append(listeners, listener{"REST", config.EndpointHttpHost})
	}
	for _, l := range listeners {
		if err := loopbackOnly(ctx, l.host); err != nil {
			return fmt.Errorf("the Runner's %s listener must answer on loopback only: %w", l.name, err)
		}
	}
	return nil
}

// loopbackOnly accepts a host every address of which is loopback.
func loopbackOnly(ctx context.Context, host string) error {
	if host == "" {
		return fmt.Errorf("no host, which binds every interface")
	}
	addresses, err := net.DefaultResolver.LookupIPAddr(ctx, host)
	if err != nil {
		return fmt.Errorf("host %q unresolved: %w", host, err)
	}
	if len(addresses) == 0 {
		return fmt.Errorf("host %q has no address", host)
	}
	for _, a := range addresses {
		if !a.IP.IsLoopback() {
			return fmt.Errorf("host %q resolves to %s", host, a.IP)
		}
	}
	return nil
}
