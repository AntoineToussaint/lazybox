package transport

import (
	"context"
	"errors"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/webpush"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/webpush/webpushtest"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
)

type fakePush struct {
	tested []string
	err    error
}

func (f *fakePush) PublicKey() string { return "BRunnerVapidKey" }
func (f *fakePush) Test(_ context.Context, device string) error {
	f.tested = append(f.tested, device)
	return f.err
}

func pushSubscription(endpoint string) *pb.PushSubscription {
	b := webpushtest.NewBrowser()
	return &pb.PushSubscription{Endpoint: endpoint, P256Dh: b.P256dh(), Auth: b.Auth()}
}

// A phone, even a read-only one, reads and sets its own notifications.
func TestPhoneSetsItsOwnPush(t *testing.T) {
	f := newDeviceFixture(t)
	push := &fakePush{}
	f.service.Push = push
	ctx := timeout(t)
	reader, token := f.pair(t)
	other, _ := f.pair(t, devices.GrantRespond)

	got, err := f.client.GetPushSettings(bearer(ctx, token), &pb.GetPushSettingsRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if got.ApplicationServerKey != "BRunnerVapidKey" || got.Subscribed || !got.Preferences.NeedsInput || !got.Preferences.Failed || got.Preferences.Review {
		t.Fatalf("defaults %+v", got)
	}
	sub := pushSubscription("https://web.push.apple.com/QAbc")
	got, err = f.client.SetPushSettings(bearer(ctx, token), &pb.SetPushSettingsRequest{Subscription: sub, Preferences: &pb.PushPreferences{NeedsInput: true, Review: true, QuietStart: "22:00", QuietEnd: "07:00", TimeZone: "Europe/Paris"}})
	if err != nil {
		t.Fatal(err)
	}
	if !got.Subscribed || got.Service != "web.push.apple.com" || got.Preferences.Failed || !got.Preferences.Review || got.Preferences.TimeZone != "Europe/Paris" {
		t.Fatalf("after setting %+v", got)
	}
	// Only its own device changed.
	if d, _ := f.devices.Device(other.ID); d.Push != nil {
		t.Fatal("another device's push changed")
	}
	if d, _ := f.devices.Device(reader.ID); d.Push == nil || d.Push.Subscription.Endpoint != sub.Endpoint {
		t.Fatal("not stored")
	}
	// An endpoint that is not a push service is refused: the Runner would post to it.
	_, err = f.client.SetPushSettings(bearer(ctx, token), &pb.SetPushSettingsRequest{Subscription: pushSubscription("https://127.0.0.1:1/lazybox.bench.v1.RunnerService/Info")})
	expectCode(t, "loopback endpoint", err, codes.InvalidArgument)

	// The owner sees it on the device, without the Runner's key.
	list, err := f.service.ListDevices(ctx, &pb.ListDevicesRequest{})
	if err != nil {
		t.Fatal(err)
	}
	for _, d := range list.Devices {
		if d.Push == nil || d.Push.ApplicationServerKey != "" {
			t.Fatalf("listing %+v", d.Push)
		}
		if d.Id == reader.ID && (!d.Push.Subscribed || d.Push.Service != "web.push.apple.com") {
			t.Fatalf("listing %+v", d.Push)
		}
	}

	// A phone tests itself, never another device.
	if _, err = f.client.SendTestPush(bearer(ctx, token), &pb.SendTestPushRequest{}); err != nil {
		t.Fatal(err)
	}
	_, err = f.client.SendTestPush(bearer(ctx, token), &pb.SendTestPushRequest{DeviceId: other.ID})
	expectCode(t, "a phone tested another device", err, codes.PermissionDenied)
	// The owner names the phone.
	if _, err = f.service.SendTestPush(ctx, &pb.SendTestPushRequest{DeviceId: other.ID}); err != nil {
		t.Fatal(err)
	}
	_, err = f.service.SendTestPush(ctx, &pb.SendTestPushRequest{})
	expectCode(t, "owner test without a device", err, codes.InvalidArgument)
	if len(push.tested) != 2 || push.tested[0] != reader.ID || push.tested[1] != other.ID {
		t.Fatalf("tested %v", push.tested)
	}
	// A failed delivery says why, in words.
	push.err = webpush.ErrGone
	_, err = f.client.SendTestPush(bearer(ctx, token), &pb.SendTestPushRequest{})
	expectCode(t, "ended subscription", err, codes.FailedPrecondition)
	push.err = devices.ErrPushOff
	_, err = f.client.SendTestPush(bearer(ctx, token), &pb.SendTestPushRequest{})
	expectCode(t, "notifications off", err, codes.FailedPrecondition)
	push.err = errors.New("the push service failed (503)")
	_, err = f.service.SendTestPush(ctx, &pb.SendTestPushRequest{DeviceId: "nobody"})
	expectCode(t, "unknown device", err, codes.NotFound)

	// The owner has no push settings of their own.
	_, err = f.service.GetPushSettings(ctx, &pb.GetPushSettingsRequest{})
	expectCode(t, "owner read push settings", err, codes.FailedPrecondition)
	_, err = f.service.SetPushSettings(ctx, &pb.SetPushSettingsRequest{Unsubscribe: true})
	expectCode(t, "owner set push settings", err, codes.FailedPrecondition)

	// Revoking the phone ends its subscription and its access.
	if _, err = f.service.RevokeDevice(ctx, &pb.RevokeDeviceRequest{DeviceId: reader.ID}); err != nil {
		t.Fatal(err)
	}
	if len(f.devices.PushTargets()) != 0 {
		t.Fatal("a revoked phone is still notified")
	}
	_, err = f.client.GetPushSettings(bearer(ctx, token), &pb.GetPushSettingsRequest{})
	expectCode(t, "revoked phone read push", err, codes.Unauthenticated)
}

// Unsubscribing keeps the preferences; a Runner without push says so.
func TestPushUnsubscribeAndUnavailable(t *testing.T) {
	f := newDeviceFixture(t)
	ctx := timeout(t)
	_, token := f.pair(t)
	_, err := f.client.GetPushSettings(bearer(ctx, token), &pb.GetPushSettingsRequest{})
	expectCode(t, "no push on this Runner", err, codes.Unimplemented)
	f.service.Push = &fakePush{}
	if _, err = f.client.SetPushSettings(bearer(ctx, token), &pb.SetPushSettingsRequest{Subscription: pushSubscription("https://fcm.googleapis.com/fcm/send/x"), Preferences: &pb.PushPreferences{Failed: true}}); err != nil {
		t.Fatal(err)
	}
	got, err := f.client.SetPushSettings(bearer(ctx, token), &pb.SetPushSettingsRequest{Unsubscribe: true})
	if err != nil {
		t.Fatal(err)
	}
	if got.Subscribed || got.Preferences.NeedsInput || !got.Preferences.Failed {
		t.Fatalf("%+v", got)
	}
}
