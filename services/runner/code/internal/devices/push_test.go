package devices_test

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/webpush"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/webpush/webpushtest"
)

func subscription(endpoint string) *webpush.Subscription {
	b := webpushtest.NewBrowser()
	return &webpush.Subscription{Endpoint: endpoint, P256dh: b.P256dh(), Auth: b.Auth()}
}

func TestPushDefaultsMirrorTheDesktop(t *testing.T) {
	m, _ := open(t, t.TempDir())
	d := pair(t, m, phone(t))
	got := d.PushSettings()
	if got.Subscription != nil || !got.NeedsInput || !got.Failed || got.Review {
		t.Fatalf("defaults %+v", got)
	}
	// Subscribing keeps the defaults.
	push, err := m.SetPush(d.ID, devices.PushChange{Subscription: subscription("https://web.push.apple.com/QAbc")})
	if err != nil {
		t.Fatal(err)
	}
	if push.Subscription == nil || !push.NeedsInput || !push.Failed || push.Review {
		t.Fatalf("after subscribing %+v", push)
	}
}

func TestPushIsStoredWithTheDeviceAndEndsWithIt(t *testing.T) {
	state := t.TempDir()
	m, _ := open(t, state)
	d := pair(t, m, phone(t))
	other := pair(t, m, phone(t))
	sub := subscription("https://fcm.googleapis.com/fcm/send/abc")
	prefs := devices.PushPreferences{NeedsInput: true, Review: true, QuietStart: "22:00", QuietEnd: "07:30", TimeZone: "Europe/Paris"}
	if _, err := m.SetPush(d.ID, devices.PushChange{Subscription: sub, Preferences: &prefs}); err != nil {
		t.Fatal(err)
	}
	if targets := m.PushTargets(); len(targets) != 1 || targets[0].ID != d.ID {
		t.Fatalf("targets %+v", targets)
	}
	info, err := os.Stat(filepath.Join(state, "devices.json"))
	if err != nil || info.Mode().Perm() != 0o600 {
		t.Fatalf("devices.json mode %v %v", info.Mode(), err)
	}
	// It survives a restart.
	again, _ := open(t, state)
	got, err := again.Device(d.ID)
	if err != nil {
		t.Fatal(err)
	}
	p := got.PushSettings()
	if p.Subscription == nil || p.Subscription.Endpoint != sub.Endpoint || p.Subscription.P256dh != sub.P256dh || p.PushPreferences != prefs {
		t.Fatalf("after restart %+v", p)
	}
	// A copy handed out cannot change the record.
	got.Push.Subscription.Endpoint = "https://evil.example/"
	if fresh, _ := again.Device(d.ID); fresh.Push.Subscription.Endpoint != sub.Endpoint {
		t.Fatal("a returned device aliases the stored record")
	}
	// Revoking the device removes its subscription, also on disk.
	if err = again.Revoke(d.ID); err != nil {
		t.Fatal(err)
	}
	if len(again.PushTargets()) != 0 {
		t.Fatal("a revoked device is still a push target")
	}
	data, _ := os.ReadFile(filepath.Join(state, "devices.json"))
	if strings.Contains(string(data), sub.Endpoint) || strings.Contains(string(data), sub.Auth) {
		t.Fatal("the revoked device's subscription is still on disk")
	}
	if _, err = again.SetPush(d.ID, devices.PushChange{Subscription: sub}); !errors.Is(err, devices.ErrUnknownDevice) {
		t.Fatalf("a revoked device subscribed: %v", err)
	}
	if _, err = again.Device(other.ID); err != nil {
		t.Fatal(err)
	}
}

func TestPushRefusesBadInput(t *testing.T) {
	m, _ := open(t, t.TempDir())
	d := pair(t, m, phone(t))
	good := subscription("https://web.push.apple.com/QAbc")
	cases := map[string]devices.PushChange{
		"loopback endpoint": {Subscription: subscription("https://127.0.0.1:8443/lazybox.bench.v1.RunnerService/Info")},
		"tailnet endpoint":  {Subscription: subscription("https://laptop.tailnet.ts.net:8443/x")},
		"http endpoint":     {Subscription: subscription("http://web.push.apple.com/QAbc")},
		"short key":         {Subscription: &webpush.Subscription{Endpoint: good.Endpoint, P256dh: "BAAA", Auth: good.Auth}},
		"long secret":       {Subscription: &webpush.Subscription{Endpoint: good.Endpoint, P256dh: good.P256dh, Auth: good.Auth + good.Auth}},
		"both":              {Subscription: good, Unsubscribe: true},
		"bad clock":         {Preferences: &devices.PushPreferences{QuietStart: "25:00", QuietEnd: "07:00"}},
		"one end":           {Preferences: &devices.PushPreferences{QuietStart: "22:00"}},
		"unknown zone":      {Preferences: &devices.PushPreferences{QuietStart: "22:00", QuietEnd: "07:00", TimeZone: "Mars/Olympus"}},
		"local zone":        {Preferences: &devices.PushPreferences{TimeZone: "Local"}},
	}
	for name, change := range cases {
		if _, err := m.SetPush(d.ID, change); err == nil {
			t.Fatalf("%s was accepted", name)
		}
	}
	if len(m.PushTargets()) != 0 {
		t.Fatal("a refused change was stored")
	}
}

