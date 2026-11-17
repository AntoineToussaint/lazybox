package remote

import (
	"context"
	"encoding/json"
	"errors"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices/devicetest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/webpush"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/webpush/webpushtest"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

type sent struct {
	endpoint string
	payload  Payload
	opts     webpush.Options
}

type fakeSender struct {
	mu   sync.Mutex
	got  []sent
	fail map[string]error
}

func (f *fakeSender) Send(_ context.Context, sub webpush.Subscription, body []byte, opts webpush.Options) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	var p Payload
	if err := json.Unmarshal(body, &p); err != nil {
		return err
	}
	var raw map[string]any
	_ = json.Unmarshal(body, &raw)
	if len(raw) != 4 {
		return errors.New("payload carries more than title, kind, url and tag")
	}
	f.got = append(f.got, sent{sub.Endpoint, p, opts})
	return f.fail[sub.Endpoint]
}

func (f *fakeSender) take() []sent {
	f.mu.Lock()
	defer f.mu.Unlock()
	out := f.got
	f.got = nil
	return out
}

type notifyFixture struct {
	n        *Notifier
	m        *devices.Manager
	sender   *fakeSender
	sessions []*pb.Session
	now      time.Time
}

func newNotifier(t *testing.T) *notifyFixture {
	t.Helper()
	m, err := devices.Open(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	f := &notifyFixture{m: m, sender: &fakeSender{fail: map[string]error{}}, now: time.Date(2026, 11, 16, 12, 0, 0, 0, time.UTC)}
	f.n = &Notifier{Devices: m, Sender: f.sender, Now: func() time.Time { return f.now }, Sessions: func(context.Context) ([]*pb.Session, error) { return f.sessions, nil }}
	return f
}

// phone pairs a device and subscribes it at endpoint.
func (f *notifyFixture) phone(t *testing.T, endpoint string, prefs *devices.PushPreferences) devices.Device {
	t.Helper()
	p, err := devicetest.New()
	if err != nil {
		t.Fatal(err)
	}
	offer, secret, err := f.m.Begin(nil)
	if err != nil {
		t.Fatal(err)
	}
	d, err := f.m.Complete(p.PairRequest(offer.ID, secret, "iPhone"))
	if err != nil {
		t.Fatal(err)
	}
	b := webpushtest.NewBrowser()
	if _, err = f.m.SetPush(d.ID, devices.PushChange{Subscription: &webpush.Subscription{Endpoint: endpoint, P256dh: b.P256dh(), Auth: b.Auth()}, Preferences: prefs}); err != nil {
		t.Fatal(err)
	}
	return d
}

func (f *notifyFixture) observe(t *testing.T) []sent {
	t.Helper()
	if err := f.n.Observe(context.Background()); err != nil {
		t.Fatal(err)
	}
	f.n.Flush()
	return f.sender.take()
}

func agents(status string, updated, ended int64) []*pb.AgentActivity {
	return []*pb.AgentActivity{{Id: "root", Status: status, UpdatedAtUnixMillis: updated, TurnEndedAtUnixMillis: ended}}
}

const apple = "https://web.push.apple.com/one"

func TestNotifierTellsANewRequestOnceAndNeverOnTheFirstLook(t *testing.T) {
	f := newNotifier(t)
	f.phone(t, apple, nil)
	s := &pb.Session{Id: "exec-1", ConversationId: "conv-1", Title: "Bump dependencies", Status: "running", Busy: true, Agents: agents("running", 1000, 0), LastMessage: "secret transcript line"}
	pending := &pb.Session{Id: "exec-9", Title: "Old request", Status: "waiting_approval", Pending: []*pb.AgentEvent{{Type: "approval", RequestId: "old"}}, Agents: agents("waiting_approval", 500, 0)}
	f.sessions = []*pb.Session{s, pending}
	if got := f.observe(t); len(got) != 0 {
		t.Fatalf("the first look notified: %+v", got)
	}
	// The agent asks for an approval.
	s.Status, s.Busy, s.Agents = "waiting_approval", false, agents("waiting_approval", 2000, 0)
	s.Pending = []*pb.AgentEvent{{Type: "approval", RequestId: "a-1", Text: "Bash", ProviderDataJson: []byte(`{"input":{"command":"rm -rf /secret/path"}}`)}}
	got := f.observe(t)
	if len(got) != 1 {
		t.Fatalf("%+v", got)
	}
	want := Payload{Title: "Bump dependencies", Kind: "approval", URL: "/?view=remote&session=exec-1", Tag: "conv-1"}
	if got[0].payload != want || got[0].opts.Urgency != "high" || got[0].opts.TTL != time.Hour || len(got[0].opts.Topic) != 32 {
		t.Fatalf("%+v", got[0])
	}
	// The same request again is not news; a second one is.
	if again := f.observe(t); len(again) != 0 {
		t.Fatalf("repeated: %+v", again)
	}
	s.Pending = append(s.Pending, &pb.AgentEvent{Type: "question", RequestId: "q-2"})
	if got = f.observe(t); len(got) != 1 || got[0].payload.Kind != "approval" {
		t.Fatalf("second request: %+v", got)
	}
	// A question first is a question.
	s.Pending = []*pb.AgentEvent{{Type: "question", RequestId: "q-3"}}
	if got = f.observe(t); len(got) != 1 || got[0].payload.Kind != "question" {
		t.Fatalf("question: %+v", got)
	}
}

func TestNotifierFailedAndReview(t *testing.T) {
	f := newNotifier(t)
	f.phone(t, apple, &devices.PushPreferences{NeedsInput: true, Failed: true, Review: true})
	s := &pb.Session{Id: "e", Title: "Deploy", Status: "running", Busy: true, Agents: agents("running", 1000, 0)}
	help := &pb.Session{Id: "h", Purpose: "help", Status: "running", Busy: true, Agents: agents("running", 1000, 0)}
	f.sessions = []*pb.Session{s, help}
	f.observe(t)
	// The turn ends: ready for review, with a shorter life.
	s.Status, s.Busy, s.Agents = "done", false, agents("done", 2000, 2000)
	help.Status, help.Busy, help.Agents = "done", false, agents("done", 2000, 2000)
	got := f.observe(t)
	if len(got) != 1 || got[0].payload.Kind != "review" || got[0].opts.TTL != 10*time.Minute || got[0].opts.Urgency != "normal" {
		t.Fatalf("review: %+v", got)
	}
	if f.observe(t) != nil {
		t.Fatal("review repeated")
	}
	// A new turn fails.
	s.Status, s.Busy, s.Agents = "running", true, agents("running", 3000, 2000)
	f.observe(t)
	s.Status, s.Busy, s.Fault, s.Agents = "failed", false, "provider_failure", agents("failed", 4000, 2000)
	if got = f.observe(t); len(got) != 1 || got[0].payload.Kind != "failed" {
		t.Fatalf("failed: %+v", got)
	}
	if f.observe(t) != nil {
		t.Fatal("failure repeated")
	}
}

func TestNotifierHonoursKindsQuietHoursAndRevoke(t *testing.T) {
	f := newNotifier(t)
	quiet := f.phone(t, "https://web.push.apple.com/quiet", &devices.PushPreferences{NeedsInput: true, QuietStart: "22:00", QuietEnd: "07:00", TimeZone: "Europe/Paris"})
	noInput := f.phone(t, "https://fcm.googleapis.com/fcm/send/failures-only", &devices.PushPreferences{Failed: true})
	gone := f.phone(t, "https://web.push.apple.com/revoked", nil)
	s := &pb.Session{Id: "e", Title: "T", Status: "running", Busy: true, Agents: agents("running", 1000, 0)}
	f.sessions = []*pb.Session{s}
	f.observe(t)
	if err := f.m.Revoke(gone.ID); err != nil {
		t.Fatal(err)
	}
	// 23:30 in Paris: the quiet phone hears nothing; the other does not want input.
	f.now = time.Date(2026, 11, 16, 22, 30, 0, 0, time.UTC)
	s.Pending, s.Agents = []*pb.AgentEvent{{Type: "approval", RequestId: "a"}}, agents("waiting_approval", 2000, 0)
	if got := f.observe(t); len(got) != 0 {
		t.Fatalf("sent %+v", got)
	}
	// 09:00 in Paris.
	f.now = time.Date(2026, 11, 17, 8, 0, 0, 0, time.UTC)
	s.Pending = append(s.Pending, &pb.AgentEvent{Type: "approval", RequestId: "b"})
	got := f.observe(t)
	if len(got) != 1 || got[0].endpoint != "https://web.push.apple.com/quiet" {
		t.Fatalf("%+v", got)
	}
	_ = quiet
	_ = noInput
}

func TestNotifierSummarisesABurstAndRateLimits(t *testing.T) {
	f := newNotifier(t)
	f.phone(t, apple, nil)
	var all []*pb.Session
	for i := range 16 {
		id := string(rune('a' + i))
		all = append(all, &pb.Session{Id: id, Title: "T" + id, Status: "running", Busy: true, Agents: agents("running", 1000, 0)})
	}
	f.sessions = all
	f.observe(t)
	for _, s := range all[:5] {
		s.Pending, s.Agents = []*pb.AgentEvent{{Type: "approval", RequestId: "r" + s.Id}}, agents("waiting_approval", 2000, 0)
	}
	got := f.observe(t)
	if len(got) != 1 || got[0].payload != (Payload{Title: "5 conversations need you", Kind: "summary", URL: "/?view=remote", Tag: "summary"}) || got[0].opts.Urgency != "high" {
		t.Fatalf("burst: %+v", got)
	}
	// One at a time, eight a minute at most.
	count := 1
	for i, s := range all[5:] {
		s.Pending, s.Agents = []*pb.AgentEvent{{Type: "approval", RequestId: "r" + s.Id}}, agents("waiting_approval", 3000+int64(i), 0)
		count += len(f.observe(t))
	}
	if count != 8 {
		t.Fatalf("%d notifications in a minute", count)
	}
	f.now = f.now.Add(time.Minute)
	all[0].Pending = append(all[0].Pending, &pb.AgentEvent{Type: "question", RequestId: "later"})
	if got = f.observe(t); len(got) != 1 {
		t.Fatalf("after a minute: %+v", got)
	}
}

func TestNotifierForgetsAnEndedSubscription(t *testing.T) {
	f := newNotifier(t)
	d := f.phone(t, apple, nil)
	f.sender.fail[apple] = webpush.ErrGone
	s := &pb.Session{Id: "e", Status: "running", Busy: true, Agents: agents("running", 1000, 0)}
	f.sessions = []*pb.Session{s}
	f.observe(t)
	s.Pending, s.Agents = []*pb.AgentEvent{{Type: "question", RequestId: "q"}}, agents("waiting_input", 2000, 0)
	if got := f.observe(t); len(got) != 1 || got[0].payload.Title != "Untitled conversation" {
		t.Fatalf("%+v", got)
	}
	if len(f.m.PushTargets()) != 0 {
		t.Fatal("an ended subscription is still a target")
	}
	if err := f.n.Test(context.Background(), d.ID); !errors.Is(err, ErrNotSubscribed) {
		t.Fatalf("test to an unsubscribed phone: %v", err)
	}
}

func TestNotifierTitleIsBounded(t *testing.T) {
	p := payloadFor(notice{kind: "approval", key: "k", session: &pb.Session{Id: "a b", Title: strings.Repeat("é", 200)}})
	if n := len([]rune(p.Title)); n != maxTitle || p.URL != "/?view=remote&session=a+b" {
		t.Fatalf("%d %s", n, p.URL)
	}
}

// End to end: the real sender, VAPID and encryption, against a fake push service.
type rewrite struct {
	*webpush.Sender
	to string
}

func (r rewrite) Send(ctx context.Context, sub webpush.Subscription, body []byte, opts webpush.Options) error {
	sub.Endpoint = r.to + strings.TrimPrefix(sub.Endpoint, "https://web.push.apple.com")
	return r.Sender.Send(ctx, sub, body, opts)
}

func TestNotifierSendsEncryptedVAPIDPushes(t *testing.T) {
	f := newNotifier(t)
	keys, err := webpush.LoadOrCreateKey(filepath.Join(t.TempDir(), "vapid.key"))
	if err != nil {
		t.Fatal(err)
	}
	service := webpushtest.NewService(keys.PublicKey())
	defer service.Close()
	browser := webpushtest.NewBrowser()
	service.Subscribe("phone", browser)
	sender := webpush.NewSender(keys)
	sender.Client, sender.Validate = service.Client(), func(string) (string, error) { return "127.0.0.1", nil }
	f.n.Sender, f.n.Keys = rewrite{sender, service.URL}, keys
	if f.n.PublicKey() != keys.PublicKey() {
		t.Fatal("public key")
	}
	d := f.phone(t, "https://web.push.apple.com/phone", nil)
	// The phone's subscription must hold this browser's keys.
	if _, err = f.m.SetPush(d.ID, devices.PushChange{Subscription: &webpush.Subscription{Endpoint: "https://web.push.apple.com/phone", P256dh: browser.P256dh(), Auth: browser.Auth()}}); err != nil {
		t.Fatal(err)
	}
	if err = f.n.Test(context.Background(), d.ID); err != nil {
		t.Fatal(err, service.Failures)
	}
	msgs := service.Messages()
	if len(msgs) != 1 || string(msgs[0].Payload) != `{"title":"Notifications work","kind":"test","url":"/?view=remote","tag":"test"}` {
		t.Fatalf("%+v %v", msgs, service.Failures)
	}
	got, _ := f.m.Device(d.ID)
	if got.Push.LastSentAt.IsZero() || got.Push.LastError != "" {
		t.Fatalf("%+v", got.Push)
	}
	// Start and Stop run the loop; a request while it runs is delivered.
	s := &pb.Session{Id: "e", Title: "Ship it", Status: "running", Busy: true, Agents: agents("running", 1000, 0), LastMessage: "do not leak me"}
	var mu sync.Mutex
	f.n.Sessions = func(context.Context) ([]*pb.Session, error) {
		mu.Lock()
		defer mu.Unlock()
		return []*pb.Session{s}, nil
	}
	f.n.Interval = 20 * time.Millisecond
	f.n.Start(context.Background())
	time.Sleep(60 * time.Millisecond)
	mu.Lock()
	s = &pb.Session{Id: "e", Title: "Ship it", Status: "waiting_input", Pending: []*pb.AgentEvent{{Type: "question", RequestId: "q", Text: "Which version?"}}, Agents: agents("waiting_input", 2000, 0), LastMessage: "do not leak me"}
	mu.Unlock()
	deadline := time.Now().Add(5 * time.Second)
	for len(service.Messages()) < 2 && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}
	f.n.Stop()
	msgs = service.Messages()
	if len(msgs) != 2 || string(msgs[1].Payload) != `{"title":"Ship it","kind":"question","url":"/?view=remote&session=e","tag":"e"}` {
		t.Fatalf("%d messages: %+v %v", len(msgs), msgs, service.Failures)
	}
	if strings.Contains(string(msgs[1].Payload), "leak") || strings.Contains(string(msgs[1].Payload), "Which version") {
		t.Fatal("the payload carries conversation content")
	}
	if msgs[1].Header.Get("Urgency") != "high" {
		t.Fatal(msgs[1].Header)
	}
}
