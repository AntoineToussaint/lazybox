package installation

import (
	"errors"
	"os"
	"strconv"
	"strings"
)

// processStart is when pid started, in clock ticks since boot (field 22 of
// /proc/<pid>/stat): with the PID, it names one process for as long as the
// machine runs, so a reused PID is never taken for the process recorded.
func processStart(pid int) (string, error) {
	data, err := os.ReadFile("/proc/" + strconv.Itoa(pid) + "/stat")
	if err != nil {
		return "", err
	}
	// The command name (field 2) is in parentheses and may hold spaces or
	// parentheses itself: the fields after it start after the last ')'.
	text := string(data)
	end := strings.LastIndexByte(text, ')')
	if end < 0 {
		return "", errors.New("unreadable /proc stat")
	}
	fields := strings.Fields(text[end+1:])
	// fields[0] is field 3 (state); starttime is field 22.
	if len(fields) < 20 {
		return "", errors.New("short /proc stat")
	}
	if fields[0] == "Z" || fields[0] == "X" {
		return "", errors.New("process exited")
	}
	return fields[19], nil
}
