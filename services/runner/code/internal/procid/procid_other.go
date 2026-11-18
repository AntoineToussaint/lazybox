//go:build !darwin && !linux

package procid

import (
	"errors"
	"time"
)

var errUnsupported = errors.New("process identity is not supported on this system")

func startOf(int) (int64, error) { return 0, errUnsupported }

func bootTime() (time.Time, error) { return time.Time{}, errUnsupported }
