package devices

import (
	"errors"
	"fmt"
	"slices"
	"strconv"
	"strings"
	"time"
	// Quiet hours are kept in the phone's IANA time zone, whatever zone
	// database this machine has.
	_ "time/tzdata"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/webpush"
)

// Push is a device's Web Push subscription and what it wants to be told
// (REMOTE.md). It lives on the device record, so revoking the device ends its
// notifications with it.
type Push struct {
	// Subscription is nil while notifications are off on the phone.
	Subscription *webpush.Subscription `json:"subscription,omitempty"`
	PushPreferences
	LastSentAt time.Time `json:"lastSentAt,omitzero"`
	// LastError is why the last delivery failed, if it did.
	LastError string `json:"lastError,omitempty"`
}

// PushPreferences are the kinds a phone is told about, mirroring the
// desktop's notification settings, and its quiet hours.
type PushPreferences struct {
	NeedsInput bool `json:"needsInput"`
	Failed     bool `json:"failed"`
	Review     bool `json:"review"`
	// QuietStart and QuietEnd are "HH:MM" in TimeZone; both empty for none.
	QuietStart string `json:"quietStart,omitempty"`
	QuietEnd   string `json:"quietEnd,omitempty"`
	TimeZone   string `json:"timeZone,omitempty"`
}

// ErrPushOff: the phone has not turned notifications on.
var ErrPushOff = errors.New("notifications are off on this phone; turn them on there first")

// ErrPushBudget: the phone already had as many notifications as it gets in
// a minute (the notifier's per-device budget), test pushes included.
var ErrPushBudget = errors.New("this phone was sent as many notifications as it gets in a minute; try again shortly")

// ErrPushChanges: the phone changed its notifications too often; each change
// rewrites the paired devices file.
var ErrPushChanges = errors.New("too many notification changes from this phone; try again in a minute")

// pushChangesPerMinute bounds SetPush per device.
const pushChangesPerMinute = 30

// DefaultPushPreferences are the desktop's defaults: needs input and failures,
// not finished turns.
var DefaultPushPreferences = PushPreferences{NeedsInput: true, Failed: true}

// Push kinds a notification can have.
const (
	KindApproval = "approval"
	KindQuestion = "question"
	KindFailed   = "failed"
	KindReview   = "review"
	KindTest     = "test"
	KindSummary  = "summary"
	// A tracked pull request needs the person (checks failed, changes
	// requested, conflicts) or is good news (checks passed, approved,
	// merged): told under the failures and the ready-for-review settings.
	KindPRFailed = "pr_failed"
	KindPRReady  = "pr_ready"
)

func (d Device) clone() Device {
	c := d
	c.Grants = append([]string(nil), d.Grants...)
	if d.Push != nil {
		p := *d.Push
		if d.Push.Subscription != nil {
			s := *d.Push.Subscription
			p.Subscription = &s
		}
		c.Push = &p
	}
	return c
}

// PushSettings is the device's push record, or the defaults when it has none.
func (d Device) PushSettings() Push {
	if d.Push == nil {
		return Push{PushPreferences: DefaultPushPreferences}
	}
	return *d.clone().Push
}

// Wants reports whether these preferences ask to be told about kind.
func (p PushPreferences) Wants(kind string) bool {
	switch kind {
	case KindApproval, KindQuestion:
		return p.NeedsInput
	case KindFailed, KindPRFailed:
		return p.Failed
	case KindReview, KindPRReady:
		return p.Review
	case KindSummary:
		return p.NeedsInput || p.Failed || p.Review
	case KindTest:
		return true
	}
	return false
}

func minutes(clock string) (int, bool) {
	if len(clock) != 5 || clock[2] != ':' {
		return 0, false
	}
	h, err1 := strconv.Atoi(clock[:2])
	m, err2 := strconv.Atoi(clock[3:])
	if err1 != nil || err2 != nil || h < 0 || h > 23 || m < 0 || m > 59 {
		return 0, false
	}
	return h*60 + m, true
}

// Quiet reports whether now falls in the quiet hours, in the phone's time
// zone. The window may wrap midnight (22:00–07:00); equal ends mean none.
func (p PushPreferences) Quiet(now time.Time) bool {
	start, ok1 := minutes(p.QuietStart)
	end, ok2 := minutes(p.QuietEnd)
	if !ok1 || !ok2 || start == end {
		return false
	}
	location := time.UTC
	if p.TimeZone != "" {
		if loc, err := time.LoadLocation(p.TimeZone); err == nil {
			location = loc
		}
	}
	local := now.In(location)
	at := local.Hour()*60 + local.Minute()
	if start < end {
		return at >= start && at < end
	}
	return at >= start || at < end
}

