package transport

import (
	"context"
	"encoding/json"
	"errors"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/telemetry"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/protobuf/encoding/protojson"
)

func (s *Service) GetTelemetry(ctx context.Context, r *pb.TelemetryRequest) (*pb.TelemetryReport, error) {
	scopes := 0
	for _, value := range []string{r.Workspace, r.Execution, r.Conversation} {
		if value != "" {
			scopes++
		}
	}
	if scopes != 1 {
		return nil, grpcError(codes.InvalidArgument, errors.New("select one workspace, conversation or execution"))
	}
	report, err := s.Bench.Telemetry.Report(ctx, telemetry.Query{Workspace: r.Workspace, Execution: r.Execution, Conversation: r.Conversation, Series: r.Series, Prices: s.Bench.Preferences.Rates()})
	if err != nil {
		return nil, rpcError(err)
	}
	data, err := json.Marshal(report)
	if err != nil {
		return nil, rpcError(err)
	}
	out := &pb.TelemetryReport{}
	if err = protojson.Unmarshal(data, out); err != nil {
		return nil, rpcError(err)
	}
	return out, nil
}
func (s *Service) RenameCollection(ctx context.Context, r *pb.RenameCollectionRequest) (*pb.CollectionResponse, error) {
	m, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	c, err := m.Rename(r.CollectionId, r.Name)
	if err != nil {
		return nil, rpcError(err)
	}
	if callerSession(ctx) != "" {
		return &pb.CollectionResponse{Collection: &pb.Collection{Id: c.ID, Name: c.Name}}, nil
	}
	return &pb.CollectionResponse{Collection: wireCollection(c)}, nil
}
