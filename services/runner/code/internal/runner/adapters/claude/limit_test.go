package claude

import (
	"encoding/base64"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/attachments"
)

// The largest message the Runner accepts reaches the worker in one line: its
// images travel as base64, and the worker's line limit counts that growth. The
// worker used to stop at 16 MiB while the Runner accepted 20 MiB of files.
func TestTheLargestAcceptedMessageFitsTheWorkersLine(t *testing.T) {
	// Four images, the most sent as images, filling the payload limit as
	// base64, with the rest of it as the person's text.
	image := base64.StdEncoding.EncodeToString(make([]byte, 6<<20))
	op := agent.Operation{ID: "big", Kind: agent.Message, Context: strings.Repeat("c", 128<<10)}
	payload := 0
	for i := 0; i < 4; i++ {
		op.Images = append(op.Images, agent.Image{MediaType: "image/png", Data: image})
		op.Files = append(op.Files, agent.File{Name: "shot.png", MediaType: "image/png", Delivery: agent.DeliverImage})
		payload += len(image)
	}
	op.Text = strings.Repeat("x", attachments.MaxPayloadBytes-payload)
	for _, parts := range []bool{true, false} {
		if err := fitsWorker(message(op, parts)); err != nil {
			t.Fatalf("parts=%v: %v", parts, err)
		}
	}
}

// Text that JSON escaping grows past the worker's line is refused with the
// reason, before anything is written, rather than ending the worker.
func TestAMessageTooLargeForTheWorkerIsRefusedBeforeItIsWritten(t *testing.T) {
	op := agent.Operation{ID: "escaped", Kind: agent.Message, Text: strings.Repeat("\x01", 12<<20)}
	err := fitsWorker(message(op, true))
	if err == nil || !strings.Contains(err.Error(), "too large for Claude") {
		t.Fatalf("got %v", err)
	}
}
