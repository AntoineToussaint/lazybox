package link

import (
	"context"
	"io"
	"os"
	"path/filepath"
	"slices"
	"testing"
)

func TestKubernetesRouteUsesPinnedTargetWithoutTTY(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("PATH", dir+":"+os.Getenv("PATH"))
	fake := "#!/bin/sh\nexec cat\n"
	if err := os.WriteFile(filepath.Join(dir, "kubectl"), []byte(fake), 0o700); err != nil {
		t.Fatal(err)
	}
	target := KubernetesTarget{Context: "pilot", Namespace: "fixture", Workload: "statefulset/runner", Container: "runner"}
	remotes := Remotes{Config: dir}
	if err := remotes.Save(Remote{Name: "pilot", Route: "kubernetes", Kubernetes: &target}); err != nil {
		t.Fatal(err)
	}
	saved, err := remotes.Load("pilot")
	if err != nil {
		t.Fatal(err)
	}
	route, err := saved.OwnerRoute("")
	if err != nil {
		t.Fatal(err)
	}
	want := []string{"--context=pilot", "--namespace=fixture", "exec", "-i", "--container=runner", "statefulset/runner", "--", "lazybox", "link", "serve", "--stdio", "--route", "kubernetes"}
	cmd, err := route.(KubernetesRoute).command(context.Background(), want[7:]...)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(cmd.Args[1:], want) {
		t.Fatalf("kubectl arguments = %q, want %q", cmd.Args[1:], want)
	}
	stream, err := route.Dial(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	defer stream.Close()
	if _, err := io.WriteString(stream, "frame"); err != nil {
		t.Fatal(err)
	}
	reply := make([]byte, len("frame"))
	if _, err := io.ReadFull(stream, reply); err != nil || string(reply) != "frame" {
		t.Fatalf("stream returned %q: %v", reply, err)
	}
	target.Workload = "statefulset/--other"
	if target.Validate() == nil {
		t.Fatal("accepted an invalid workload")
	}
}
