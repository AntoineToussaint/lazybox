package remote

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"net/url"
	"slices"
	"strconv"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/webpush"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// Notifier pushes "an agent needs you" to paired phones (REMOTE.md). Every
// few seconds it observes the Runner's conversations, derives what each asks
// of the person exactly as the desktop inbox does (attention.ts), and sends
// each phone that subscribed the kinds it asked for, outside its quiet hours.
//
// A payload is the conversation's title, the kind and a link into the phone
// app — never transcript, command or workspace text — and it is encrypted to
// the phone's key, so the push service relays what it cannot read.
type Notifier struct {
	Devices *devices.Manager
	Sender  Sender
	// Sessions lists the Runner's conversations, as Info does.
	Sessions func(ctx context.Context) ([]*pb.Session, error)
	Keys     *webpush.Keys
	Interval time.Duration
	Now      func() time.Time

	mu     sync.Mutex
	primed bool
	last   map[string]attention
	sent   map[string][]time.Time
	slots  chan struct{}
	wg     sync.WaitGroup
	cancel context.CancelFunc
	done   chan struct{}
	ctx    context.Context
}

// Sender delivers one encrypted message (webpush.Sender).
type Sender interface {
	Send(ctx context.Context, sub webpush.Subscription, payload []byte, opts webpush.Options) error
}

const (
	defaultInterval = 3 * time.Second
	// perDeviceBudget bounds notifications per phone per minute.
	perDeviceBudget = 8
	// burst: more notices than this in one observation become one summary.
	burst          = 3
	maxConcurrency = 4
	maxTitle       = 80
)

// Payload is everything a notification carries.
type Payload struct {
	Title string `json:"title"`
	Kind  string `json:"kind"`
	URL   string `json:"url"`
	Tag   string `json:"tag"`
}

// ErrNotSubscribed: the phone has not turned notifications on.
var ErrNotSubscribed = devices.ErrPushOff

// PublicKey is the VAPID key a phone subscribes with.
func (n *Notifier) PublicKey() string {
	if n.Keys == nil {
		return ""
	}
	return n.Keys.PublicKey()
}

func (n *Notifier) now() time.Time {
	if n.Now != nil {
		return n.Now()
	}
	return time.Now()
}

func (n *Notifier) init() {
	if n.last == nil {
		n.last = map[string]attention{}
		n.sent = map[string][]time.Time{}
		n.slots = make(chan struct{}, maxConcurrency)
	}
	if n.ctx == nil {
		n.ctx = context.Background()
	}
}

// Start observes until ctx ends or Stop.
func (n *Notifier) Start(ctx context.Context) {
	n.mu.Lock()
	n.init()
	ctx, n.cancel = context.WithCancel(ctx)
	n.ctx, n.done = ctx, make(chan struct{})
	interval := n.Interval
	if interval <= 0 {
		interval = defaultInterval
	}
	done := n.done
	n.mu.Unlock()
	go func() {
		defer close(done)
		ticker := time.NewTicker(interval)
		defer ticker.Stop()
		for {
			_ = n.Observe(ctx)
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
			}
		}
	}()
}

// Stop ends observing and waits for deliveries in flight.
func (n *Notifier) Stop() {
	n.mu.Lock()
	cancel, done := n.cancel, n.done
	n.mu.Unlock()
	if cancel != nil {
		cancel()
		<-done
	}
	n.wg.Wait()
}

// Flush waits for the deliveries in flight (tests).
func (n *Notifier) Flush() { n.wg.Wait() }

// attention is what one conversation asks of the person, as attention.ts
// derives it, reduced to what decides a notification.
type attention struct {
	state     string // needs-input, failed, working, ended
	kind      string // question or approval, for needs-input
	reason    string // "Interrupted" for a turn a restart stopped
	at        int64
	turnEnded int64
	requests  []string
	session   *pb.Session
}

var assistantPurposes = map[string]bool{"help": true, "planning": true, "summary": true}

func conversationKey(s *pb.Session) string {
	if s.ConversationId != "" {
		return s.ConversationId
	}
	return s.Id
}

