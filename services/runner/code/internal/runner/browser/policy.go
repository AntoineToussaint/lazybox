package browser

import (
	"errors"
	"fmt"
	"net"
	"net/url"
	"strconv"
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
	host, err := canonicalHost(u.Hostname())
	if err != nil {
		return nil, err
	}
	// The address the browser goes to is the one judged: 2130706433 is
	// 127.0.0.1 to it, and so it is to the policy and the site grants.
	if host != strings.ToLower(u.Hostname()) {
		hostname := host
		if strings.Contains(host, ":") {
			hostname = "[" + host + "]"
		}
		if port := u.Port(); port != "" {
			hostname += ":" + port
		}
		u.Host = hostname
	}
	if err := checkHost(u, blockedHosts, loopbackPorts); err != nil {
		return nil, err
	}
	return u, nil
}

// canonicalHost is a URL's host as a browser reads it: an IPv4 address in any
// of the forms a browser accepts (2130706433, 0x7f000001, 127.1, 0177.0.0.1,
// a trailing dot) is written as a.b.c.d; an IPv6 address and a name are lower
// case. A host whose last label is a number but which
// is no IPv4 address is refused, as a browser refuses it.
func canonicalHost(host string) (string, error) {
	host = strings.ToLower(strings.Trim(host, "[]"))
	if strings.Contains(host, ":") {
		if net.ParseIP(host) == nil {
			return "", fmt.Errorf("%w: %s is not a valid address", ErrBlocked, host)
		}
		return host, nil
	}
	labels := strings.Split(host, ".")
	if len(labels) > 1 && labels[len(labels)-1] == "" {
		labels = labels[:len(labels)-1]
	}
	if !numericLabel(labels[len(labels)-1]) {
		return host, nil
	}
	ip, ok := parseIPv4(labels)
	if !ok {
		return "", fmt.Errorf("%w: %s is not a valid address", ErrBlocked, host)
	}
	return ip.String(), nil
}

// numericLabel: decimal digits, or 0x and hex digits (WHATWG's "ends in a
// number").
func numericLabel(label string) bool {
	if label == "" {
		return false
	}
	digits, hex := label, false
	if rest, ok := strings.CutPrefix(digits, "0x"); ok {
		digits, hex = rest, true
	}
	for _, c := range digits {
		if !(c >= '0' && c <= '9') && !(hex && c >= 'a' && c <= 'f') {
			return false
		}
	}
	return hex || digits != ""
}

// parseIPv4 is WHATWG's IPv4 parser: one to four parts, each decimal, octal
// (a leading 0) or hex (0x), the last filling the bytes the others leave.
func parseIPv4(labels []string) (net.IP, bool) {
	if len(labels) > 4 {
		return nil, false
	}
	var value uint64
	for i, label := range labels {
		n, ok := ipv4Number(label)
		last := i == len(labels)-1
		if !ok || (!last && n > 255) || (last && n >= 1<<(8*(5-len(labels)))) {
			return nil, false
		}
		if !last {
			value |= n << (8 * (3 - i))
		} else {
			value |= n
		}
	}
	return net.IPv4(byte(value>>24), byte(value>>16), byte(value>>8), byte(value)), true
}

func ipv4Number(label string) (uint64, bool) {
	base := 10
	switch {
	case strings.HasPrefix(label, "0x"):
		label, base = label[2:], 16
		if label == "" {
			return 0, true
		}
	case len(label) > 1 && label[0] == '0':
		label, base = label[1:], 8
	case label == "":
		return 0, false
	}
	if len(label) > 32 {
		return 0, false
	}
	n, err := strconv.ParseUint(label, base, 64)
	return n, err == nil && n <= 1<<32
}

// checkHost refuses Lazybox's own addresses, loopback except a workspace
// preview's port, and private networks. Names are checked as written; the
// browser service also checks what they resolve to.
func checkHost(u *url.URL, blockedHosts []string, loopbackPorts []int) error {
	host, err := canonicalHost(u.Hostname())
	if err != nil {
		return err
	}
	// localhost. is localhost: a trailing dot only says the name is complete.
	host = strings.TrimSuffix(host, ".")
	port := u.Port()
	if port == "" {
		port = map[string]string{"http": "80", "https": "443"}[u.Scheme]
	}
	hostPort := net.JoinHostPort(host, port)
	for _, blocked := range blockedHosts {
		if sameHostPort(blocked, hostPort) || (isLoopbackName(host) && strings.HasSuffix(blocked, ":"+port) && isLoopbackHost(blocked)) {
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

// sameHostPort compares host:port pairs by the address each names.
func sameHostPort(a, b string) bool {
	ah, ap, errA := net.SplitHostPort(a)
	bh, bp, errB := net.SplitHostPort(b)
	if errA != nil || errB != nil {
		return strings.EqualFold(a, b)
	}
	ah, errA = canonicalHost(ah)
	bh, errB = canonicalHost(bh)
	return errA == nil && errB == nil && ap == bp && strings.TrimSuffix(ah, ".") == strings.TrimSuffix(bh, ".")
}

func isLoopbackHost(hostPort string) bool {
	host, _, err := net.SplitHostPort(hostPort)
	return err == nil && isLoopbackName(strings.ToLower(host))
}

// isLoopbackName: this computer, by name or address.
func isLoopbackName(host string) bool {
	host = strings.TrimSuffix(strings.Trim(host, "[]"), ".")
	if host == "localhost" || strings.HasSuffix(host, ".localhost") {
		return true
	}
	ip := net.ParseIP(host)
	if v4 := embeddedIPv4(ip); v4 != nil {
		ip = v4
	}
	return ip != nil && (ip.IsLoopback() || ip.IsUnspecified())
}

// embeddedIPv4 is the IPv4 address an IPv6 one carries in its last 32 bits:
// the old IPv4-compatible ::a.b.c.d and NAT64's 64:ff9b::a.b.c.d (Go reads
// IPv4-mapped ::ffff:a.b.c.d as IPv4 already).
func embeddedIPv4(ip net.IP) net.IP {
	if ip == nil || ip.To4() != nil || len(ip) != net.IPv6len {
		return nil
	}
	zero := func(b []byte) bool {
		for _, x := range b {
			if x != 0 {
				return false
			}
		}
		return true
	}
	compatible := zero(ip[:12]) && !zero(ip[12:]) && !ip.Equal(net.IPv6loopback)
	nat64 := ip[0] == 0 && ip[1] == 0x64 && ip[2] == 0xff && ip[3] == 0x9b && zero(ip[4:12])
	if !compatible && !nat64 {
		return nil
	}
	return net.IPv4(ip[12], ip[13], ip[14], ip[15])
}

// privateIP: addresses on the person's networks rather than the internet.
func privateIP(ip net.IP) bool {
	if v4 := embeddedIPv4(ip); v4 != nil {
		ip = v4
	}
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