func (p PushPreferences) validate() error {
	if (p.QuietStart == "") != (p.QuietEnd == "") {
		return errors.New("quiet hours need both a start and an end")
	}
	if p.QuietStart != "" {
		if _, ok := minutes(p.QuietStart); !ok {
			return errors.New("quiet hours start must be HH:MM")
		}
		if _, ok := minutes(p.QuietEnd); !ok {
			return errors.New("quiet hours end must be HH:MM")
		}
	}
	if p.TimeZone == "" {
		return nil
	}
	// "Local" would be this computer's zone, not the phone's.
	if _, err := time.LoadLocation(p.TimeZone); len(p.TimeZone) > 64 || strings.EqualFold(p.TimeZone, "local") || err != nil {
		return fmt.Errorf("unknown time zone %q", p.TimeZone)
	}
	return nil
}

// CheckSubscription validates a phone's subscription: a known push service
// over HTTPS, and keys of the shapes RFC 8291 needs.
func CheckSubscription(s webpush.Subscription) error {
	if _, err := webpush.ValidateEndpoint(s.Endpoint); err != nil {
		return err
	}
	return webpush.CheckKeys(s.P256dh, s.Auth)
}

// PushChange is what a phone changes about its own notifications.
type PushChange struct {
	// Subscription replaces the device's subscription.
	Subscription *webpush.Subscription
	// Unsubscribe removes it.
	Unsubscribe bool
	// Preferences, when set, replace the device's preferences.
	Preferences *PushPreferences
}

// SetPush applies a phone's change to its own push record and persists it.
func (m *Manager) SetPush(id string, change PushChange) (Push, error) {
	if change.Subscription != nil && change.Unsubscribe {
		return Push{}, errors.New("subscribe or unsubscribe, not both")
	}
	if change.Subscription != nil {
		if err := CheckSubscription(*change.Subscription); err != nil {
			return Push{}, err
		}
	}
	if change.Preferences != nil {
		if err := change.Preferences.validate(); err != nil {
			return Push{}, err
		}
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	d := m.devices[id]
	if d == nil {
		return Push{}, ErrUnknownDevice
	}
	now := m.Now()
	recent := slices.DeleteFunc(m.pushChanges[id], func(at time.Time) bool { return now.Sub(at) >= time.Minute })
	if len(recent) >= pushChangesPerMinute {
		m.pushChanges[id] = recent
		return Push{}, ErrPushChanges
	}
	m.pushChanges[id] = append(recent, now)
	previous := d.Push
	next := d.PushSettings()
	if change.Subscription != nil {
		s := *change.Subscription
		next.Subscription, next.LastError = &s, ""
	}
	if change.Unsubscribe {
		next.Subscription, next.LastError = nil, ""
	}
	if change.Preferences != nil {
		next.PushPreferences = *change.Preferences
	}
	d.Push = &next
	if err := m.save(); err != nil {
		d.Push = previous
		return Push{}, err
	}
	return d.PushSettings(), nil
}

// PushTargets are the devices with a push subscription.
func (m *Manager) PushTargets() []Device {
	m.mu.Lock()
	defer m.mu.Unlock()
	var out []Device
	for _, d := range m.devices {
		if d.Push != nil && d.Push.Subscription != nil {
			out = append(out, d.clone())
		}
	}
	return out
}

// Device returns one paired device.
func (m *Manager) Device(id string) (Device, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	d := m.devices[id]
	if d == nil {
		return Device{}, ErrUnknownDevice
	}
	return d.clone(), nil
}

// RecordPush records a delivery's outcome. A subscription the push service no
// longer knows (webpush.ErrGone) is forgotten. The file is rewritten only when
// what it says changes, not on every delivery; endpoint is the subscription
// the delivery went to, so a newer one the phone made meanwhile is kept.
func (m *Manager) RecordPush(id, endpoint string, at time.Time, sendErr error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	d := m.devices[id]
	if d == nil || d.Push == nil || d.Push.Subscription == nil || d.Push.Subscription.Endpoint != endpoint {
		return
	}
	message := ""
	if sendErr != nil {
		message = sendErr.Error()
	}
	changed := message != d.Push.LastError
	d.Push.LastError = message
	if sendErr == nil {
		d.Push.LastSentAt = at
	}
	if errors.Is(sendErr, webpush.ErrGone) {
		d.Push.Subscription = nil
		changed = true
	}
	if changed {
		_ = m.save() // Advisory; the next change writes it again.
	}
}
