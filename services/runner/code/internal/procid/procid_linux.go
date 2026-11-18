package procid

import (
	"bufio"
	"errors"
	"os"
	"strconv"
	"strings"
	"time"
)

// startOf is the process's start, from /proc/<pid>/stat: the boot time (so a
// value never repeats across boots) and the clock ticks after boot at which
// the process started, combined into one comparable number.
func startOf(pid int) (int64, error) {
	data, err := os.ReadFile("/proc/" + strconv.Itoa(pid) + "/stat")
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return 0, ErrGone
		}
		return 0, err
	}
	// The command name (field 2) is in parentheses and may hold spaces.
	end := strings.LastIndexByte(string(data), ')')
	if end < 0 {
		return 0, errors.New("unreadable process status")
	}
	fields := strings.Fields(string(data[end+1:]))
	// Field 22 (starttime) is the 20th after the name.
	if len(fields) < 20 {
		return 0, errors.New("unreadable process status")
	}
	ticks, err := strconv.ParseInt(fields[19], 10, 64)
	if err != nil {
		return 0, err
	}
	boot, err := bootTime()
	if err != nil {
		return 0, err
	}
	return boot.Unix()*1_000_000_000 + ticks, nil
}

func bootTime() (time.Time, error) {
	f, err := os.Open("/proc/stat")
	if err != nil {
		return time.Time{}, err
	}
	defer f.Close()
	scanner := bufio.NewScanner(f)
	for scanner.Scan() {
		if value, ok := strings.CutPrefix(scanner.Text(), "btime "); ok {
			seconds, err := strconv.ParseInt(strings.TrimSpace(value), 10, 64)
			if err != nil {
				return time.Time{}, err
			}
			return time.Unix(seconds, 0), nil
		}
	}
	return time.Time{}, errors.New("boot time not found")
}
