package browser

import (
	"errors"
	"fmt"
	"net"
	"net/url"
	"strings"
)

// Site policy. The Runner decides which sites an agent may open without
// asking; the browser service refuses blocked addresses on every request the
// page makes (BROWSER.md, "Safety"). Both sides check the scheme and the
// addresses that are never reachable, so neither is the only guard.

// ErrBlocked refuses an address no agent browser may reach.
var ErrBlocked = errors.New("blocked")

// Origin is scheme://host[:port] of an http(s) URL, with the default port
// left out, lower case.
func Origin(u *url.URL) string {
	host := strings.ToLower(u.Hostname())
	port := u.Port()
	if (u.Scheme == "http" && port == "80") || (u.Scheme == "https" && port == "443") {
		port = ""
	}
	if strings.Contains(host, ":") {
		host = "[" + host + "]"
	}
	if port != "" {
		host += ":" + port
	}
	return u.Scheme + "://" + host
}

// Site is how sites are named in allow-lists: the host, with its port when
// it is not the scheme's default.
func Site(u *url.URL) string {
	return strings.TrimPrefix(strings.TrimPrefix(Origin(u), "https://"), "http://")
}

// ParseURL accepts an absolute http or https URL an agent asked for. Anything
// else (file:, javascript:, data:, a Lazybox origin) is refused with a reason
// the agent can act on.
func ParseURL(raw string, blockedHosts []string, loopbackPorts []int) (*url.URL, error) {
	raw = strings.TrimSpace(raw)
	if raw == "" {
		return nil, errors.New("a URL is required")
	}
	u, err := url.Parse(raw)
	if err != nil {
		return nil, fmt.Errorf("not a URL: %w", err)
	}
	if u.Scheme != "http" && u.Scheme != "https" {
		if u.Scheme == "" {
			return nil, errors.New("give an absolute URL, starting with https://")
		}
		return nil, fmt.Errorf("%w: only http and https pages, not %s: URLs", ErrBlocked, u.Scheme)
	}
	if u.Hostname() == "" {
		return nil, errors.New("the URL has no host")
	}
	if u.User != nil {
		return nil, fmt.Errorf("%w: URLs with credentials in them are refused", ErrBlocked)
	}
	if err := checkHost(u, blockedHosts, loopbackPorts); err != nil {
		return nil, err
	}
	return u, nil
}

// checkHost refuses Lazybox's own addresses, loopback except a workspace
// preview's port, and private networks. Names are checked as written; the
// browser service also checks what they resolve to.
func checkHost(u *url.URL, blockedHosts []string, loopbackPorts []int) error {
	host := strings.ToLower(u.Hostname())
	port := u.Port()
	if port == "" {
		port = map[string]string{"http": "80", "https": "443"}[u.Scheme]
	}
	hostPort := net.JoinHostPort(host, port)
	for _, blocked := range blockedHosts {
		if strings.EqualFold(blocked, hostPort) || (isLoopbackName(host) && strings.HasSuffix(blocked, ":"+port) && isLoopbackHost(blocked)) {
			return fmt.Errorf("%w: %s is Lazybox itself", ErrBlocked, hostPort)
		}
	}
	if isLoopbackName(host) {
		for _, p := range loopbackPorts {
			if fmt.Sprint(p) == port {
				return nil
			}
		}
		return fmt.Errorf("%w: %s is this computer, and only this workspace's running previews are reachable", ErrBlocked, hostPort)
	}
	if ip := net.ParseIP(strings.Trim(host, "[]")); ip != nil && privateIP(ip) {
		return fmt.Errorf("%w: %s is a private network address", ErrBlocked, host)
	}
	return nil
}

func isLoopbackHost(hostPort string) bool {
	host, _, err := net.SplitHostPort(hostPort)
	return err == nil && isLoopbackName(strings.ToLower(host))
}

// isLoopbackName: this computer, by name or address.
func isLoopbackName(host string) bool {
	host = strings.Trim(host, "[]")
	if host == "localhost" || strings.HasSuffix(host, ".localhost") {
		return true
	}
	ip := net.ParseIP(host)
	return ip != nil && (ip.IsLoopback() || ip.IsUnspecified())
}

// privateIP: addresses on the person's networks rather than the internet.
func privateIP(ip net.IP) bool {
	if ip.IsPrivate() || ip.IsLinkLocalUnicast() || ip.IsLinkLocalMulticast() || ip.IsLoopback() || ip.IsUnspecified() || ip.IsMulticast() {
		return true
	}
	// Carrier-grade NAT, which Tailscale uses for its tailnet.
	_, cgnat, _ := net.ParseCIDR("100.64.0.0/10")
	return cgnat.Contains(ip)
}

// NormalizeSite validates an allow-list entry: a host (example.com, with an
// optional :port) or a wildcard (*.example.com, which also covers
// example.com). A pasted URL is reduced to its site.
func NormalizeSite(entry string) (string, error) {
	entry = strings.ToLower(strings.TrimSpace(entry))
	if strings.Contains(entry, "://") {
		u, err := url.Parse(entry)
		if err != nil || u.Host == "" {
			return "", fmt.Errorf("%q is not a site", entry)
		}
		entry = Site(u)
	}
	entry = strings.TrimSuffix(entry, "/")
	wild := strings.HasPrefix(entry, "*.")
	host := strings.TrimPrefix(entry, "*.")
	name, port, err := net.SplitHostPort(host)
	if err != nil {
		name, port = host, ""
	}
	if name == "" || strings.ContainsAny(name, "/*?#@ ") || len(entry) > 253 {
		return "", fmt.Errorf("%q is not a site: use a host such as example.com or *.example.com", entry)
	}
	if !strings.Contains(name, ".") && name != "localhost" {
		return "", fmt.Errorf("%q is not a site: use a full host name such as example.com", entry)
	}
	if port != "" {
		if wild {
			return "", fmt.Errorf("%q: a wildcard site takes no port", entry)
		}
		return name + ":" + port, nil
	}
	if wild {
		return "*." + name, nil
	}
	return name, nil
}

// SiteMatches reports whether an allow-list entry covers the site of u.
func SiteMatches(entry string, u *url.URL) bool {
	if base, ok := strings.CutPrefix(entry, "*."); ok {
		host := strings.ToLower(u.Hostname())
		return host == base || strings.HasSuffix(host, "."+base)
	}
	return entry == Site(u)
}
