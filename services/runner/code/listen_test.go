package main

import (
	"context"
	"net"
	"strconv"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/adapters"
)

func TestRunnerRefusesAListenerOffLoopback(t *testing.T) {
	ctx := context.Background()
	port := uint16(1)
	for host, loopback := range map[string]bool{
		"localhost": true, "127.0.0.1": true, "::1": true,
		"": false, "0.0.0.0": false, "::": false, "192.168.1.20": false,
	} {
		for _, config := range []*adapters.Configuration{
			{EndpointGrpcPort: port, EndpointGrpcHost: host},
			{EndpointGrpcPort: port, EndpointGrpcHost: "localhost", EndpointConnectPort: &port, EndpointConnectHost: host},
			{EndpointConnectPort: &port, EndpointConnectHost: host},
		} {
			if err := requireLoopbackListeners(ctx, config); (err == nil) != loopback {
				t.Errorf("host %q in %+v: %v", host, config, err)
			}
		}
	}
	// It is the first thing the Runner checks, before it takes any state.
	if _, err := configureRunner(ctx, &adapters.Configuration{EndpointGrpcPort: port}); err == nil {
		t.Fatal("the Runner started with a wildcard listener")
	}
}

// The generated main binds the host Codefly's native carrier names, and the
// generated listeners honour it: the Runner's ports answer on loopback and on
// none of this machine's other addresses. Before go-grpc 0.1.70 both bound
// every interface.
func TestRunnerListenersAnswerOnlyOnLoopback(t *testing.T) {
	t.Setenv("CODEFLY__RUNTIME_CONTEXT", "native")
	host := listenerHost("localhost")
	if host != "localhost" {
		t.Fatalf("a native run binds %q, not its carrier's host", host)
	}
	grpcPort, connectPort := freePort(t), freePort(t)
	config := &adapters.Configuration{EndpointGrpcPort: grpcPort, EndpointGrpcHost: host, EndpointConnectPort: &connectPort, EndpointConnectHost: host}
	if err := requireLoopbackListeners(context.Background(), config); err != nil {
		t.Fatal(err)
	}
	server, err := adapters.NewServer(config)
	if err != nil {
		t.Fatal(err)
	}
	go func() { _ = server.Start(context.Background()) }()
	t.Cleanup(server.Stop)
	ports := []uint16{grpcPort, connectPort}
	for _, port := range ports {
		answers(t, "127.0.0.1", port, 5*time.Second)
	}
	others := machineAddresses(t)
	if len(others) == 0 {
		t.Skip("this machine has no non-loopback address to dial from")
	}
	for _, port := range ports {
		for _, ip := range others {
			target := net.JoinHostPort(ip.String(), strconv.Itoa(int(port)))
			if conn, err := net.DialTimeout("tcp", target, 500*time.Millisecond); err == nil {
				conn.Close()
				t.Errorf("the Runner answers off loopback at %s", target)
			}
		}
	}
}

func freePort(t *testing.T) uint16 {
	t.Helper()
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer l.Close()
	return uint16(l.Addr().(*net.TCPAddr).Port)
}

func answers(t *testing.T, host string, port uint16, within time.Duration) {
	t.Helper()
	target := net.JoinHostPort(host, strconv.Itoa(int(port)))
	for end := time.Now().Add(within); ; time.Sleep(20 * time.Millisecond) {
		conn, err := net.DialTimeout("tcp", target, 500*time.Millisecond)
		if err == nil {
			conn.Close()
			return
		}
		if time.Now().After(end) {
			t.Fatalf("nothing answers at %s: %v", target, err)
		}
	}
}

// machineAddresses are the addresses a sibling machine on the LAN could dial.
func machineAddresses(t *testing.T) []net.IP {
	t.Helper()
	addresses, err := net.InterfaceAddrs()
	if err != nil {
		t.Fatal(err)
	}
	var ips []net.IP
	for _, a := range addresses {
		network, ok := a.(*net.IPNet)
		if !ok || network.IP.IsLoopback() || network.IP.IsLinkLocalUnicast() || !network.IP.IsGlobalUnicast() && !network.IP.IsPrivate() {
			continue
		}
		ips = append(ips, network.IP)
	}
	return ips
}
