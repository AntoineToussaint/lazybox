// Package listenports finds the TCP ports a process tree listens on: how
// `lazybox run` knows a command is a server (and can say "listening on
// http://localhost:3000"), and how the Runner shows a managed command's ports.
//
// The tree is read from the process table (ps), so a descendant that left the
// command's process group is still counted while its parent lives. Sockets
// come from lsof (macOS, and Linux when installed) or /proc (Linux).
package listenports

import (
	"bufio"
	"bytes"
	"context"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"time"
)

// Of returns the TCP ports the process tree rooted at pid listens on, lowest
// first, without duplicates. No ports and no error when nothing listens.
func Of(ctx context.Context, pid int) ([]uint32, error) {
	if pid <= 0 {
		return nil, errors.New("no process")
	}
	ctx, cancel := context.WithTimeout(ctx, 3*time.Second)
	defer cancel()
	pids, err := Tree(ctx, pid)
	if err != nil {
		return nil, err
	}
	if _, err := exec.LookPath("lsof"); err == nil {
		return viaLsof(ctx, pids)
	}
	if runtime.GOOS == "linux" {
		return viaProc(pids)
	}
	return nil, errors.New("lsof is not installed")
}

// Tree is pid and every process under it, from one read of the process table.
func Tree(ctx context.Context, pid int) ([]int, error) {
	out, err := exec.CommandContext(ctx, "ps", "-A", "-o", "pid=,ppid=").Output()
	if err != nil {
		return nil, err
	}
	children := map[int][]int{}
	scanner := bufio.NewScanner(bytes.NewReader(out))
	for scanner.Scan() {
		f := strings.Fields(scanner.Text())
		if len(f) < 2 {
			continue
		}
		child, err1 := strconv.Atoi(f[0])
		parent, err2 := strconv.Atoi(f[1])
		if err1 == nil && err2 == nil && child != parent {
			children[parent] = append(children[parent], child)
		}
	}
	seen := map[int]bool{}
	queue := []int{pid}
	var all []int
	for len(queue) > 0 {
		p := queue[0]
		queue = queue[1:]
		if seen[p] {
			continue
		}
		seen[p] = true
		all = append(all, p)
		queue = append(queue, children[p]...)
	}
	return all, nil
}

func viaLsof(ctx context.Context, pids []int) ([]uint32, error) {
	list := make([]string, len(pids))
	for i, p := range pids {
		list[i] = strconv.Itoa(p)
	}
	cmd := exec.CommandContext(ctx, "lsof", "-nP", "-a", "-iTCP", "-sTCP:LISTEN", "-p", strings.Join(list, ","), "-Fn")
	out, err := cmd.Output()
	var exit *exec.ExitError
	if err != nil && !(errors.As(err, &exit) && exit.ExitCode() == 1) {
		return nil, err // 1 is "nothing found"
	}
	return parseLsof(out), nil
}

// parseLsof reads lsof -Fn: "n*:3000", "n127.0.0.1:5173", "n[::1]:8080".
func parseLsof(out []byte) []uint32 {
	var ports []uint32
	for _, line := range strings.Split(string(out), "\n") {
		name, ok := strings.CutPrefix(line, "n")
		if !ok {
			continue
		}
		if i := strings.Index(name, "->"); i >= 0 {
			continue // a connection, not a listener
		}
		at := strings.LastIndex(name, ":")
		if at < 0 {
			continue
		}
		if port, err := strconv.ParseUint(name[at+1:], 10, 16); err == nil && port > 0 {
			ports = append(ports, uint32(port))
		}
	}
	return tidy(ports)
}

func viaProc(pids []int) ([]uint32, error) {
	inodes := map[string]bool{}
	for _, pid := range pids {
		dir := filepath.Join("/proc", strconv.Itoa(pid), "fd")
		entries, err := os.ReadDir(dir)
		if err != nil {
			continue
		}
		for _, e := range entries {
			target, err := os.Readlink(filepath.Join(dir, e.Name()))
			if err == nil && strings.HasPrefix(target, "socket:[") {
				inodes[strings.TrimSuffix(strings.TrimPrefix(target, "socket:["), "]")] = true
			}
		}
	}
	var ports []uint32
	for _, table := range []string{"/proc/net/tcp", "/proc/net/tcp6"} {
		data, err := os.ReadFile(table)
		if err != nil {
			continue
		}
		ports = append(ports, parseProcNet(data, inodes)...)
	}
	return tidy(ports), nil
}

// parseProcNet reads /proc/net/tcp: local address "0100007F:1F90", state 0A
// (LISTEN), and the socket's inode in the tenth column.
func parseProcNet(data []byte, inodes map[string]bool) []uint32 {
	var ports []uint32
	for i, line := range strings.Split(string(data), "\n") {
		f := strings.Fields(line)
		if i == 0 || len(f) < 10 || f[3] != "0A" || !inodes[f[9]] {
			continue
		}
		at := strings.LastIndex(f[1], ":")
		if port, err := strconv.ParseUint(f[1][at+1:], 16, 16); at >= 0 && err == nil && port > 0 {
			ports = append(ports, uint32(port))
		}
	}
	return ports
}

func tidy(ports []uint32) []uint32 {
	slices.Sort(ports)
	return slices.Compact(ports)
}
