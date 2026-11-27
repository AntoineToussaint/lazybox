package browser

import (
	"errors"
	"testing"
)

// Regression: hosts a browser reads as loopback or private addresses passed
// the Runner's check, which recognised only a dotted address or ::1.
func TestParseURLJudgesHostsAsABrowserReadsThem(t *testing.T) {
	blocked := []string{"127.0.0.1:7777"}
	for _, raw := range []string{
		"http://localhost.:8080/",
		"http://169.254.169.254./latest/meta-data/",
		"http://2130706433/",
		"http://127.1/",
		"http://0x7f000001/",
		"http://0177.0.0.1/",
		"http://0x7f.0.0.1/",
		"http://0/",
		"http://[::127.0.0.1]/",
		"http://[::ffff:127.0.0.1]/",
		"http://[64:ff9b::a9fe:a9fe]/",
		"http://[64:ff9b::10.0.0.1]/",
		"http://167772161/",
		"http://0xa9fea9fe/",
		"http://127.0.0.1.:7777/",
		"http://0x7f000001:7777/",
		// Numbers a browser refuses to read as an address are refused too.
		"http://4294967296/",
		"http://1.2.3.4.5/",
		"http://08/",
		"http://example.0x/",
	} {
		if u, err := ParseURL(raw, blocked, func() []int { return []int{5173} }); !errors.Is(err, ErrBlocked) {
			t.Errorf("ParseURL(%s) = %v, %v; want it refused", raw, u, err)
		}
	}
	for raw, want := range map[string]string{
		"https://example.com/":       "https://example.com/",
		"https://deadbeef/":          "https://deadbeef/",
		"https://example.com./":      "https://example.com./",
		"http://2130706433:5173/":    "http://127.0.0.1:5173/",
		"http://127.1:5173/x":        "http://127.0.0.1:5173/x",
		"http://localhost.:5173/":    "http://localhost.:5173/",
		"https://8.8.8.8/":           "https://8.8.8.8/",
		"https://134744072/":         "https://8.8.8.8/",
		"https://[2606:4700::1111]/": "https://[2606:4700::1111]/",
	} {
		u, err := ParseURL(raw, blocked, func() []int { return []int{5173} })
		if err != nil || u.String() != want {
			t.Errorf("ParseURL(%s) = %v, %v; want %s", raw, u, err, want)
		}
	}
}

func TestPublicSiteSkipsPreviewLookup(t *testing.T) {
	_, err := ParseURL("https://example.com/", nil, func() []int {
		t.Fatal("public site queried workspace preview ports")
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
}
