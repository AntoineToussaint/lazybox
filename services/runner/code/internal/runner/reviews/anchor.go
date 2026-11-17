package reviews

import (
	"crypto/sha256"
	"encoding/hex"
	"strings"
)

// Freshness says how a comment's lines relate to the file as it is now, the
// per-comment counterpart of V1's review Freshness: a comment is only ever
// shown as current when its quoted lines are provably still there.
type Freshness string

const (
	// Current: the quoted lines are still at the lines the comment was made on.
	Current Freshness = "current"
	// Moved: the same lines exist elsewhere in the file; the comment re-anchors
	// to the occurrence nearest where it was made.
	Moved Freshness = "moved"
	// Outdated: the quoted lines are no longer in the file (or the file is
	// gone). The comment keeps its original snippet so it still reads.
	Outdated Freshness = "outdated"
	// Unknown: the file could not be read (too large, binary, unreadable).
	Unknown Freshness = "unknown"
)

// Anchor is where a comment points now.
type Anchor struct {
	Freshness Freshness
	// StartLine and EndLine are 1-based and inclusive in the current file; zero
	// when the lines are not there.
	StartLine, EndLine int
	// Reason explains anything other than Current, in a sentence.
	Reason string
}

// normalize ignores what an editor or formatter changes without changing the
// line: trailing whitespace and a carriage return.
func normalize(line string) string { return strings.TrimRight(line, " \t\r") }

// Hash is the content hash of a run of lines, as stored with a comment.
func Hash(lines []string) string {
	h := sha256.New()
	for i, line := range lines {
		if i > 0 {
			h.Write([]byte{'\n'})
		}
		h.Write([]byte(normalize(line)))
	}
	return hex.EncodeToString(h.Sum(nil))
}

// FileHash is the hash of a whole file's content.
func FileHash(content []byte) string {
	sum := sha256.Sum256(content)
	return hex.EncodeToString(sum[:])
}

// Lines splits file content the way a diff numbers it: a final newline does
// not start another line.
func Lines(content string) []string {
	if content == "" {
		return nil
	}
	return strings.Split(strings.TrimSuffix(content, "\n"), "\n")
}

// Locate finds snippet in file and returns the 1-based start line of the
// occurrence nearest hint (the line the comment was made on), or 0 when the
// lines are not there. Ties go to the earlier occurrence.
func Locate(file, snippet []string, hint int) int {
	n := len(snippet)
	if n == 0 || n > len(file) {
		return 0
	}
	want := make([]string, n)
	for i, line := range snippet {
		want[i] = normalize(line)
	}
	best, bestDistance := 0, -1
	for start := 0; start+n <= len(file); start++ {
		match := true
		for i := 0; i < n; i++ {
			if normalize(file[start+i]) != want[i] {
				match = false
				break
			}
		}
		if !match {
			continue
		}
		distance := start + 1 - hint
		if distance < 0 {
			distance = -distance
		}
		if bestDistance < 0 || distance < bestDistance {
			best, bestDistance = start+1, distance
		}
	}
	return best
}

// Evaluate anchors a comment against the file as it is now. exists is false
// when the file is gone. A comment on removed lines ("old" side) quotes lines
// that are not in the file by design, so it stays current only while the file
// is byte-for-byte what it was when the comment was made.
func Evaluate(c Comment, file []byte, exists bool) Anchor {
	if !exists {
		return Anchor{Freshness: Outdated, Reason: "The file no longer exists."}
	}
	if c.Side == SideOld {
		if c.FileHash != "" && FileHash(file) == c.FileHash {
			return Anchor{Freshness: Current}
		}
		return Anchor{Freshness: Outdated, Reason: "The file changed since; the removed lines are quoted as they were."}
	}
	start := Locate(Lines(string(file)), c.Snippet, c.StartLine)
	if start == 0 {
		return Anchor{Freshness: Outdated, Reason: "These lines changed since the comment was made."}
	}
	anchor := Anchor{Freshness: Current, StartLine: start, EndLine: start + len(c.Snippet) - 1}
	if start != c.StartLine {
		anchor.Freshness = Moved
		anchor.Reason = "The same lines moved; the comment follows them."
	}
	return anchor
}
