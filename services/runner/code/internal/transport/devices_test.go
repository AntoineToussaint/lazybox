package transport

import (
	"context"
	"net/url"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices/devicetest"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
)

type fixedAccess struct{ phone string }

func (a fixedAccess) Access(context.Context) *pb.RemoteAccess {
	return &pb.RemoteAccess{LocalUrl: "http://127.0.0.1:1", PhoneUrl: a.phone, Tailscale: &pb.TailscaleAccess{State: "serving"}}
}
func (a fixedAccess) SetTailscale(context.Context, bool) (*pb.RemoteAccess, error) {
	return a.Access(context.Background()), nil
}

func TestPairingNeedsARouteFromThePhone(t *testing.T) {
	f := newDeviceFixture(t)
	ctx := timeout(t)
	for _, phone := range []string{"", "http://laptop.example.ts.net:8443"} {
		f.service.Remote = fixedAccess{phone: phone}
		_, err := f.service.BeginDevicePairing(ctx, &pb.BeginDevicePairingRequest{})
		expectCode(t, "pairing over "+phone, err, codes.FailedPrecondition)
	}
}

// The QR link carries the one-time secret only in its fragment: a browser
// never sends a fragment, so the secret reaches no server or proxy log. Only
// BeginDevicePairing returns it.
func TestPairingLinkKeepsTheSecretInTheFragment(t *testing.T) {
	f := newDeviceFixture(t)
	ctx := timeout(t)
	f.service.Remote = fixedAccess{phone: "https://laptop.example.ts.net:8443"}
	offer, err := f.service.BeginDevicePairing(ctx, &pb.BeginDevicePairingRequest{Grants: []string{devices.GrantRespond}})
	if err != nil {
		t.Fatal(err)
	}
	link, err := url.Parse(offer.Url)
	if err != nil {
		t.Fatal(err)
	}
	pair := url.Values{}
	if fragment, err := url.ParseQuery(link.Fragment); err == nil {
		pair = fragment
	}
	id, secret, ok := strings.Cut(pair.Get("pair"), ".")
	if link.Scheme != "https" || link.Host != "laptop.example.ts.net:8443" || link.Query().Get("view") != "remote" || !ok || id != offer.Id || len(secret) < 40 {
		t.Fatalf("pairing link %q", offer.Url)
	}
	if strings.Contains(link.Path+link.RawQuery, secret) {
		t.Fatal("the secret is outside the fragment")
	}
	if strings.Join(offer.Grants, ",") != "read,respond" || offer.State != devices.OfferPending {
		t.Fatalf("offer %+v", offer)
	}
	status, err := f.service.GetDevicePairing(ctx, &pb.DevicePairingRequest{PairingId: offer.Id})
	if err != nil || status.Url != "" || status.State != devices.OfferPending {
		t.Fatalf("status %+v %v", status, err)
	}
	p, err := devicetest.New()
	if err != nil {
		t.Fatal(err)
	}
	if _, err = f.devices.Complete(p.PairRequest(id, secret, "iPhone")); err != nil {
		t.Fatal(err)
	}
	status, err = f.service.GetDevicePairing(ctx, &pb.DevicePairingRequest{PairingId: offer.Id})
	if err != nil || status.State != devices.OfferPaired || status.Device.GetName() != "iPhone" || status.Device.Fingerprint == "" {
		t.Fatalf("paired status %+v %v", status, err)
	}
	list, err := f.service.ListDevices(ctx, &pb.ListDevicesRequest{})
	if err != nil || len(list.Devices) != 1 || list.Devices[0].LastSeenAtUnixMillis == 0 {
		t.Fatalf("devices %+v %v", list, err)
	}
	list, err = f.service.RevokeDevice(ctx, &pb.RevokeDeviceRequest{DeviceId: list.Devices[0].Id})
	if err != nil || len(list.Devices) != 0 {
		t.Fatalf("after revoke %+v %v", list, err)
	}
	_, err = f.service.RevokeDevice(ctx, &pb.RevokeDeviceRequest{DeviceId: "gone"})
	expectCode(t, "revoking an unknown device", err, codes.NotFound)
	// Cancelling withdraws a pending offer.
	next, err := f.service.BeginDevicePairing(ctx, &pb.BeginDevicePairingRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if cancelled, err := f.service.CancelDevicePairing(ctx, &pb.DevicePairingRequest{PairingId: next.Id}); err != nil || cancelled.State != devices.OfferCancelled {
		t.Fatalf("cancel %+v %v", cancelled, err)
	}
	_, err = f.service.BeginDevicePairing(ctx, &pb.BeginDevicePairingRequest{Grants: []string{"settings"}})
	expectCode(t, "an unknown grant", err, codes.InvalidArgument)
}
