package listenports

import (
	"context"
	"net"
	"os"
	"os/exec"
	"slices"
	"strconv"
	"strings"
	"testing"
	"time"
)

// A child that opens a listener (here: a grandchild, under a shell) is found
// from the root's pid, and only ports the tree listens on are reported.
func TestOfFindsAListenerInTheProcessTree(t *testing.T) {
	if os.Getenv("LISTENPORTS_CHILD") == "1" {
		l, err := net.Listen("tcp", "127.0.0.1:0")
		if err != nil {
			os.Exit(2)
		}
		os.Stdout.WriteString(strconv.Itoa(l.Addr().(*net.TCPAddr).Port) + "\n")
		time.Sleep(time.Minute)
		return
	}
	// A port held by this test process itself, outside the tree.
	outside, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer outside.Close()
	cmd := exec.Command("/bin/sh", "-c", "\""+os.Args[0]+"\" -test.run=TestOfFindsAListenerInTheProcessTree; true")
	cmd.Env = append(os.Environ(), "LISTENPORTS_CHILD=1")
	out, err := cmd.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err = cmd.Start(); err != nil {
		t.Fatal(err)
	}
	defer func() { _ = cmd.Process.Kill(); _ = cmd.Wait() }()
	line := make([]byte, 16)
	n, err := out.Read(line)
	if err != nil {
		t.Fatal(err)
	}
	want, _ := strconv.Atoi(strings.TrimSpace(string(line[:n])))
	var ports []uint32
	for deadline := time.Now().Add(5 * time.Second); time.Now().Before(deadline); time.Sleep(100 * time.Millisecond) {
		if ports, err = Of(context.Background(), cmd.Process.Pid); err == nil && len(ports) > 0 {
			break
		}
	}
	if err != nil || !slices.Equal(ports, []uint32{uint32(want)}) {
		t.Fatalf("ports %v (%v), want [%d] and not the test's own %d", ports, err, want, outside.Addr().(*net.TCPAddr).Port)
	}
}

func TestOfReportsNothingForAProcessThatDoesNotListen(t *testing.T) {
	cmd := exec.Command("sleep", "30")
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	defer func() { _ = cmd.Process.Kill(); _ = cmd.Wait() }()
	ports, err := Of(context.Background(), cmd.Process.Pid)
	if err != nil || len(ports) != 0 {
		t.Fatalf("%v %v", ports, err)
	}
}

func TestParsers(t *testing.T) {
	got := parseLsof([]byte("p12\nf5\nn*:3000\nf6\nn127.0.0.1:5173\nf7\nn[::1]:3000\nf8\nn127.0.0.1:5000->127.0.0.1:61000\n"))
	if !slices.Equal(got, []uint32{3000, 5173}) {
		t.Fatalf("lsof: %v", got)
	}
	table := "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n" +
		"   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0\n" +
		"   1: 0100007F:1F91 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 9999 1 0\n" +
		"   2: 0100007F:1F92 0100007F:C350 01 00000000:00000000 00:00000000 00000000  1000        0 4243 1 0\n"
	if got := parseProcNet([]byte(table), map[string]bool{"4242": true, "4243": true}); !slices.Equal(got, []uint32{8080}) {
		t.Fatalf("proc: %v", got)
	}
}
