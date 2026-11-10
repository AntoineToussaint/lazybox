package redact

import (
	"strings"
	"testing"
)

func TestCredentialValuesAreRedacted(t *testing.T) {
	var tail Tail
	tail.Redact([]string{"API_KEY=sk-live-123", "HOME=/home/me", "EMPTY_TOKEN="})
	_, _ = tail.Write([]byte("auth failed for sk-live-123 in /home/me\n"))
	if got := tail.Text(); got != "auth failed for [redacted] in /home/me" {
		t.Fatalf("%q", got)
	}
}

func TestTailIsBounded(t *testing.T) {
	var tail Tail
	_, _ = tail.Write([]byte(strings.Repeat("a", 3*Limit)))
	_, _ = tail.Write([]byte("end"))
	got := tail.Text()
	if len(got) != Limit || !strings.HasSuffix(got, "end") {
		t.Fatalf("len %d", len(got))
	}
}

// A secret that straddles the start of the reported window must not leave
// its remaining characters in it.
func TestSecretCutByTheWindowDoesNotLeak(t *testing.T) {
	secret := "SECRETVALUE-0123456789"
	var tail Tail
	tail.Redact([]string{"SERVICE_TOKEN=" + secret})
	// The secret's last ten bytes fall inside the last Limit bytes.
	_, _ = tail.Write([]byte(strings.Repeat("x", 100) + secret))
	_, _ = tail.Write([]byte(strings.Repeat("y", Limit-10)))
	if got := tail.Text(); strings.Contains(got, "0123456789") {
		t.Fatalf("part of a secret leaked: %q", got[:40])
	}
}
