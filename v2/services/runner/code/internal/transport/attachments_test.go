package transport

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/attachments"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
)

func TestGenericAttachmentDeliveryBothProviderShapesAndImageVision(t *testing.T) {
	for _, provider := range []string{"claude", "codex"} {
		t.Run(provider, func(t *testing.T) {
			bench, err := runner.New(runner.Config{State: t.TempDir()})
			if err != nil {
				t.Fatal(err)
			}
			t.Cleanup(func() { _ = bench.Close() })
			adapter := &agenttest.Adapter{}
			bench.Adapters[provider] = adapter
			execution, err := bench.Open(provider+"-default", t.TempDir(), "", accounts.Options{}, "direct", "", "")
			if err != nil {
				t.Fatal(err)
			}
			service := &Service{Bench: bench}
			info, err := service.Info(context.Background(), &pb.InfoRequest{})
			if err != nil || info.AttachmentLimits.MaxFiles != attachments.MaxFiles {
				t.Fatal("attachment limits not advertised")
			}
			if !wireSession(execution).Capabilities.Files {
				t.Fatal("file capability not advertised")
			}
			image, err := base64.StdEncoding.DecodeString("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jWZkAAAAASUVORK5CYII=")
			if err != nil {
				t.Fatal(err)
			}
			inputs := []*pb.FileAttachment{{Name: "source.go", MediaType: "text/plain", Data: []byte("package uploaded")}, {Name: "report.pdf", MediaType: "application/pdf", Data: []byte("%PDF-1.7 fixture")}, {Name: "binary.dat", Data: []byte{0, 255, 1}}, {Name: "pixel.png", MediaType: "image/png", Data: image}}
			request := &pb.ApplyRequest{SessionId: execution.ID, OperationId: "files-only", Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Attachments: inputs}}}
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			defer cancel()
			if _, err := service.Apply(ctx, request); err != nil {
				t.Fatal(err)
			}
			var operation agent.Operation
			select {
			case operation = <-adapter.Session.Applied:
			case <-ctx.Done():
				t.Fatal("file-only message was not delivered")
			}
			if len(operation.Files) != 4 || len(operation.Images) != 1 || operation.Images[0].Data != base64.StdEncoding.EncodeToString(image) {
				t.Fatal("generic files or native image input lost")
			}
			for i, file := range operation.Files {
				actual, err := os.ReadFile(file.Path)
				if err != nil || !bytes.Equal(actual, inputs[i].Data) {
					t.Fatal("client bytes not retained on Runner")
				}
			}
			if _, err := service.Apply(ctx, request); err != nil {
				t.Fatalf("retry: %v", err)
			}
			select {
			case <-adapter.Session.Applied:
				t.Fatal("retry duplicated the turn")
			default:
			}
			found := false
			for cursor := uint64(0); ; {
				record, err := execution.Engine.Events(ctx, cursor)
				if err != nil {
					t.Fatal(err)
				}
				cursor = record.Sequence
				var event agent.Event
				if err := json.Unmarshal(record.Value, &event); err != nil {
					t.Fatal(err)
				}
				if event.Type == "operation_accepted" {
					if !strings.Contains(string(event.Data), "source.go") || bytes.Contains(record.Value, inputs[0].Data) {
						t.Fatal("journal must retain metadata, never raw file bytes")
					}
					found = true
					break
				}
			}
			if !found {
				t.Fatal("accepted attachment metadata missing")
			}
			other, err := bench.Open(provider+"-default", t.TempDir(), "", accounts.Options{}, "direct", "", "")
			if err != nil {
				t.Fatal(err)
			}
			request.SessionId = other.ID
			request.OperationId = "traversal"
			request.Operation = &pb.ApplyRequest_Message{Message: &pb.Message{Attachments: []*pb.FileAttachment{{Name: "../escape", Data: []byte("blocked")}}}}
			if _, err := service.Apply(ctx, request); err == nil {
				t.Fatal("client-controlled path accepted")
			}
		})
	}
}
