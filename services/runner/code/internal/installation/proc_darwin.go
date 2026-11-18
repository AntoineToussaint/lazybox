package installation

import (
	"errors"
	"strconv"

	"golang.org/x/sys/unix"
)

// processStart is when pid started (the kernel's kinfo_proc start time):
// with the PID, it names one process, so a reused PID is never taken for
// the process recorded.
func processStart(pid int) (string, error) {
	info, err := unix.SysctlKinfoProc("kern.proc.pid", pid)
	if err != nil {
		return "", err
	}
	if info.Proc.P_pid != int32(pid) {
		return "", errors.New("no such process")
	}
	if info.Proc.P_stat == 5 { // SZOMB
		return "", errors.New("process exited")
	}
	t := info.Proc.P_starttime
	return strconv.FormatInt(int64(t.Sec), 10) + "." + strconv.FormatInt(int64(t.Usec), 10), nil
}