func activityAt(s *pb.Session) int64 {
	at := s.CreatedAtUnixMillis
	for _, a := range s.Agents {
		at = max(at, a.UpdatedAtUnixMillis)
	}
	return at
}

func turnEndedAt(s *pb.Session) int64 {
	var root *pb.AgentActivity
	for _, a := range s.Agents {
		if a.Id == "root" {
			root = a
			break
		}
	}
	if root == nil {
		for _, a := range s.Agents {
			if a.ParentId == "" {
				root = a
				break
			}
		}
	}
	if root == nil {
		return 0
	}
	if root.TurnEndedAtUnixMillis > 0 {
		return root.TurnEndedAtUnixMillis
	}
	if root.LastTurnStatus != "" || root.Status == "done" || root.Status == "interrupted" || root.Status == "failed" {
		return root.UpdatedAtUnixMillis
	}
	return 0
}

func awaitingFirstMessage(s *pb.Session) bool {
	if s.Closed || s.Fault != "" || s.Busy || s.Queued > 0 || len(s.Pending) > 0 {
		return false
	}
	if s.Status != "starting" && s.Status != "ready" {
		return false
	}
	for _, a := range s.Agents {
		if a.Id == "root" {
			return a.LastTurnStatus == "" && a.TurnStartedAtUnixMillis == 0 && a.TurnEndedAtUnixMillis == 0 && a.Status != "running"
		}
	}
	return true
}

func derive(s *pb.Session) attention {
	a := attention{at: activityAt(s), turnEnded: turnEndedAt(s), session: s}
	if !s.Closed {
		for _, r := range s.Pending {
			a.requests = append(a.requests, r.RequestId)
		}
	}
	if !s.Closed && (len(s.Pending) > 0 || s.Status == "waiting_input" || s.Status == "waiting_approval") {
		a.state, a.kind = "needs-input", devices.KindApproval
		if len(s.Pending) > 0 && s.Pending[0].Type == "question" || len(s.Pending) == 0 && s.Status == "waiting_input" {
			a.kind = devices.KindQuestion
		}
		return a
	}
	fault := s.Fault != "" && s.Fault != "native_exit_unconfirmed"
	interrupted := false
	if s.Historical {
		for _, agent := range s.Agents {
			if agent.Status == "interrupted" {
				interrupted = true
			}
		}
	}
	if fault || s.Status == "failed" || interrupted {
		a.state = "failed"
		if !fault && s.Status != "failed" {
			a.reason = "Interrupted"
		}
		return a
	}
	if !s.Closed && !awaitingFirstMessage(s) && (s.Busy || s.Queued > 0 || s.Status == "running" || s.Status == "starting" || s.Status == "switching_profile") {
		a.state = "working"
		return a
	}
	a.state = "ended"
	return a
}

// notice is one thing worth telling, before any phone's preferences.
type notice struct {
	kind    string
	key     string
	session *pb.Session
}

// noticeFor ports attention.ts noticeFor: only a change this Runner watched
// happen counts, and only one with new agent activity behind it.
func noticeFor(previous, next attention) (notice, bool) {
	n := notice{key: conversationKey(next.session), session: next.session}
	switch next.state {
	case "needs-input":
		fresh := 0
		for _, id := range next.requests {
			if !slices.Contains(previous.requests, id) {
				fresh++
			}
		}
		if (previous.state == "needs-input" && fresh == 0) || (fresh == 0 && next.at <= previous.at) {
			return n, false
		}
		n.kind = next.kind
		return n, true
	case "failed":
		if previous.state == "failed" {
			return n, false
		}
		if next.reason == "Interrupted" {
			if previous.state != "working" {
				return n, false
			}
		} else if next.at <= previous.at {
			return n, false
		}
		n.kind = devices.KindFailed
		return n, true
	case "ended":
		if next.turnEnded > 0 && next.turnEnded > previous.turnEnded {
			n.kind = devices.KindReview
			return n, true
		}
	}
	return n, false
}

