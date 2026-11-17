package transport

import (
	"context"
	"errors"
	"net/url"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/webpush"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// PushNotifier sends a paired phone its notifications (remote.Notifier).
type PushNotifier interface {
	// PublicKey is the VAPID key a phone subscribes with.
	PublicKey() string
	// Test sends one phone a test notification and waits for the push service.
	Test(ctx context.Context, deviceID string) error
}

var errNoPush = status.Error(codes.Unimplemented, "this Runner does not send push notifications")

// Sessions is every conversation this Runner holds, as Info lists them: what
// the push notifier observes. It reads no profiles, so it is cheap to poll.
func (s *Service) Sessions(ctx context.Context) ([]*pb.Session, error) {
	var out []*pb.Session
	for _, e := range s.Bench.List() {
		view, err := s.sessionView(ctx, e)
		if err != nil {
			return nil, err
		}
		out = append(out, view)
	}
	return out, nil
}

func wirePush(p devices.Push) *pb.PushSettings {
	out := &pb.PushSettings{Subscribed: p.Subscription != nil, LastError: p.LastError, Preferences: &pb.PushPreferences{
		NeedsInput: p.NeedsInput, Failed: p.Failed, Review: p.Review, QuietStart: p.QuietStart, QuietEnd: p.QuietEnd, TimeZone: p.TimeZone,
	}}
	if !p.LastSentAt.IsZero() {
		out.LastSentAtUnixMillis = p.LastSentAt.UnixMilli()
	}
	if p.Subscription != nil {
		if u, err := url.Parse(p.Subscription.Endpoint); err == nil {
			out.Service = u.Hostname()
		}
	}
	return out
}

// pushDevice is the phone calling, for the RPCs that are a phone's own.
func (s *Service) pushDevice(ctx context.Context) (*devices.Manager, *devices.Device, error) {
	m, err := s.devices()
	if err != nil {
		return nil, nil, err
	}
	if s.Push == nil {
		return nil, nil, errNoPush
	}
	d := callerDevice(ctx)
	if d == nil {
		return nil, nil, status.Error(codes.FailedPrecondition, "only a paired phone has push settings; open Lazybox on the phone")
	}
	return m, d, nil
}

// GetPushSettings is a phone's own notification settings, with the key it
// subscribes with.
func (s *Service) GetPushSettings(ctx context.Context, _ *pb.GetPushSettingsRequest) (*pb.PushSettings, error) {
	m, caller, err := s.pushDevice(ctx)
	if err != nil {
		return nil, err
	}
	d, err := m.Device(caller.ID)
	if err != nil {
		return nil, status.Error(codes.Unauthenticated, err.Error())
	}
	out := wirePush(d.PushSettings())
	out.ApplicationServerKey = s.Push.PublicKey()
	return out, nil
}

// SetPushSettings changes a phone's own subscription and preferences. A phone
// can change no other device's.
func (s *Service) SetPushSettings(ctx context.Context, r *pb.SetPushSettingsRequest) (*pb.PushSettings, error) {
	m, caller, err := s.pushDevice(ctx)
	if err != nil {
		return nil, err
	}
	change := devices.PushChange{Unsubscribe: r.Unsubscribe}
	if r.Subscription != nil {
		change.Subscription = &webpush.Subscription{Endpoint: r.Subscription.Endpoint, P256dh: r.Subscription.P256Dh, Auth: r.Subscription.Auth}
	}
	if p := r.Preferences; p != nil {
		change.Preferences = &devices.PushPreferences{NeedsInput: p.NeedsInput, Failed: p.Failed, Review: p.Review, QuietStart: p.QuietStart, QuietEnd: p.QuietEnd, TimeZone: p.TimeZone}
	}
	push, err := m.SetPush(caller.ID, change)
	if errors.Is(err, devices.ErrUnknownDevice) {
		return nil, status.Error(codes.Unauthenticated, err.Error())
	}
	if errors.Is(err, devices.ErrPushChanges) {
		return nil, status.Error(codes.ResourceExhausted, err.Error())
	}
	if err != nil {
		return nil, status.Error(codes.InvalidArgument, err.Error())
	}
	out := wirePush(push)
	out.ApplicationServerKey = s.Push.PublicKey()
	return out, nil
}

// SendTestPush sends a test notification: the owner to any paired phone, a
// phone to itself only, within the phone's notification budget. No other
// credential (a hook's, say, which otherwise reaches what a phone reaches)
// may send one.
func (s *Service) SendTestPush(ctx context.Context, r *pb.SendTestPushRequest) (*pb.SendTestPushResponse, error) {
	m, err := s.devices()
	if err != nil {
		return nil, err
	}
	if s.Push == nil {
		return nil, errNoPush
	}
	if who, ok := ctx.Value(principalKey{}).(principal); ok && !who.owner {
		return nil, status.Error(codes.PermissionDenied, "only the owner or the phone itself sends a test notification")
	}
	target := r.DeviceId
	if caller := callerDevice(ctx); caller != nil {
		if target != "" && target != caller.ID {
			return nil, status.Error(codes.PermissionDenied, "a phone can only send itself a test notification")
		}
		target = caller.ID
	} else if target == "" {
		return nil, status.Error(codes.InvalidArgument, "name the phone to send a test notification to")
	}
	if _, err = m.Device(target); err != nil {
		return nil, status.Error(codes.NotFound, err.Error())
	}
	if err = s.Push.Test(ctx, target); err != nil {
		message := "The test notification was not delivered: " + err.Error()
		switch {
		case errors.Is(err, webpush.ErrGone):
			message = "This phone's notification subscription ended; turn notifications on again on the phone."
		case errors.Is(err, devices.ErrUnknownDevice):
			return nil, status.Error(codes.NotFound, err.Error())
		case errors.Is(err, devices.ErrPushBudget):
			return nil, status.Error(codes.ResourceExhausted, err.Error())
		case errors.Is(err, devices.ErrPushOff):
			message = "Notifications are off on this phone; turn them on there first."
		}
		return nil, status.Error(codes.FailedPrecondition, message)
	}
	d, err := m.Device(target)
	if err != nil {
		return nil, status.Error(codes.NotFound, err.Error())
	}
	return &pb.SendTestPushResponse{Push: wirePush(d.PushSettings())}, nil
}