func TestRecordPushForgetsAnEndedSubscription(t *testing.T) {
	state := t.TempDir()
	m, _ := open(t, state)
	d := pair(t, m, phone(t))
	sub := subscription("https://web.push.apple.com/one")
	if _, err := m.SetPush(d.ID, devices.PushChange{Subscription: sub}); err != nil {
		t.Fatal(err)
	}
	at := time.Unix(1_800_000_100, 0)
	m.RecordPush(d.ID, sub.Endpoint, at, nil)
	got, _ := m.Device(d.ID)
	if !got.Push.LastSentAt.Equal(at) || got.Push.LastError != "" {
		t.Fatalf("%+v", got.Push)
	}
	m.RecordPush(d.ID, sub.Endpoint, at, fmt.Errorf("the push service failed (503)"))
	if got, _ = m.Device(d.ID); got.Push.LastError == "" || got.Push.Subscription == nil {
		t.Fatalf("a failure must be recorded and keep the subscription: %+v", got.Push)
	}
	// A delivery to a subscription the phone has since replaced changes nothing.
	newer := subscription("https://web.push.apple.com/two")
	if _, err := m.SetPush(d.ID, devices.PushChange{Subscription: newer}); err != nil {
		t.Fatal(err)
	}
	m.RecordPush(d.ID, sub.Endpoint, at, webpush.ErrGone)
	if got, _ = m.Device(d.ID); got.Push.Subscription == nil || got.Push.Subscription.Endpoint != newer.Endpoint {
		t.Fatal("an old subscription's end removed the new one")
	}
	m.RecordPush(d.ID, newer.Endpoint, at, webpush.ErrGone)
	if got, _ = m.Device(d.ID); got.Push.Subscription != nil || len(m.PushTargets()) != 0 {
		t.Fatal("an ended subscription must be forgotten")
	}
	again, _ := open(t, state)
	if len(again.PushTargets()) != 0 {
		t.Fatal("an ended subscription came back after a restart")
	}
}

func TestQuietHours(t *testing.T) {
	paris, err := time.LoadLocation("Europe/Paris")
	if err != nil {
		t.Fatal(err)
	}
	at := func(h, m int) time.Time { return time.Date(2026, 11, 16, h, m, 0, 0, paris) }
	night := devices.PushPreferences{QuietStart: "22:00", QuietEnd: "07:00", TimeZone: "Europe/Paris"}
	for _, c := range []struct {
		when  time.Time
		quiet bool
	}{{at(21, 59), false}, {at(22, 0), true}, {at(23, 30), true}, {at(3, 0), true}, {at(6, 59), true}, {at(7, 0), false}, {at(12, 0), false}} {
		if got := night.Quiet(c.when); got != c.quiet {
			t.Fatalf("%v quiet=%v", c.when, got)
		}
	}
	lunch := devices.PushPreferences{QuietStart: "12:00", QuietEnd: "13:00", TimeZone: "America/New_York"}
	// 12:30 in Paris is 06:30 in New York: not quiet there.
	if lunch.Quiet(at(12, 30)) {
		t.Fatal("quiet hours must be read in the phone's time zone")
	}
	if !lunch.Quiet(time.Date(2026, 11, 16, 12, 30, 0, 0, time.UTC).Add(5 * time.Hour)) {
		t.Fatal("12:30 in New York is quiet")
	}
	if (devices.PushPreferences{}).Quiet(at(3, 0)) || (devices.PushPreferences{QuietStart: "08:00", QuietEnd: "08:00"}).Quiet(at(8, 0)) {
		t.Fatal("no window, or an empty one, is never quiet")
	}
}

func TestWants(t *testing.T) {
	p := devices.PushPreferences{NeedsInput: true}
	if !p.Wants(devices.KindApproval) || !p.Wants(devices.KindQuestion) || p.Wants(devices.KindFailed) || p.Wants(devices.KindReview) || !p.Wants(devices.KindTest) || p.Wants("other") {
		t.Fatal("kinds map to the desktop's toggles")
	}
}
