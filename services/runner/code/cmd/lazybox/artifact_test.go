package main

import (
	"bytes"
	"context"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"connectrpc.com/connect"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// fakeArtifacts keeps artifacts like the Runner does, for one workspace.
type fakeArtifacts struct {
	workspace string
	items     map[string]*pb.Artifact
	contents  map[string][]string
	puts      []*pb.PutArtifactRequest
	retires   []*pb.RetireArtifactRequest
}

func (f *fakeArtifacts) check(ws string) error {
	if ws != f.workspace {
		return connect.NewError(connect.CodePermissionDenied, nil)
	}
	return nil
}

func (f *fakeArtifacts) ListArtifacts(_ context.Context, r *connect.Request[pb.ListArtifactsRequest]) (*connect.Response[pb.ListArtifactsResponse], error) {
	if err := f.check(r.Msg.CollectionId); err != nil {
		return nil, err
	}
	out := &pb.ListArtifactsResponse{}
	for _, a := range f.items {
		if a.Retired == nil || r.Msg.IncludeRetired {
			out.Artifacts = append(out.Artifacts, a)
		}
	}
	slices.SortFunc(out.Artifacts, func(a, b *pb.Artifact) int { return strings.Compare(a.Id, b.Id) })
	return connect.NewResponse(out), nil
}

func (f *fakeArtifacts) GetArtifact(_ context.Context, r *connect.Request[pb.GetArtifactRequest]) (*connect.Response[pb.Artifact], error) {
	if err := f.check(r.Msg.CollectionId); err != nil {
		return nil, err
	}
	a := f.items[r.Msg.ArtifactId]
	if a == nil {
		return nil, connect.NewError(connect.CodeNotFound, nil)
	}
	n := r.Msg.Version
	if n == 0 {
		n = uint32(len(f.contents[a.Id]))
	}
	return connect.NewResponse(&pb.Artifact{Id: a.Id, Title: a.Title, Kind: a.Kind, Versions: a.Versions, Content: f.contents[a.Id][n-1], ContentVersion: n, Retired: a.Retired}), nil
}

func (f *fakeArtifacts) RetireArtifact(_ context.Context, r *connect.Request[pb.RetireArtifactRequest]) (*connect.Response[pb.Artifact], error) {
	if err := f.check(r.Msg.CollectionId); err != nil {
		return nil, err
	}
	f.retires = append(f.retires, r.Msg)
	a := f.items[r.Msg.ArtifactId]
	if a == nil {
		return nil, connect.NewError(connect.CodeNotFound, nil)
	}
	a.Retired = &pb.ArtifactRetirement{SupersededBy: r.Msg.SupersededBy, Reason: r.Msg.Reason}
	return connect.NewResponse(a), nil
}

func (f *fakeArtifacts) PutArtifact(_ context.Context, r *connect.Request[pb.PutArtifactRequest]) (*connect.Response[pb.Artifact], error) {
	if err := f.check(r.Msg.CollectionId); err != nil {
		return nil, err
	}
	f.puts = append(f.puts, r.Msg)
	id := r.Msg.ArtifactId
	if id == "" {
		id = "art" + string(rune('a'+len(f.items)))
		kind := r.Msg.Kind
		if kind == "" {
			kind = "other"
		}
		f.items[id] = &pb.Artifact{Id: id, Title: r.Msg.Title, Kind: kind}
	}
	a := f.items[id]
	if versions := f.contents[id]; len(versions) == 0 || versions[len(versions)-1] != r.Msg.Content {
		f.contents[id] = append(f.contents[id], r.Msg.Content)
		a.Versions = append(a.Versions, &pb.ArtifactVersion{Number: uint32(len(f.contents[id]))})
	}
	for _, old := range r.Msg.Supersedes {
		f.items[old].Retired = &pb.ArtifactRetirement{SupersededBy: id}
	}
	n := uint32(len(f.contents[id]))
	return connect.NewResponse(&pb.Artifact{Id: id, Title: a.Title, Kind: a.Kind, Versions: a.Versions, Content: f.contents[id][n-1], ContentVersion: n}), nil
}

func TestArtifactVerbPublishesReadsAndLists(t *testing.T) {
	ctx := context.Background()
	f := &fakeArtifacts{workspace: "ws", items: map[string]*pb.Artifact{}, contents: map[string][]string{}}
	run := func(stdin string, args ...string) (string, error) {
		var out, errs bytes.Buffer
		err := artifactCommand(ctx, f, "ws", args, strings.NewReader(stdin), &out, &errs)
		return out.String(), err
	}
	plan := filepath.Join(t.TempDir(), "PLAN.md")
	if err := os.WriteFile(plan, []byte("# Plan\nv1\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	// Flags may follow the file.
	out, err := run("", "put", plan, "--title", "Rollout plan", "--kind", "plan")
	if err != nil || !strings.Contains(out, "Published “Rollout plan” as artifact arta, version 1 (plan)") || !strings.Contains(out, "--id arta") {
		t.Fatal(out, err)
	}
	if put := f.puts[0]; put.CollectionId != "ws" || put.Content != "# Plan\nv1\n" || put.ArtifactId != "" {
		t.Fatalf("%+v", put)
	}
	out, err = run("# Plan\nv2\n", "put", "--id", "arta", "-")
	if err != nil || !strings.Contains(out, "version 2") {
		t.Fatal(out, err)
	}
	out, err = run("# Plan\nv2\n", "put", "-", "--id", "arta")
	if err != nil || !strings.Contains(out, "unchanged at version 2") {
		t.Fatal(out, err)
	}
	out, err = run("", "get", "arta")
	if err != nil || out != "# Plan\nv2\n" {
		t.Fatalf("%q %v", out, err)
	}
	out, err = run("", "get", "arta", "--version", "1")
	if err != nil || out != "# Plan\nv1\n" {
		t.Fatalf("%q %v", out, err)
	}
	out, err = run("", "list")
	if err != nil || out != "arta\tplan\tv2\tRollout plan\n" {
		t.Fatalf("%q %v", out, err)
	}

	if _, err = run("x", "put", "-"); err == nil || !strings.Contains(err.Error(), "--title is required") {
		t.Fatal(err)
	}
	if _, err = run("", "put"); err == nil {
		t.Fatal("put without a file")
	}
	if _, err = run("", "frobnicate"); err == nil {
		t.Fatal("unknown command accepted")
	}
	var out2, errs bytes.Buffer
	if err = artifactCommand(ctx, f, "", []string{"list"}, strings.NewReader(""), &out2, &errs); err == nil || !strings.Contains(err.Error(), "artifacts belong to a workspace") {
		t.Fatal(err)
	}
	if err = artifactCommand(ctx, f, "", []string{"list", "--collection", "ws"}, strings.NewReader(""), &out2, &errs); err != nil {
		t.Fatal("--collection ignored", err)
	}
}

// An agent consolidates: `put --supersedes` publishes one artifact and
// retires the ones it replaces, `retire` archives one, and `list` leaves
// retired artifacts out unless asked with --all. The lines printed are the
// ones the conversation's card reads.
func TestArtifactVerbRetiresAndSupersedes(t *testing.T) {
	ctx := context.Background()
	f := &fakeArtifacts{workspace: "ws", items: map[string]*pb.Artifact{}, contents: map[string][]string{}}
	run := func(stdin string, args ...string) (string, error) {
		var out, errs bytes.Buffer
		err := artifactCommand(ctx, f, "ws", args, strings.NewReader(stdin), &out, &errs)
		return out.String(), err
	}
	for _, title := range []string{"Milestone plan", "What's left", "Limitations"} {
		if _, err := run("# "+title, "put", "-", "--title", title, "--kind", "plan"); err != nil {
			t.Fatal(err)
		}
	}
	out, err := run("# Status", "put", "-", "--title", "Status", "--kind", "plan", "--supersedes", "arta, artb,arta")
	if err != nil {
		t.Fatal(err)
	}
	want := "Published “Status” as artifact artd, version 1 (plan). The user sees it in Lazybox → Artifacts. Publish a new version with --id artd.\n" +
		"Retired “Milestone plan” (artifact arta), superseded by “Status” (artifact artd).\n" +
		"Retired “What's left” (artifact artb), superseded by “Status” (artifact artd).\n" +
		"Retired artifacts keep every version; the person can restore them in Lazybox → Artifacts.\n"
	if out != want {
		t.Fatalf("%q", out)
	}
	if put := f.puts[len(f.puts)-1]; !slices.Equal(put.Supersedes, []string{"arta", "artb"}) {
		t.Fatal(put.Supersedes)
	}
	if _, err = run("# Status", "put", "-", "--id", "artd", "--supersedes", "missing"); err == nil || !strings.Contains(err.Error(), "--supersedes missing") {
		t.Fatal("an unknown superseded artifact", err)
	}

	out, err = run("", "retire", "artc", "--superseded-by", "artd", "--reason", "folded in")
	if err != nil || out != "Retired “Limitations” (artifact artc), superseded by “Status” (artifact artd). Retired artifacts keep every version; the person can restore them in Lazybox → Artifacts.\n" {
		t.Fatalf("%q %v", out, err)
	}
	if r := f.retires[0]; r.ArtifactId != "artc" || r.SupersededBy != "artd" || r.Reason != "folded in" || r.Restore {
		t.Fatalf("%+v", r)
	}
	if _, err = run("", "retire"); err == nil {
		t.Fatal("retire without an ID")
	}

	out, err = run("", "list")
	if err != nil || out != "artd\tplan\tv1\tStatus\n" {
		t.Fatalf("%q %v", out, err)
	}
	out, err = run("", "list", "--all")
	if err != nil || out != "arta\tplan\tv1\tMilestone plan\tretired (superseded by artd)\nartb\tplan\tv1\tWhat's left\tretired (superseded by artd)\nartc\tplan\tv1\tLimitations\tretired (superseded by artd)\nartd\tplan\tv1\tStatus\n" {
		t.Fatalf("%q %v", out, err)
	}
}
