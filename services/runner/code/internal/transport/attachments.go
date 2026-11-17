package transport

import (
	"context"
	"errors"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/attachments"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
)

// ReadAttachment serves the bytes of an image or a text file a sent message
// carried, for the conversation's attachment boxes and their previews. Agent and terminal credentials cannot call it
// (auth.go); a paired phone can, with its read grant (device_auth.go).
func (s *Service) ReadAttachment(_ context.Context, r *pb.ReadAttachmentRequest) (*pb.AttachmentContent, error) {
	file, data, err := s.Bench.Attachments.Read(r.Path)
	switch {
	case errors.Is(err, attachments.ErrNotAttachment):
		return nil, grpcError(codes.InvalidArgument, err)
	case errors.Is(err, attachments.ErrTooLarge):
		return nil, grpcError(codes.ResourceExhausted, err)
	case err != nil:
		return nil, rpcError(err)
	}
	return &pb.AttachmentContent{Name: file.Name, MediaType: file.MediaType, Data: data}, nil
}
