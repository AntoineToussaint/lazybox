package procid

import (
	"time"

	"golang.org/x/sys/unix"
)

// startOf is the process's start time in microseconds since the epoch, from
// the kernel's process table (kern.proc.pid).
func startOf(pid int) (int64, error) {
	info, err := unix.SysctlKinfoProc("kern.proc.pid", pid)
	if err != nil {
		return 0, err
	}
	t := info.Proc.P_starttime
	return t.Sec*1_000_000 + int64(t.Usec), nil
}

func bootTime() (time.Time, error) {
	t, err := unix.SysctlTimeval("kern.boottime")
	if err != nil {
		return time.Time{}, err
	}
	return time.Unix(t.Sec, int64(t.Usec)*1000), nil
}
