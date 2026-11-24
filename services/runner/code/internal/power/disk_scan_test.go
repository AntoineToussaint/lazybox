package power

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestDiskScanOnlyPreviewsAndEscapesCandidates(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("PATH", dir)
	script := `#!/bin/sh
[ "$#" = 3 ] && [ "$1" = --dry-run ] && [ "$2" = --sizes ] && [ "$3" = --json ] || exit 1
echo '{"dry_run":true,"disk":{"free_bytes":1,"total_bytes":2},"targets":[{"label":"memory item","frees":"memory"},{"label":"small","frees":"disk"},{"label":"<big>","bytes":1234,"frees":"disk"}]}'
`
	if err := os.WriteFile(filepath.Join(dir, "dev-cleaner"), []byte(script), 0700); err != nil {
		t.Fatal(err)
	}
	page := runDiskScan(context.Background()).page()
	if !strings.Contains(page, "&lt;big&gt;") || strings.Index(page, "small") < strings.Index(page, "&lt;big&gt;") || strings.Contains(page, "memory item") {
		t.Fatal(page)
	}
}

// Turning it off is covered without the scheduler's timing by
// TestTurningTheScanOffCancelsItAtOnceAndStartsNone and
// TestAScanCancelledByTurningItOffIsNoReportAndRunsAgain.
func TestDiskScanScheduleRequiresApproval(t *testing.T) {
	dir := t.TempDir()
	e := NewExtensions(dir, filepath.Join(t.TempDir(), "approvals.json"))
	if _, err := e.InstallExample("disk-cleanup"); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	finished := make(chan struct{})
	runs := make(chan struct{}, 2)
	go func() {
		defer close(finished)
		e.watchDiskScans(ctx, time.Millisecond, 20*time.Millisecond, 0, func(ctx context.Context) diskResult {
			select {
			case runs <- struct{}{}:
			default:
			}
			return diskResult{At: time.Now()}
		})
	}()
	defer func() { cancel(); <-finished }()
	select {
	case <-runs:
		t.Fatal("ran before approval")
	case <-time.After(30 * time.Millisecond):
	}
	if _, err := e.Approve("disk-cleanup", []string{GrantDiskScan}, nil); err != nil {
		t.Fatal(err)
	}
	for range 2 {
		select {
		case <-runs:
		case <-time.After(time.Second):
			t.Fatal("scheduled scan missing")
		}
	}
}

func TestAScanCancelledByTurningItOffIsNoReportAndRunsAgain(t *testing.T) {
	e := NewExtensions(t.TempDir(), filepath.Join(t.TempDir(), "approvals.json"))
	if _, err := e.InstallExample("disk-cleanup"); err != nil {
		t.Fatal(err)
	}
	if _, err := e.Approve("disk-cleanup", []string{GrantDiskScan}, nil); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	finished := make(chan struct{})
	started := make(chan struct{}, 4)
	go func() {
		defer close(finished)
		// An hour between scans: only a scan whose run was revoked may start again soon.
		e.watchDiskScans(ctx, 5*time.Millisecond, time.Hour, 0, func(ctx context.Context) diskResult {
			started <- struct{}{}
			<-ctx.Done()
			return diskResult{At: time.Now(), Error: "cancelled"}
		})
	}()
	defer func() { cancel(); <-finished }()
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("first scan missing")
	}
	if _, err := e.SetEnabled("disk-cleanup", false); err != nil {
		t.Fatal(err)
	}
	if _, err := e.SetEnabled("disk-cleanup", true); err != nil {
		t.Fatal(err)
	}
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("the scan cancelled by turning it off did not run again once allowed")
	}
	e.mu.Lock()
	report := e.diskReport
	e.mu.Unlock()
	if report.Error == "cancelled" {
		t.Fatal("the cancelled scan's result became the report")
	}
}

func TestTurningTheScanOffCancelsItAtOnceAndStartsNone(t *testing.T) {
	e := NewExtensions(t.TempDir(), filepath.Join(t.TempDir(), "approvals.json"))
	if _, err := e.InstallExample("disk-cleanup"); err != nil {
		t.Fatal(err)
	}
	if _, err := e.Approve("disk-cleanup", []string{GrantDiskScan}, nil); err != nil {
		t.Fatal(err)
	}
	started := make(chan context.Context, 1)
	run := e.startDiskScan(context.Background(), func(ctx context.Context) diskResult {
		started <- ctx
		<-ctx.Done()
		return diskResult{}
	})
	if run == nil {
		t.Fatal("an approved, enabled scan did not start")
	}
	scanCtx := <-started
	if _, err := e.SetEnabled("disk-cleanup", false); err != nil {
		t.Fatal(err)
	}
	if scanCtx.Err() == nil {
		t.Fatal("the scan under way kept running once its extension was turned off")
	}
	<-run.result
	if e.startDiskScan(context.Background(), func(context.Context) diskResult {
		t.Error("a scan started after its extension was turned off")
		return diskResult{}
	}) != nil {
		t.Fatal("startDiskScan registered a scan the snapshot no longer allows")
	}
}

func TestDiskScanRestoresReportAndDeadline(t *testing.T) {
	dir := t.TempDir()
	approvals := filepath.Join(t.TempDir(), "extensions.json")
	e := NewExtensions(dir, approvals)
	if _, err := e.InstallExample("disk-cleanup"); err != nil {
		t.Fatal(err)
	}
	if _, err := e.Approve("disk-cleanup", []string{GrantDiskScan}, nil); err != nil {
		t.Fatal(err)
	}
	runs := make(chan struct{}, 2)
	start := func(e *Extensions, delay time.Duration) func() {
		ctx, cancel := context.WithCancel(context.Background())
		finished := make(chan struct{})
		go func() {
			defer close(finished)
			e.watchDiskScans(ctx, time.Millisecond, 24*time.Hour, delay, func(context.Context) diskResult {
				runs <- struct{}{}
				return diskResult{At: time.Now(), Scan: diskScan{Pressure: "critical"}}
			})
		}()
		return func() { cancel(); <-finished }
	}
	stop := start(e, 60*time.Millisecond)
	select {
	case <-runs:
		stop()
		t.Fatal("scanned during startup")
	case <-time.After(10 * time.Millisecond):
	}
	select {
	case <-runs:
	case <-time.After(time.Second):
		stop()
		t.Fatal("no scan")
	}
	for deadline := time.Now().Add(time.Second); e.DiskPressureNotice() == "" && time.Now().Before(deadline); {
		time.Sleep(time.Millisecond)
	}
	stop()
	restored := NewExtensions(dir, approvals)
	finish := start(restored, 0)
	defer finish()
	select {
	case <-runs:
		t.Fatal("repeated scan after restart")
	case <-time.After(40 * time.Millisecond):
	}
	if got := restored.DiskPressureNotice(); got != "disk-pressure-critical" {
		t.Fatal("restored pressure", got)
	}
	if _, err := restored.SetEnabled("disk-cleanup", false); err != nil {
		t.Fatal(err)
	}
	if got := restored.DiskPressureNotice(); got != "" {
		t.Fatal("disabled pressure", got)
	}
}
