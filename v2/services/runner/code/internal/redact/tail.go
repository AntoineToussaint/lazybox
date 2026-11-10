// Package redact keeps a bounded tail of a child process's stderr for exit
// diagnostics, with the credential values of its environment removed.
package redact

import (
	"strings"
	"sync"
)

// Limit is how much diagnostic text a Tail reports.
const Limit = 8192

// Tail is an io.Writer for a child's stderr. It is safe for concurrent use.
type Tail struct {
	mu      sync.Mutex
	data    []byte
	secrets []string
	longest int
}

// Redact removes the values of credential-like variables in env (names
// containing KEY, TOKEN, SECRET or PASSWORD) from everything Text returns.
func (t *Tail) Redact(env []string) {
	t.mu.Lock()
	defer t.mu.Unlock()
	for _, entry := range env {
		name, value, ok := strings.Cut(entry, "=")
		upper := strings.ToUpper(name)
		if ok && value != "" && (strings.Contains(upper, "KEY") || strings.Contains(upper, "TOKEN") || strings.Contains(upper, "SECRET") || strings.Contains(upper, "PASSWORD")) {
			t.secrets = append(t.secrets, value)
			t.longest = max(t.longest, len(value))
		}
	}
}

// Write keeps the last Limit bytes plus enough before them that a secret
// overlapping the reported window is still whole when it is redacted.
func (t *Tail) Write(data []byte) (int, error) {
	t.mu.Lock()
	defer t.mu.Unlock()
	n := len(data)
	keep := Limit + t.longest
	if n > keep {
		data = data[n-keep:]
	}
	t.data = append(t.data, data...)
	if len(t.data) > keep {
		t.data = append([]byte(nil), t.data[len(t.data)-keep:]...)
	}
	return n, nil
}

// Text is the redacted tail, at most Limit bytes.
func (t *Tail) Text() string {
	t.mu.Lock()
	defer t.mu.Unlock()
	text := string(t.data)
	for _, secret := range t.secrets {
		text = strings.ReplaceAll(text, secret, "[redacted]")
	}
	// Trim after redacting: trimming first could cut a secret and leave the
	// rest of it in the window, where it no longer matches.
	if len(text) > Limit {
		text = text[len(text)-Limit:]
	}
	return strings.TrimSpace(text)
}
