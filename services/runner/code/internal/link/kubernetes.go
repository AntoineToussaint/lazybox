package link

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"time"
)

// OwnerRoute can run the existing owner administration commands.
type OwnerRoute interface {
	Route
	Run(context.Context, io.Reader, ...string) ([]byte, error)
	ownerCommand() []string
}

func (r SSHRoute) ownerCommand() []string {
	words := []string{r.Remote}
	if r.Runner != "" {
		words = append(words, "--runner", r.Runner)
	}
	return words
}

// KubernetesTarget names a workload whose pod can change across reconnects.
type KubernetesTarget struct {
	Context   string `json:"context"`
	Namespace string `json:"namespace"`
	Workload  string `json:"workload"`
	Container string `json:"container"`
	Kubectl   string `json:"kubectl,omitempty"`
}

var kubeName = regexp.MustCompile(`^[a-z0-9]([a-z0-9.-]*[a-z0-9])?$`)

func (t KubernetesTarget) Validate() error {
	kind, name, _ := strings.Cut(t.Workload, "/")
	if strings.TrimSpace(t.Context) == "" || !kubeName.MatchString(t.Namespace) ||
		(kind != "statefulset" && kind != "deployment") || !kubeName.MatchString(name) ||
		!kubeName.MatchString(t.Container) {
		return fmt.Errorf("supply a Kubernetes context, namespace, deployment/name or statefulset/name, and container")
	}
	return nil
}

type KubernetesRoute struct {
	Target KubernetesTarget
}

func (r KubernetesRoute) Kind() string { return "kubernetes" }
func (r KubernetesRoute) Describe() RouteInfo {
	return RouteInfo{Kind: r.Kind(), Label: "Kubernetes", Host: r.Target.Context + "/" + r.Target.Namespace + "/" + r.Target.Workload}
}

func (r KubernetesRoute) ownerCommand() []string { return []string{"lazybox"} }

func (r KubernetesRoute) command(ctx context.Context, words ...string) (*exec.Cmd, error) {
	t := r.Target
	if err := t.Validate(); err != nil {
		return nil, err
	}
	args := []string{"--context=" + t.Context, "--namespace=" + t.Namespace, "exec", "-i", "--container=" + t.Container, t.Workload, "--"}
	binary := t.Kubectl
	if binary == "" {
		binary = "kubectl"
	} else if !filepath.IsAbs(binary) {
		return nil, fmt.Errorf("the saved kubectl executable must have an absolute path")
	}
	cmd := exec.CommandContext(ctx, binary, append(args, words...)...)
	cmd.WaitDelay = 2 * time.Second
	return cmd, nil
}

func (r KubernetesRoute) Run(ctx context.Context, stdin io.Reader, words ...string) ([]byte, error) {
	cmd, err := r.command(ctx, words...)
	if err != nil {
		return nil, err
	}
	var stderr bytes.Buffer
	cmd.Stdin, cmd.Stderr = stdin, &stderr
	out, err := cmd.Output()
	if err != nil {
		return nil, fmt.Errorf("kubectl exec %s: %w: %s", r.Describe().Host, err, strings.TrimSpace(stderr.String()))
	}
	return out, nil
}

func (r KubernetesRoute) Dial(ctx context.Context) (io.ReadWriteCloser, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	cmd, err := r.command(context.Background(), "lazybox", "link", "serve", "--stdio", "--route", r.Kind())
	if err != nil {
		return nil, err
	}
	return startStream(cmd, r.Describe().Host)
}
