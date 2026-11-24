package power

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"html/template"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/atomicfile"
)

type diskCandidate struct {
	Label, Path, Frees string
	Bytes              *int64
}
type diskScan struct {
	Pressure string `json:"disk_pressure"`
	DryRun   bool   `json:"dry_run"`
	Disk     struct {
		Free  int64 `json:"free_bytes"`
		Total int64 `json:"total_bytes"`
	}
	Targets []diskCandidate
}

var diskPage = template.Must(template.New("disk").Funcs(template.FuncMap{"size": func(n int64) string { return fmt.Sprintf("%.2f GiB", float64(n)/(1<<30)) }}).Parse(`<!doctype html><html lang="en"><meta charset="utf-8"><title>Disk Cleanup</title><link rel="stylesheet" href="lazybox.css"><h1>Disk Cleanup</h1><p>Scanned {{.At}} · {{size .Scan.Disk.Free}} free of {{size .Scan.Disk.Total}}</p><ul>{{range .Scan.Targets}}<li>{{.Label}} {{.Path}} · {{if .Bytes}}{{size .Bytes}}{{else}}size unknown{{end}}</li>{{else}}<li>No disk cleanup candidates.</li>{{end}}</ul><a href="disk-report.html">Reload saved report</a></html>`))

type diskResult struct {
	At    time.Time
	Scan  diskScan
	Error string
}

func runDiskScan(ctx context.Context) diskResult {
	home, err := os.UserHomeDir()
	if err != nil {
		return diskResult{Error: "Could not resolve the Runner home directory."}
	}
	cmd := exec.CommandContext(ctx, "dev-cleaner", "--dry-run", "--sizes", "--json")
	cmd.Dir = home
	cmd.WaitDelay = 5 * time.Second
	data, err := cmd.Output()
	if err != nil {
		return diskResult{Error: "Scan failed. Check that dev-cleaner is installed and runs on the Runner PATH."}
	}
	var s diskScan
	if json.Unmarshal(data, &s) != nil || !s.DryRun || s.Disk.Total <= 0 {
		return diskResult{Error: "The cleaner returned an invalid disk report."}
	}
	rows := s.Targets[:0]
	for _, r := range s.Targets {
		if r.Frees == "disk" {
			rows = append(rows, r)
		}
	}
	s.Targets = rows
	sort.SliceStable(rows, func(i, j int) bool {
		return rows[i].Bytes != nil && (rows[j].Bytes == nil || *rows[i].Bytes > *rows[j].Bytes)
	})
	return diskResult{Scan: s, At: time.Now().UTC()}
}

func (r diskResult) page() string {
	if r.Error != "" {
		return template.HTMLEscapeString(r.Error)
	}
	if r.At.IsZero() {
		return "Waiting for the first scan."
	}
	var body bytes.Buffer
	if err := diskPage.Execute(&body, struct {
		At   string
		Scan diskScan
	}{r.At.Format(time.RFC3339), r.Scan}); err != nil {
		return "Could not display the disk report."
	}
	return body.String()
}

// WatchDiskScans runs one read-only scan per day while a disk scan grant is active.
func (e *Extensions) WatchDiskScans(ctx context.Context) {
	e.watchDiskScans(ctx, time.Second, 24*time.Hour, 5*time.Minute, runDiskScan)
}

func (e *Extensions) watchDiskScans(ctx context.Context, poll, interval, startupDelay time.Duration, scan func(context.Context) diskResult) {
	ticker := time.NewTicker(poll)
	defer ticker.Stop()
	path := filepath.Join(filepath.Dir(e.approvalsPath), "disk-scan.json")
	var state struct {
		Started time.Time
		Report  diskResult
	}
	if data, err := os.ReadFile(path); err == nil {
		if err = json.Unmarshal(data, &state); err != nil {
			log.Printf("read disk scan state: %v", err)
		}
	}
	save := func() {
		data, err := json.Marshal(state)
		if err == nil {
			err = atomicfile.WriteFile(path, data)
		}
		if err != nil {
			log.Printf("save disk scan state: %v", err)
		}
	}
	e.mu.Lock()
	e.diskReport = state.Report
	e.mu.Unlock()
	ready := time.Now().Round(0).Add(startupDelay)
	var cancel context.CancelFunc
	var done chan diskResult
	defer func() {
		if cancel != nil {
			cancel()
		}
		if done != nil {
			<-done
		}
	}()
	for {
		select {
		case <-ctx.Done():
			return
		case page := <-done:
			done = nil
			if cancel != nil {
				cancel()
				cancel = nil
				e.mu.Lock()
				if page.Error != "" {
					page.Scan.Pressure = state.Report.Scan.Pressure
				}
				state.Report = page
				e.diskReport = page
				e.mu.Unlock()
				save()
			}
		case now := <-ticker.C:
			if !e.Snapshot().diskScanEnabled() {
				if cancel != nil {
					cancel()
					cancel = nil
				}
				if !state.Report.At.IsZero() || state.Report.Error != "" {
					state.Report = diskResult{}
					e.mu.Lock()
					e.diskReport = state.Report
					e.mu.Unlock()
					save()
				}
				continue
			}
			now = now.Round(0)
			if done != nil || now.Before(ready) || now.Before(state.Started.Add(interval)) {
				continue
			}
			run := e.startDiskScan(ctx, scan)
			if run == nil {
				continue
			}
			state.Started = now
			save()
			cancel, done = run.cancel, run.result
		}
	}
}

// diskScanRun is one scan under way.
type diskScanRun struct {
	cancel context.CancelFunc
	result chan diskResult
}

// startDiskScan starts a scan if the snapshot still allows one, deciding and
// registering it under e.mu: a SetEnabled, Approve or Remove that takes the
// grant away (reloadLocked) cancels it before returning, and none starts after.
func (e *Extensions) startDiskScan(ctx context.Context, scan func(context.Context) diskResult) *diskScanRun {
	e.mu.Lock()
	defer e.mu.Unlock()
	if !e.snap.diskScanEnabled() {
		return nil
	}
	scanCtx, cancel := context.WithTimeout(ctx, 30*time.Minute)
	run := &diskScanRun{cancel: cancel, result: make(chan diskResult, 1)}
	e.diskScan = run
	go func() {
		defer cancel()
		page := scan(scanCtx)
		e.mu.Lock()
		if e.diskScan == run {
			e.diskScan = nil
		}
		e.mu.Unlock()
		run.result <- page
	}()
	return run
}

// DiskPressureNotice returns the workspace notice ID for the last measured warning while a scan grant is active.
func (e *Extensions) DiskPressureNotice() string {
	e.mu.Lock()
	defer e.mu.Unlock()
	if e.snap.diskScanEnabled() {
		switch e.diskReport.Scan.Pressure {
		case "tight":
			return "disk-pressure-tight"
		case "critical":
			return "disk-pressure-critical"
		}
	}
	return ""
}

func (s ExtensionsSnapshot) diskScanEnabled() bool {
	for _, x := range s.Extensions {
		if x.Runs() && HasGrant(x.Manifest.Grants, GrantDiskScan) && HasGrant(x.ApprovedGrants, GrantDiskScan) {
			return true
		}
	}
	return false
}
