package transport

import (
	"context"
	"errors"
	"net/url"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// RemoteAccess is how a phone reaches this Runner (remote.Gateway implements
// it): the device gateway's address and the Tailscale route in front of it.
type RemoteAccess interface {
	Access(ctx context.Context) *pb.RemoteAccess
	SetTailscale(ctx context.Context, on bool) (*pb.RemoteAccess, error)
}

func (s *Service) devices() (*devices.Manager, error) {
	if s.Devices == nil {
		return nil, status.Error(codes.Unimplemented, "this Runner does not pair devices")
	}
	return s.Devices, nil
}

// GetRemoteAccess is the owner's alone, as is everything about devices.
func (s *Service) GetRemoteAccess(ctx context.Context, _ *pb.GetRemoteAccessRequest) (*pb.RemoteAccess, error) {
	if s.Remote == nil {
		return &pb.RemoteAccess{Problem: "Remote access is not available on this Runner.", Tailscale: &pb.TailscaleAccess{State: "not_installed"}}, nil
	}
	return s.Remote.Access(ctx), nil
}

func (s *Service) SetRemoteAccess(ctx context.Context, r *pb.SetRemoteAccessRequest) (*pb.RemoteAccess, error) {
	if s.Remote == nil {
		return nil, status.Error(codes.Unimplemented, "remote access is not available on this Runner")
	}
	access, err := s.Remote.SetTailscale(ctx, r.Tailscale)
	if err != nil {
		return nil, rpcError(err)
	}
	return access, nil
}

// BeginDevicePairing makes a one-time offer for the QR code. The URL carries
// the secret in its fragment, which a browser never sends to a server, so it
// reaches neither the gateway's nor a proxy's logs.
func (s *Service) BeginDevicePairing(ctx context.Context, r *pb.BeginDevicePairingRequest) (*pb.DevicePairing, error) {
	m, err := s.devices()
	if err != nil {
		return nil, err
	}
	access, err := s.GetRemoteAccess(ctx, nil)
	if err != nil {
		return nil, err
	}
	if access.PhoneUrl == "" {
		return nil, status.Error(codes.FailedPrecondition, "no route reaches this Runner from a phone yet; turn on Tailscale access in Settings → Devices first")
	}
	base, err := url.Parse(access.PhoneUrl)
	if err != nil || base.Scheme != "https" && !loopbackURL(base) {
		return nil, status.Error(codes.FailedPrecondition, "a phone may only pair over HTTPS")
	}
	offer, secret, err := m.Begin(r.Grants)
	if err != nil {
		if errors.Is(err, devices.ErrTooMany) {
			return nil, status.Error(codes.ResourceExhausted, err.Error())
		}
		return nil, status.Error(codes.InvalidArgument, err.Error())
	}
	link := *base
	link.Path = "/"
	link.RawQuery = url.Values{"view": {"remote"}}.Encode()
	link.Fragment = ""
	link.RawFragment = ""
	wire := wirePairing(offer)
	wire.Url = link.String() + "#" + url.Values{"pair": {offer.ID + "." + secret}, "grants": {strings.Join(offer.Grants, ",")}}.Encode()
	return wire, nil
}

// loopbackURL admits plain HTTP only to this machine, for local testing.
func loopbackURL(u *url.URL) bool {
	if u.Scheme != "http" {
		return false
	}
	switch u.Hostname() {
	case "127.0.0.1", "::1", "localhost":
		return true
	}
	return false
}

func (s *Service) GetDevicePairing(_ context.Context, r *pb.DevicePairingRequest) (*pb.DevicePairing, error) {
	m, err := s.devices()
	if err != nil {
		return nil, err
	}
	offer, err := m.Pairing(r.PairingId)
	if err != nil {
		return nil, status.Error(codes.NotFound, err.Error())
	}
	return wirePairing(offer), nil
}

func (s *Service) CancelDevicePairing(_ context.Context, r *pb.DevicePairingRequest) (*pb.DevicePairing, error) {
	m, err := s.devices()
	if err != nil {
		return nil, err
	}
	offer, err := m.Cancel(r.PairingId)
	if err != nil {
		return nil, status.Error(codes.NotFound, err.Error())
	}
	return wirePairing(offer), nil
}

func (s *Service) ListDevices(context.Context, *pb.ListDevicesRequest) (*pb.ListDevicesResponse, error) {
	m, err := s.devices()
	if err != nil {
		return nil, err
	}
	return wireDevices(m), nil
}

func (s *Service) RevokeDevice(_ context.Context, r *pb.RevokeDeviceRequest) (*pb.ListDevicesResponse, error) {
	m, err := s.devices()
	if err != nil {
		return nil, err
	}
	if err = m.Revoke(r.DeviceId); err != nil {
		if errors.Is(err, devices.ErrUnknownDevice) {
			return nil, status.Error(codes.NotFound, err.Error())
		}
		return nil, rpcError(err)
	}
	return wireDevices(m), nil
}

func wireDevices(m *devices.Manager) *pb.ListDevicesResponse {
	out := &pb.ListDevicesResponse{}
	for _, d := range m.List() {
		out.Devices = append(out.Devices, wireDevice(d))
	}
	return out
}

func wireDevice(d devices.Device) *pb.Device {
	return &pb.Device{Id: d.ID, Name: d.Name, Grants: d.Grants, PairedAtUnixMillis: d.PairedAt.UnixMilli(), LastSeenAtUnixMillis: d.LastSeenAt.UnixMilli(), Fingerprint: d.Fingerprint(), Push: wirePush(d.PushSettings())}
}

func wirePairing(o devices.Offer) *pb.DevicePairing {
	out := &pb.DevicePairing{Id: o.ID, State: o.State, ExpiresAtUnixMillis: o.ExpiresAt.UnixMilli(), Grants: o.Grants}
	if o.Device != nil {
		out.Device = wireDevice(*o.Device)
	}
	return out
}