// Observe reads the conversations once and sends what changed. The first
// observation only records: a Runner restart never notifies again.
func (n *Notifier) Observe(ctx context.Context) error {
	if n.Sessions == nil || n.Devices == nil {
		return nil
	}
	// No phone listens: read nothing, and start afresh when one subscribes.
	if len(n.Devices.PushTargets()) == 0 {
		n.mu.Lock()
		n.primed, n.last = false, nil
		n.mu.Unlock()
		return nil
	}
	sessions, err := n.Sessions(ctx)
	if err != nil {
		return err
	}
	current := map[string]attention{}
	for _, s := range sessions {
		if assistantPurposes[s.Purpose] {
			continue
		}
		a := derive(s)
		key := conversationKey(s)
		if previous, ok := current[key]; !ok || a.at > previous.at {
			current[key] = a
		}
	}
	n.mu.Lock()
	n.init()
	primed, last := n.primed, n.last
	n.last, n.primed = current, true
	n.mu.Unlock()
	if !primed {
		return nil
	}
	var notices []notice
	for key, next := range current {
		// A conversation that appeared since the last look is compared with
		// nothing: a request it already holds is news.
		if found, ok := noticeFor(last[key], next); ok {
			notices = append(notices, found)
		}
	}
	slices.SortFunc(notices, func(a, b notice) int {
		if a.key < b.key {
			return -1
		}
		if a.key > b.key {
			return 1
		}
		return 0
	})
	if len(notices) == 0 {
		return nil
	}
	now := n.now()
	for _, d := range n.Devices.PushTargets() {
		push := d.PushSettings()
		if push.Quiet(now) {
			continue
		}
		var wanted []notice
		for _, found := range notices {
			if push.Wants(found.kind) {
				wanted = append(wanted, found)
			}
		}
		if len(wanted) == 0 {
			continue
		}
		var payload Payload
		var opts webpush.Options
		if len(wanted) > burst {
			payload = Payload{Title: strconv.Itoa(len(wanted)) + " conversations need you", Kind: devices.KindSummary, URL: "/?view=remote", Tag: "summary"}
			opts = webpush.Options{TTL: time.Hour, Urgency: "normal", Topic: "summary"}
			for _, found := range wanted {
				if found.kind == devices.KindApproval || found.kind == devices.KindQuestion {
					opts.Urgency = "high"
				}
			}
			n.deliver(d, *push.Subscription, payload, opts, now)
			continue
		}
		for _, found := range wanted {
			n.deliver(d, *push.Subscription, payloadFor(found), optionsFor(found), now)
		}
	}
	return nil
}

func payloadFor(found notice) Payload {
	title := found.session.Title
	if title == "" {
		title = "Untitled conversation"
	}
	if utf8.RuneCountInString(title) > maxTitle {
		runes := []rune(title)
		title = string(runes[:maxTitle-1]) + "…"
	}
	return Payload{Title: title, Kind: found.kind, URL: "/?view=remote&session=" + url.QueryEscape(found.session.Id), Tag: found.key}
}

func optionsFor(found notice) webpush.Options {
	sum := sha256.Sum256([]byte(found.key))
	opts := webpush.Options{TTL: time.Hour, Urgency: "normal", Topic: base64.RawURLEncoding.EncodeToString(sum[:])[:32]}
	switch found.kind {
	case devices.KindApproval, devices.KindQuestion:
		opts.Urgency = "high"
	case devices.KindReview:
		opts.TTL = 10 * time.Minute
	}
	return opts
}

// marshal writes the payload as JSON without HTML escaping, so the link's
// "&" stays as is; the phone parses it, nothing renders it as HTML.
func (p Payload) marshal() ([]byte, error) {
	var out bytes.Buffer
	encoder := json.NewEncoder(&out)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(p); err != nil {
		return nil, err
	}
	return bytes.TrimRight(out.Bytes(), "\n"), nil
}

// allow spends one of a phone's notifications for this minute.
func (n *Notifier) allow(device string, now time.Time) bool {
	n.mu.Lock()
	defer n.mu.Unlock()
	n.init()
	recent := n.sent[device][:0]
	for _, at := range n.sent[device] {
		if now.Sub(at) < time.Minute {
			recent = append(recent, at)
		}
	}
	if len(recent) >= perDeviceBudget {
		n.sent[device] = recent
		return false
	}
	n.sent[device] = append(recent, now)
	return true
}

