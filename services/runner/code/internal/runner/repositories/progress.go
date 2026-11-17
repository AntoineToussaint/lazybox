package repositories

import (
	"context"
	"regexp"
	"strconv"
	"strings"
)

// Only recognized stage names and numeric counters may leave the Git process.
// Raw stderr can contain URLs or credentials and is deliberately never forwarded.
var transferLine = regexp.MustCompile(`^(?:remote: )?(Counting objects|Compressing objects|Receiving objects|Resolving deltas):\s+([0-9]{1,3})% \(([0-9]+)/([0-9]+)\)`)

type progressWriter struct {
	line    []byte
	discard bool
	last    string
	emit    func(string) error
	cancel  context.CancelFunc
}

func (w *progressWriter) Write(p []byte) (int, error) {
	for _, b := range p {
		if b == '\r' || b == '\n' {
			if !w.discard {
				match := transferLine.FindStringSubmatch(strings.TrimSpace(string(w.line)))
				if len(match) > 0 {
					percent, _ := strconv.Atoi(match[2])
					message := match[1] + ": " + match[2] + "% (" + match[3] + "/" + match[4] + ")"
					if percent <= 100 && message != w.last {
						w.last = message
						if err := w.emit(message); err != nil {
							w.cancel()
							return 0, err
						}
					}
				}
			}
			w.line = w.line[:0]
			w.discard = false
		} else if len(w.line) < 4096 && !w.discard {
			w.line = append(w.line, b)
		} else {
			w.discard = true
		}
	}
	return len(p), nil
}
