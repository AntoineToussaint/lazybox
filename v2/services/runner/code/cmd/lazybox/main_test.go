package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"connectrpc.com/connect"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
	"github.com/AntoineToussaint/lazybox/v2/pkg/gen/genconnect"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/proto"
)

type pipeClient struct {
	genconnect.RunnerServiceClient
	registerError, publishError error
	finished                    *pb.FinishPipeRequest
	offset                      uint64
}

func (c *pipeClient) RegisterPipe(context.Context, *connect.Request[pb.RegisterPipeRequest]) (*connect.Response[pb.CommandStatus], error) {
	return connect.NewResponse(&pb.CommandStatus{Id: "command"}), c.registerError
}
func (c *pipeClient) Publish(_ context.Context, r *connect.Request[pb.PublishRequest]) (*connect.Response[pb.CommandStatus], error) {
	c.offset += uint64(len(r.Msg.Data))
	return connect.NewResponse(&pb.CommandStatus{}), c.publishError
}
func (c *pipeClient) FinishPipe(_ context.Context, r *connect.Request[pb.FinishPipeRequest]) (*connect.Response[pb.CommandStatus], error) {
	c.finished = r.Msg
	return connect.NewResponse(&pb.CommandStatus{}), nil
}
func TestPublicationFailureNeverSwallowsAgentOutput(t *testing.T) {
	text := strings.Repeat("retained for agent\n", 5000)
	for _, stage := range []string{"register", "publish", "success"} {
		t.Run(stage, func(t *testing.T) {
			c := &pipeClient{}
			if stage == "register" {
				c.registerError = errors.New("registration failed")
			}
			if stage == "publish" {
				c.publishError = errors.New("publication failed")
			}
			var output, stderr bytes.Buffer
			err := publish(context.Background(), c, "execution", "console", strings.NewReader(text), &output, &stderr)
			if output.String() != text {
				t.Fatal("agent output was swallowed")
			}
			if (err == nil) != (stage == "success") {
				t.Fatalf("wrong publication outcome: %v", err)
			}
			if stage != "register" && (c.finished == nil || (c.finished.CaptureError == "") != (stage == "success")) {
				t.Fatalf("capture completion not reported: %+v", c.finished)
			}
		})
	}
}

type shortWriter struct{}

func (shortWriter) Write(p []byte) (int, error) { return 0, nil }
func TestFailedAgentWriteStillClosesCapture(t *testing.T) {
	c := &pipeClient{}
	err := publish(context.Background(), c, "execution", "console", strings.NewReader("output"), shortWriter{}, io.Discard)
	if !errors.Is(err, io.ErrShortWrite) || c.finished == nil || c.finished.CaptureError == "" {
		t.Fatalf("%v %+v", err, c.finished)
	}
}
func TestMissingEndpointStillPreservesPipe(t *testing.T) {
	var output bytes.Buffer
	cause := errors.New("missing endpoint")
	err := preserveOnFailure("log", cause, strings.NewReader("producer output"), &output)
	if !errors.Is(err, cause) || output.String() != "producer output" {
		t.Fatal("startup failure swallowed producer output")
	}
}

func TestCollectionCLIUsesServerIdentityAndStableRequestKey(t *testing.T) {
	t.Setenv("LAZYBOX_V2_COLLECTION_ID", "")
	type call struct {
		method string
		body   map[string]any
	}
	calls := make(chan call, 1)
	host := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body map[string]any
		method := filepath.Base(r.URL.Path)
		requests := map[string]proto.Message{"ListRepositories": &pb.ListRepositoriesRequest{}, "ListCollections": &pb.ListCollectionsRequest{}, "GetCollection": &pb.CollectionRequest{}, "ArchiveCollection": &pb.CollectionRequest{}, "CreateCollection": &pb.CreateCollectionRequest{}, "OpenSession": &pb.OpenSessionRequest{}, "OpenAssistant": &pb.OpenAssistantRequest{}, "ReconcileCollection": &pb.ReconcileCollectionRequest{}}
		message := requests[method]
		if r.Header.Get("Authorization") != "Bearer cli-fixture" {
			t.Error("missing runner authority")
		}
		data, err := io.ReadAll(r.Body)
		if err != nil || message == nil {
			t.Error("invalid request", err)
			w.WriteHeader(http.StatusBadRequest)
			return
		}
		if err := proto.Unmarshal(data, message); err != nil {
			t.Error(err)
		}
		encoded, err := protojson.Marshal(message)
		if err != nil {
			t.Error(err)
		}
		if err := json.Unmarshal(encoded, &body); err != nil {
			t.Error(err)
		}
		calls <- call{method, body}
		w.Header().Set("Content-Type", "application/proto")
		w.WriteHeader(http.StatusOK)
	}))
	defer host.Close()
	path := filepath.Join(t.TempDir(), "endpoint.json")
	data, _ := json.Marshal(map[string]string{"url": host.URL, "token": "cli-fixture"})
	if err := os.WriteFile(path, data, 0600); err != nil {
		t.Fatal(err)
	}
	spec := filepath.Join(t.TempDir(), "request.json")
	if err := os.WriteFile(spec, []byte(`{"requestKey":"stable-key","spec":{"name":"Feature","members":[{"name":"api","repositoryId":"backend"}]}}`), 0600); err != nil {
		t.Fatal(err)
	}
	originalArgs := os.Args
	defer func() { os.Args = originalArgs }()
	for _, test := range []struct {
		args   []string
		method string
	}{
		{[]string{"repositories"}, "ListRepositories"},
		{[]string{"assistant", "--profile", "claude-default", "--purpose", "planning", "--rotate-profiles", "work,personal"}, "OpenAssistant"},
		{[]string{"create-collection", "--spec", spec}, "CreateCollection"},
		{[]string{"collections"}, "ListCollections"},
		{[]string{"collection", "--collection", "collection-id"}, "GetCollection"},
		{[]string{"open", "--collection", "collection-id", "--profile", "claude-default"}, "OpenSession"},
		{[]string{"archive-collection", "--collection", "collection-id"}, "ArchiveCollection"},
		{[]string{"reconcile-collection", "--collection", "collection-id", "--owner", "old-owner", "--reason", "fixture has no processes", "--processes-stopped"}, "ReconcileCollection"},
	} {
		os.Args = append([]string{"lazybox", "--endpoint", path}, test.args...)
		if err := run(); err != nil {
			t.Fatal(err)
		}
		called := <-calls
		method, body := called.method, called.body
		if method != test.method {
			t.Fatalf("called %s instead of %s", method, test.method)
		}
		if method == "CreateCollection" && body["requestKey"] != "stable-key" {
			t.Fatal("idempotency key replaced")
		}
		if method == "OpenAssistant" && (body["purpose"] != "planning" || body["cwd"] != nil || body["collectionId"] != nil) {
			t.Fatal("assistant scope lost", body)
		}
		if method == "OpenSession" && (body["cwd"] != nil || body["collectionId"] != "collection-id") {
			t.Fatal("client generated a local collection path", body)
		}
		if method == "ReconcileCollection" && (body["expectedOwnerExecutionId"] != "old-owner" || body["processesStopped"] != true) {
			t.Fatal("operator attestation lost", body)
		}
	}
}