// deliver sends off the observation loop, a few at a time, so a slow push
// service never delays the next observation.
func (n *Notifier) deliver(d devices.Device, sub webpush.Subscription, payload Payload, opts webpush.Options, now time.Time) {
	if !n.allow(d.ID, now) {
		return
	}
	body, err := payload.marshal()
	if err != nil {
		return
	}
	n.mu.Lock()
	ctx, slots := n.ctx, n.slots
	n.mu.Unlock()
	n.wg.Add(1)
	go func() {
		defer n.wg.Done()
		select {
		case slots <- struct{}{}:
		case <-ctx.Done():
			return
		}
		defer func() { <-slots }()
		err := n.Sender.Send(ctx, sub, body, opts)
		if ctx.Err() != nil {
			return
		}
		n.Devices.RecordPush(d.ID, sub.Endpoint, n.now(), err)
	}()
}

// PullRequestNotice is a tracked pull request's transition (runner/items).
type PullRequestNotice struct {
	// Event is pr.checks-failed, pr.approved, …
	Event      string
	Repository string
	Number     int64
	Title      string
	Reason     string
}

// pullRequestKinds: what each transition is, for a phone's settings; the
// others (opened, closed) are not pushed.
var pullRequestKinds = map[string]string{
	"pr.checks-failed":     devices.KindPRFailed,
	"pr.changes-requested": devices.KindPRFailed,
	"pr.conflicts":         devices.KindPRFailed,
	"pr.checks-passed":     devices.KindPRReady,
	"pr.approved":          devices.KindPRReady,
	"pr.merged":            devices.KindPRReady,
}

// PullRequest tells every phone that wants it about a tracked pull request's
// transition, outside its quiet hours: "#812 Fix rounding — checks failed:
// lint". It opens the phone's Inbox, where the reaction is one tap.
func (n *Notifier) PullRequest(notice PullRequestNotice) {
	kind, ok := pullRequestKinds[notice.Event]
	if !ok || n.Devices == nil || n.Sender == nil {
		return
	}
	n.mu.Lock()
	n.init()
	n.mu.Unlock()
	title := "#" + strconv.FormatInt(notice.Number, 10)
	if notice.Title != "" {
		title += " " + notice.Title
	}
	if utf8.RuneCountInString(title) > maxTitle {
		title = string([]rune(title)[:maxTitle-1]) + "…"
	}
	if notice.Reason != "" {
		title += " — " + notice.Reason
	}
	tag := "pr:" + notice.Repository + "#" + strconv.FormatInt(notice.Number, 10)
	payload := Payload{Title: title, Kind: kind, URL: "/?view=remote", Tag: tag}
	sum := sha256.Sum256([]byte(tag))
	opts := webpush.Options{TTL: time.Hour, Urgency: "normal", Topic: base64.RawURLEncoding.EncodeToString(sum[:])[:32]}
	now := n.now()
	for _, d := range n.Devices.PushTargets() {
		push := d.PushSettings()
		if push.Quiet(now) || !push.Wants(kind) {
			continue
		}
		n.deliver(d, *push.Subscription, payload, opts, now)
	}
}

// Test sends a test notification to one phone and waits for the push
// service's answer.
func (n *Notifier) Test(ctx context.Context, deviceID string) error {
	d, err := n.Devices.Device(deviceID)
	if err != nil {
		return err
	}
	push := d.PushSettings()
	if push.Subscription == nil {
		return ErrNotSubscribed
	}
	// A test is a notification like any other: it spends the phone's
	// per-minute budget, so it cannot be used to flood the phone.
	if !n.allow(d.ID, n.now()) {
		return devices.ErrPushBudget
	}
	body, err := Payload{Title: "Notifications work", Kind: devices.KindTest, URL: "/?view=remote", Tag: "test"}.marshal()
	if err != nil {
		return err
	}
	err = n.Sender.Send(ctx, *push.Subscription, body, webpush.Options{TTL: time.Minute, Urgency: "normal", Topic: "test"})
	n.Devices.RecordPush(d.ID, push.Subscription.Endpoint, n.now(), err)
	return err
}
