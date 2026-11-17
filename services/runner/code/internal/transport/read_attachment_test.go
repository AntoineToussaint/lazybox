package transport

import (
	"bytes"
	"encoding/base64"
	"encoding/json"
	"path/filepath"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// The conversation shows a sent image from the path its operation_accepted
// record lists; the owner may read it (and a paired phone, device_auth_test.go),
// never an agent, and only an image.
func TestReadAttachmentServesSentImagesAndTextNeverToAnAgent(t *testing.T) {
	f := newAuthorityFixture(t)
	ctx := timeout(t)
	execution, fake, agentCredential := f.open(t)
	pixel, err := base64.StdEncoding.DecodeString("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jWZkAAAAASUVORK5CYII=")
	if err != nil {
		t.Fatal(err)
	}
	owner := bearer(ctx, ownerToken)
	inputs := []*pb.FileAttachment{{Name: "screen shot.png", MediaType: "image/png", Data: pixel}, {Name: "notes.txt", MediaType: "text/plain", Data: []byte("plain text")}, {Name: "archive.bin", Data: []byte{0x1f, 0x8b, 0, 0xff}}}
	if _, err := f.client.Apply(owner, &pb.ApplyRequest{SessionId: execution.ID, OperationId: "with-image", Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: "look", Attachments: inputs}}}); err != nil {
		t.Fatal(err)
	}
	<-fake.Session.Applied
	// What a client knows: the accepted record's file list.
	var files []agent.File
	for cursor := uint64(0); files == nil; {
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
			var data struct{ Files []agent.File }
			if err := json.Unmarshal(event.Data, &data); err != nil || len(data.Files) != 3 {
				t.Fatalf("accepted record lists no files: %s %v", event.Data, err)
			}
			files = data.Files
		}
	}
	got, err := f.client.ReadAttachment(owner, &pb.ReadAttachmentRequest{Path: files[0].Path})
	if err != nil || !bytes.Equal(got.Data, pixel) || got.MediaType != "image/png" || got.Name != "screen shot.png" {
		t.Fatalf("owner cannot read the sent image: %v", err)
	}
	// How each reached the agent is on the record, for every client to show.
	if files[0].Delivery != agent.DeliverImage || files[1].Delivery != agent.DeliverText || files[2].Delivery != agent.DeliverFile {
		t.Fatalf("deliveries not recorded: %+v", files)
	}
	// Text is served for its preview, as plain text; a binary file is not.
	if got, err := f.client.ReadAttachment(owner, &pb.ReadAttachmentRequest{Path: files[1].Path}); err != nil || string(got.Data) != "plain text" || got.MediaType != "text/plain; charset=utf-8" {
		t.Fatalf("a text attachment was not served as text: %v", err)
	}
	if _, err := f.client.ReadAttachment(owner, &pb.ReadAttachmentRequest{Path: files[2].Path}); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("a binary attachment was served for preview: %v", err)
	}
	for _, path := range []string{"", "relative.png", filepath.Join(filepath.Dir(files[0].Path), "..", "..", "..", "..", "state.json"), filepath.Dir(files[0].Path)} {
		if _, err := f.client.ReadAttachment(owner, &pb.ReadAttachmentRequest{Path: path}); status.Code(err) != codes.InvalidArgument {
			t.Errorf("%q: got %v, want InvalidArgument", path, err)
		}
	}
	missing := filepath.Join(filepath.Dir(filepath.Dir(files[0].Path)), "0000000000000000000000000000000000000000000000000000000000000000", "gone.png")
	if _, err := f.client.ReadAttachment(owner, &pb.ReadAttachmentRequest{Path: missing}); status.Code(err) != codes.NotFound {
		t.Fatalf("missing attachment: got %v, want NotFound", err)
	}
	// The agent that received the image cannot read it back through the Runner,
	// and a caller without a bearer is not let in.
	if _, err := f.client.ReadAttachment(bearer(ctx, agentCredential), &pb.ReadAttachmentRequest{Path: files[0].Path}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("agent credential: got %v, want PermissionDenied", err)
	}
	if _, err := f.client.ReadAttachment(ctx, &pb.ReadAttachmentRequest{Path: files[0].Path}); status.Code(err) != codes.Unauthenticated {
		t.Fatalf("no credential: got %v, want Unauthenticated", err)
	}
}
