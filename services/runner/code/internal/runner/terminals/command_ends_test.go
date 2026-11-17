package terminals

import "testing"

// The shell's command-end marks are counted with their status, including one
// cut across two chunks of output, and never twice.
func TestCommandEndsAreCountedAcrossChunks(t *testing.T) {
	var s Status
	var tail []byte
	feed := func(chunk string) { tail = countCommandEnds(&s, append(tail, chunk...)) }
	feed("fixture shell\r\n\x1b]133;D;0\x07$ ")
	if s.CommandEnds != 1 || s.LastCommandStatus != 0 || tail != nil {
		t.Fatalf("first prompt: %+v %q", s, tail)
	}
	feed("acme login\r\nsigned in\r\n\x1b]133;D;")
	if s.CommandEnds != 1 {
		t.Fatalf("a mark cut short is not counted yet: %+v", s)
	}
	feed("130\x07$ ")
	if s.CommandEnds != 2 || s.LastCommandStatus != 130 {
		t.Fatalf("the rest of the mark completes it: %+v", s)
	}
	feed("\x1b[1mbold\x1b[0m and \x1b]133;D;2\x1b\\$ ")
	if s.CommandEnds != 3 || s.LastCommandStatus != 2 || tail != nil {
		t.Fatalf("ST-terminated mark, colours ignored: %+v %q", s, tail)
	}
}
