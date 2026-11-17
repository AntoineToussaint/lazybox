package remote

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os/exec"
	"strconv"
	"strings"
	"time"
)

// Tailscale is the route a phone takes to this Runner (REMOTE.md): `tailscale
// serve` terminates HTTPS on this machine's tailnet name with a certificate
// the phone's browser trusts, and proxies to the loopback device gateway.
// Traffic between the phone and this machine is WireGuard, end to end; only
// devices in the owner's tailnet can connect at all. Funnel, which would
// publish the port to the internet, is never used, and a port where Funnel is
// on is refused.
type Tailscale struct {
	// Run executes the tailscale CLI; tests replace it. Nil finds `tailscale`
	// on PATH.
	Run func(ctx context.Context, args ...string) ([]byte, error)
}

// Tailscale states, as RemoteAccess reports them.
const (
	TailscaleNotInstalled  = "not_installed"
	TailscaleNotRunning    = "not_running"
	TailscaleNeedsLogin    = "needs_login"
	TailscaleHTTPSDisabled = "https_disabled"
	TailscaleAvailable     = "available"
	TailscaleServing       = "serving"
	TailscaleConflict      = "conflict"
	TailscaleFailed        = "failed"
)

// TailscaleStatus is this machine on its tailnet, and whether the gateway is
// served there.
type TailscaleStatus struct {
	State   string
	DNSName string
	Detail  string
}

var errNotInstalled = errors.New("tailscale is not installed")

func (t Tailscale) run(ctx context.Context, args ...string) ([]byte, error) {
	if t.Run != nil {
		return t.Run(ctx, args...)
	}
	path, err := exec.LookPath("tailscale")
	if err != nil {
		return nil, errNotInstalled
	}
	ctx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	// Output, not CombinedOutput: the CLI prints version-skew warnings on stderr.
	out, err := exec.CommandContext(ctx, path, args...).Output()
	if err != nil {
		var exit *exec.ExitError
		if errors.As(err, &exit) && len(exit.Stderr) > 0 {
			return out, fmt.Errorf("tailscale %s: %s", args[0], strings.TrimSpace(string(exit.Stderr)))
		}
		return out, fmt.Errorf("tailscale %s: %w", args[0], err)
	}
	return out, nil
}

type tailnetStatus struct {
	BackendState string
	Self         struct{ DNSName string }
	CertDomains  []string
}

// serveConfig is the part of `tailscale serve status --json` that matters:
// which HTTPS ports are served, what each proxies to, and where Funnel is on.
type serveConfig struct {
	TCP map[string]struct {
		HTTPS        bool
		TCPForward   string
		TerminateTLS string
	}
	Web map[string]struct {
		Handlers map[string]struct {
			Proxy string
			Path  string
			Text  string
		}
	}
	AllowFunnel map[string]bool
}

// Status reports this machine's tailnet and whether port serves target.
func (t Tailscale) Status(ctx context.Context, port uint32, target string) TailscaleStatus {
	out, err := t.run(ctx, "status", "--json")
	if errors.Is(err, errNotInstalled) {
		return TailscaleStatus{State: TailscaleNotInstalled, Detail: "Install Tailscale on this computer and your phone, and sign in to the same tailnet on both."}
	}
	if err != nil && len(out) == 0 {
		return TailscaleStatus{State: TailscaleNotRunning, Detail: "Tailscale is installed but not running. Open Tailscale and connect."}
	}
	var s tailnetStatus
	if json.Unmarshal(out, &s) != nil {
		return TailscaleStatus{State: TailscaleFailed, Detail: "Tailscale's status could not be read."}
	}
	switch s.BackendState {
	case "Running":
	case "NeedsLogin", "NeedsMachineAuth":
		return TailscaleStatus{State: TailscaleNeedsLogin, Detail: "Sign in to Tailscale on this computer."}
	default:
		return TailscaleStatus{State: TailscaleNotRunning, Detail: "Tailscale is not connected. Open Tailscale and connect."}
	}
	name := strings.TrimSuffix(s.Self.DNSName, ".")
	if name == "" || len(s.CertDomains) == 0 {
		return TailscaleStatus{State: TailscaleHTTPSDisabled, DNSName: name, Detail: "Turn on MagicDNS and HTTPS certificates for your tailnet in the Tailscale admin console (DNS page)."}
	}
	status := TailscaleStatus{State: TailscaleAvailable, DNSName: name}
	serve, err := t.serve(ctx)
	if err != nil {
		return TailscaleStatus{State: TailscaleFailed, DNSName: name, Detail: err.Error()}
	}
	switch owner, funnel := serve.owner(name, port); {
	case funnel:
		status.State, status.Detail = TailscaleConflict, fmt.Sprintf("Funnel is on for port %d, which would put this Runner on the public internet. Turn Funnel off for that port, or choose another one.", port)
	case owner == target && target != "":
		status.State = TailscaleServing
	case owner != "":
		status.State, status.Detail = TailscaleConflict, fmt.Sprintf("Port %d of %s already serves %s. Choose another port in the Runner configuration (remote.httpsPort), or remove that service.", port, name, owner)
	}
	return status
}

