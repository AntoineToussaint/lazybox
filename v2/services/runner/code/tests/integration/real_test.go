package integration

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"image"
	"image/color"
	"image/png"
	"os"
	"os/exec"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/commands"
)

// TestReal uses the same engine/command owners for both installed providers.
// These are live, billable integration runs, enabled explicitly by make test-real.
// It never treats unavailable credentials or a protocol limitation as a pass.
func TestReal(t *testing.T) {
	if os.Getenv("LAZYBOX_V2_REAL") != "1" {
		t.Skip("real harness runs require LAZYBOX_V2_REAL=1; see runtime evidence")
	}
	for _, provider := range []string{"codex", "claude"} {
		t.Run(provider, func(t *testing.T) {
			if selected := os.Getenv("LAZYBOX_V2_REAL_PROVIDER"); selected != "" && selected != provider {
				t.Skip("different provider selected")
			}
			version, err := exec.Command(provider, "--version").Output()
			if err != nil {
				t.Fatal(err)
			}
			t.Logf("harness version: %s", strings.TrimSpace(string(version)))
			b, err := runner.New(runner.Config{State: t.TempDir(), Worker: os.Getenv("LAZYBOX_V2_WORKER")})
			if err != nil {
				t.Fatal(err)
			}
			t.Cleanup(func() { _ = b.Close() })
			e, err := b.Open(provider+"-default", t.TempDir(), "", accounts.Options{}, "direct", "", "")
			if err != nil {
				t.Fatal(err)
			}
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
			defer cancel()
			cursor := uint64(0)
			turn := func(text string, images []agent.Image) string {
				t.Helper()
				if _, err := e.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Message, Text: text, Images: images}); err != nil {
					t.Fatal(err)
				}
				var response strings.Builder
				for {
					r, err := e.Engine.Events(ctx, cursor)
					if err != nil {
						t.Fatal(err)
					}
					cursor = r.Sequence
					var ev agent.Event
					if err = json.Unmarshal(r.Value, &ev); err != nil {
						t.Fatal(err)
					}
					switch ev.Type {
					case "message_delta":
						response.WriteString(ev.Text)
					case "approval":
						t.Logf("real approval %s (%s)", ev.RequestID, ev.Text)
						if _, err = e.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Approve, RequestID: ev.RequestID, Decision: "accept"}); err != nil {
							t.Fatal(err)
						}
					case "question":
						t.Logf("real question %s", ev.RequestID)
						var data struct {
							Questions []struct {
								ID       string `json:"id"`
								Question string `json:"question"`
							} `json:"questions"`
							Input struct {
								Questions []struct {
									Question string `json:"question"`
								} `json:"questions"`
							} `json:"input"`
						}
						_ = json.Unmarshal(ev.Data, &data)
						answers := map[string][]string{}
						for _, q := range data.Questions {
							answers[q.ID] = []string{"Blue"}
						}
						for _, q := range data.Input.Questions {
							answers[q.Question] = []string{"Blue"}
						}
						if _, err = e.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Answer, RequestID: ev.RequestID, Answers: answers}); err != nil {
							t.Fatal(err)
						}
					case "failure":
						t.Fatalf("provider failure %s: %s %s", ev.Status, ev.Text, ev.Data)
					case "turn_ended":
						if ev.Status != "completed" {
							t.Fatalf("turn ended %s: %s", ev.Status, response.String())
						}
						return response.String()
					}
				}
			}
			if got := turn("Do not use tools or delegate. Remember the word ORCHID for this conversation. Reply exactly ORCHID.", nil); !strings.Contains(got, "ORCHID") {
				t.Fatalf("first response: %s", got)
			}
			if got := turn("What word did I ask you to remember? Reply with that word only, no tools.", nil); !strings.Contains(got, "ORCHID") {
				t.Fatalf("follow-up lost context: %s", got)
			}
			img := image.NewRGBA(image.Rect(0, 0, 32, 32))
			for y := 0; y < 32; y++ {
				for x := 0; x < 32; x++ {
					img.Set(x, y, color.RGBA{255, 0, 0, 255})
				}
			}
			var encoded bytes.Buffer
			if err = png.Encode(&encoded, img); err != nil {
				t.Fatal(err)
			}
			if got := turn("What solid color is this attached image? Reply with the color only; no tools.", []agent.Image{{MediaType: "image/png", Data: base64.StdEncoding.EncodeToString(encoded.Bytes())}}); !strings.Contains(strings.ToLower(got), "red") {
				t.Fatalf("image not established: %s", got)
			}
			turn("Use your lazybox command tool (lazybox_command or mcp__lazybox__command) to start exactly one command with action start and argv [\"/bin/sh\",\"-c\",\"printf 'SHARED-CONSOLE-EVIDENCE\\n'\"]. Then use action wait and action read with the returned command id. Report the actual exit code and output marker. Do not use your built-in shell tool, delegate, or modify files.", nil)
			outputs := b.Commands.List(e.ID)
			if len(outputs) != 1 || !outputs[0].Done {
				t.Fatalf("expected one completed shared command, got %+v", outputs)
			}
			command, err := b.Commands.Get(outputs[0].ID)
			if err != nil {
				t.Fatal(err)
			}
			for viewer := 0; viewer < 2; viewer++ {
				data, status, err := command.Read(ctx, 0, 65536)
				if err != nil || string(data) != "SHARED-CONSOLE-EVIDENCE\n" || status.ExitCode == nil || *status.ExitCode != 0 {
					t.Fatalf("viewer %d: %q %+v %v", viewer, data, status, err)
				}
			}
			reference := e.Engine.Snapshot().Reference
			if reference == "" {
				t.Fatal("native session reference absent")
			}
			_ = e.Engine.Close()
			e, err = b.Open(provider+"-default", e.Cwd, reference, accounts.Options{}, "direct", e.WorkID, e.ID)
			if err != nil {
				t.Fatal(err)
			}
			cursor = 0
			if got := turn("What word did I ask you to remember at the start of this conversation? Reply with the word only; no tools.", nil); !strings.Contains(got, "ORCHID") {
				t.Fatalf("resume lost context: %s", got)
			}
			t.Log("verified: multi-turn, image, real managed-tool approval, agent and two readers see one command, native conversation resume; pending-request recovery and profile switching remain separate gates")
		})
	}
}
