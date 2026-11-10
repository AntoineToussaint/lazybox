package transport

import (
	"context"
	"crypto/subtle"
	"strings"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"
)

// LocalAuthority protects the experimental Runner's execution authority on
// every protocol. Codefly's Connect listener invokes these same interceptors.
func LocalAuthority(token string) []grpc.ServerOption {
	check := func(ctx context.Context, method string) error {
		if strings.HasPrefix(method, "/grpc.health.v1.Health/") {
			return nil
		}
		md, _ := metadata.FromIncomingContext(ctx)
		values := md.Get("authorization")
		if len(values) != 1 || subtle.ConstantTimeCompare([]byte(values[0]), []byte("Bearer "+token)) != 1 {
			return status.Error(codes.Unauthenticated, "local test bench bearer required")
		}
		return nil
	}
	return []grpc.ServerOption{
		grpc.ChainUnaryInterceptor(func(ctx context.Context, req any, info *grpc.UnaryServerInfo, handler grpc.UnaryHandler) (any, error) {
			if err := check(ctx, info.FullMethod); err != nil {
				return nil, err
			}
			return handler(ctx, req)
		}),
		grpc.ChainStreamInterceptor(func(srv any, stream grpc.ServerStream, info *grpc.StreamServerInfo, handler grpc.StreamHandler) error {
			if err := check(stream.Context(), info.FullMethod); err != nil {
				return err
			}
			return handler(srv, stream)
		}),
	}
}
