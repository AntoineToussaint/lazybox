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

func diskSize(bytes int64) string {
	if bytes < 1024 {
		return fmt.Sprintf("%d B", bytes)
	}
	value := float64(bytes)
	for _, unit := range []string{"KiB", "MiB", "GiB", "TiB", "PiB", "EiB"} {
		value /= 1024
		if value < 1024 {
			return fmt.Sprintf("%.1f %s", value, unit)
		}
	}
	return fmt.Sprintf("%d B", bytes)
}

var diskPage = template.Must(template.New("disk").Funcs(template.FuncMap{"size": diskSize}).Parse(`<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>Disk Cleanup</title>
<link rel="stylesheet" href="lazybox.css"><script src="lazybox.js" defer></script></head>
<body class="workflow"><header><h1>Disk Cleanup</h1><p>Runner storage</p><a href="disk-report.html">Reload saved report</a></header>
{{if .Message}}<p role="status">{{.Message}}</p>{{else}}
<section aria-label="Disk space"><h2>{{size .Scan.Disk.Free}} free of {{size .Scan.Disk.Total}}</h2>
<meter aria-label="Free disk space" value="{{.Scan.Disk.Free}}" min="0" max="{{.Scan.Disk.Total}}"></meter>
<p class="muted">Scanned <time datetime="{{.At}}">{{.Scanned}}</time></p></section>
<h2>Cleanup candidates{{if .Scan.Targets}} · {{len .Scan.Targets}}{{end}}</h2>{{if .Scan.Targets}}<ul>{{range .Scan.Targets}}<li><strong>{{.Label}}</strong><small><code>{{.Path}}</code></small><output>{{if .Bytes}}{{size .Bytes}}{{else}}Size unknown{{end}}</output></li>{{end}}</ul>{{else}}<p>No disk cleanup candidates.</p>{{end}}{{end}}
</body></html>`))

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
	message := r.Error
	if message == "" && r.At.IsZero() {
		message = "Waiting for the first scan."
	}
	var body bytes.Buffer
	if err := diskPage.Execute(&body, struct {
		At      string
		Scanned string
		Message string
		Scan    diskScan
	}{r.At.Format(time.RFC3339), r.At.UTC().Format("Jan 2, 2006 at 15:04 UTC"), message, r.Scan}); err != nil {
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
	var done chan diskOutcome
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
		case outcome := <-done:
			done = nil
			if outcome.revoked {
				// Turned off (or its grant taken away) while it ran: not a
				// result. It runs again as soon as it is allowed again.
				cancel = nil
				state.Started = time.Time{}
				save()
				continue
			}
			if cancel != nil {
				cancel()
				cancel = nil
				page := outcome.page
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
	result chan diskOutcome
}

// diskOutcome is how a scan ended: its page, or revoked when reloadLocked
// cancelled it because its extension no longer allows scans.
type diskOutcome struct {
	page    diskResult
	revoked bool
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
	run := &diskScanRun{cancel: cancel, result: make(chan diskOutcome, 1)}
	e.diskScan = run
	go func() {
		defer cancel()
		page := scan(scanCtx)
		e.mu.Lock()
		// reloadLocked clears e.diskScan when it cancels the run.
		revoked := e.diskScan != run
		if !revoked {
			e.diskScan = nil
		}
		e.mu.Unlock()
		run.result <- diskOutcome{page: page, revoked: revoked}
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