func (t Tailscale) serve(ctx context.Context) (serveConfig, error) {
	var c serveConfig
	out, err := t.run(ctx, "serve", "status", "--json")
	if err != nil {
		return c, err
	}
	if strings.TrimSpace(string(out)) == "" {
		return c, nil
	}
	if err = json.Unmarshal(out, &c); err != nil {
		return c, errors.New("tailscale serve's configuration could not be read")
	}
	return c, nil
}

// owner says what port serves on this machine's name ("" if nothing) and
// whether Funnel is on for it.
func (c serveConfig) owner(name string, port uint32) (string, bool) {
	key := name + ":" + strconv.FormatUint(uint64(port), 10)
	funnel := c.AllowFunnel[key]
	if web, ok := c.Web[key]; ok {
		if h, ok := web.Handlers["/"]; ok && h.Proxy != "" {
			return h.Proxy, funnel
		}
		return "another web service", funnel
	}
	if tcp, ok := c.TCP[strconv.FormatUint(uint64(port), 10)]; ok {
		if tcp.TCPForward != "" {
			return tcp.TCPForward, funnel
		}
		if !tcp.HTTPS {
			return "another TCP service", funnel
		}
	}
	return "", funnel
}

// Serve makes port on the tailnet name proxy to target (the gateway on
// loopback), replacing only a mapping this Runner made before (previous).
func (t Tailscale) Serve(ctx context.Context, port uint32, target, previous string) (TailscaleStatus, error) {
	if !loopbackTarget(target) {
		return TailscaleStatus{}, errors.New("tailscale may only serve the device gateway on loopback")
	}
	status := t.Status(ctx, port, target)
	if status.State == TailscaleConflict && previous != "" {
		if serve, err := t.serve(ctx); err == nil {
			if owner, funnel := serve.owner(status.DNSName, port); owner == previous && !funnel {
				status.State = TailscaleAvailable
			}
		}
	}
	switch status.State {
	case TailscaleServing:
		return status, nil
	case TailscaleAvailable:
	default:
		return status, errors.New(status.Detail)
	}
	// HTTPS on the tailnet only: never --http, never funnel.
	if _, err := t.run(ctx, "serve", "--bg", "--https="+strconv.FormatUint(uint64(port), 10), target); err != nil {
		return TailscaleStatus{State: TailscaleFailed, DNSName: status.DNSName, Detail: err.Error()}, err
	}
	return t.Status(ctx, port, target), nil
}

// Unserve removes the mapping if it is this Runner's (target or previous);
// it never removes another service's.
func (t Tailscale) Unserve(ctx context.Context, port uint32, target, previous string) error {
	status := t.Status(ctx, port, target)
	if status.DNSName == "" {
		return nil
	}
	serve, err := t.serve(ctx)
	if err != nil {
		return err
	}
	owner, _ := serve.owner(status.DNSName, port)
	if owner == "" || (owner != target && owner != previous) {
		return nil
	}
	_, err = t.run(ctx, "serve", "--https="+strconv.FormatUint(uint64(port), 10), "off")
	return err
}

func loopbackTarget(target string) bool {
	for _, prefix := range []string{"http://127.0.0.1:", "http://[::1]:", "http://localhost:"} {
		if rest, ok := strings.CutPrefix(target, prefix); ok {
			_, err := strconv.ParseUint(rest, 10, 16)
			return err == nil
		}
	}
	return false
}
