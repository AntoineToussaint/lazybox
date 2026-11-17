// Package titles names conversations. A conversation's shown title is, in
// order: the name the person gave it, a title generated after its first turn,
// or Fallback of its first message. Nothing here calls a provider: the Runner
// asks the conversation's own agent (agent.Titler) and passes the answer
// through Tidy.
package titles

import (
	"regexp"
	"strings"
	"unicode"
	"unicode/utf8"
)

// Limit is the longest fallback title, in runes, before its ellipsis.
const Limit = 60

// generatedLimit bounds a generated title: a provider that ignores "3 to 7
// words" is cut at a word, never mid-word.
const generatedLimit = 80

var (
	fence    = regexp.MustCompile("(?s)```.*?(```|$)")
	markup   = regexp.MustCompile("[#>*_`~]+")
	listMark = regexp.MustCompile(`^(?:[-+•]|\d+[.)])\s+`)
	spaces   = regexp.MustCompile(`\s+`)
	// filler opens a request without saying what it is about. Matched at the
	// start, case-insensitively, with what follows it kept.
	filler = regexp.MustCompile(`(?i)^(?:(?:hi|hey|hello|ok|okay|so|alright|please|pls)\b[\s,.!:;-]*|(?:i|we)\s+(?:want|wanna|need|have)\s+(?:you\s+)?to\b\s*|(?:i|we)\s*(?:'d|’d|\s+would)\s+like\s+(?:you\s+)?to\b\s*|(?:i|we)\s*(?:'m|’m|'re|’re|\s+am|\s+are)\s+(?:going|trying)\s+to\b\s*|(?:can|could|would|will)\s+(?:you|we)\s+(?:please\s+)?|let'?s\b\s*|let’s\b\s*|help\s+me\s+(?:to\s+)?|i\s+think\s+(?:we\s+should\s+)?|we\s+should\s+)`)
	// dangling words end a cut title badly ("…the goal is to…").
	dangling = map[string]bool{"a": true, "an": true, "the": true, "to": true, "of": true, "and": true, "or": true, "but": true, "for": true, "with": true, "in": true, "on": true, "at": true, "is": true, "are": true, "be": true, "we": true, "i": true, "it": true, "that": true, "this": true, "so": true, "as": true, "by": true, "from": true, "into": true, "about": true, "if": true, "then": true, "my": true, "our": true, "your": true}
	quotes   = "\"'`“”‘’«»"
)

// Fallback is a title made from a first message when there is no better one:
// its first sentence or line, without markup and the words that only open a
// request ("I want to", "can you"), capitalised, and cut at a word with an
// ellipsis when it is long. It never cuts inside a word unless the first word
// alone is longer than Limit.
func Fallback(text string) string {
	line := firstLine(text)
	if line == "" {
		return ""
	}
	for i := 0; i < 4; i++ {
		trimmed := strings.TrimLeft(filler.ReplaceAllString(line, ""), " ,.:;-")
		if trimmed == line || utf8.RuneCountInString(trimmed) < 3 {
			break
		}
		line = trimmed
	}
	line = capitalise(line)
	if sentence := firstSentence(line); sentence != "" && utf8.RuneCountInString(sentence) <= Limit {
		return strings.TrimRight(sentence, ".!:;,")
	}
	if utf8.RuneCountInString(line) <= Limit {
		return strings.TrimRight(line, ".!:;,")
	}
	return cut(line, Limit)
}

// Tidy makes a provider's generated title safe to show: one line, no
// surrounding quotes, "Title:" prefix or closing full stop, at most
// generatedLimit runes cut at a word. It returns "" for nothing usable.
func Tidy(generated string) string {
	line := ""
	for _, l := range strings.Split(generated, "\n") {
		if l = strings.TrimSpace(l); l != "" {
			line = l
			break
		}
	}
	line = strings.Map(func(r rune) rune {
		if unicode.IsControl(r) {
			return -1
		}
		return r
	}, line)
	line = spaces.ReplaceAllString(line, " ")
	if head, rest, ok := strings.Cut(line, ":"); ok && strings.EqualFold(strings.TrimSpace(head), "title") {
		line = strings.TrimSpace(rest)
	}
	line = strings.Trim(line, quotes+" ")
	line = strings.TrimRight(line, ".")
	line = strings.Trim(line, quotes+" ")
	if line == "" || !strings.ContainsFunc(line, func(r rune) bool { return unicode.IsLetter(r) || unicode.IsDigit(r) }) {
		return ""
	}
	if utf8.RuneCountInString(line) > generatedLimit {
		line = cut(line, generatedLimit)
	}
	return capitalise(line)
}

// Description is what a provider is given to name a conversation: its first
// message and, when there is one, the end of the agent's reply, both bounded.
func Description(first, reply string) string {
	first = bound(strings.TrimSpace(first), 2000)
	reply = bound(strings.TrimSpace(reply), 600)
	if reply == "" {
		return first
	}
	return first + "\n\n(The agent's reply ended with: " + reply + ")"
}

// bound keeps at most n runes of s.
func bound(s string, n int) string {
	if utf8.RuneCountInString(s) <= n {
		return s
	}
	return string([]rune(s)[:n])
}

// firstLine is the first line of text with words, outside code blocks and
// without markdown markup or a list marker.
func firstLine(text string) string {
	text = fence.ReplaceAllString(text, "\n")
	for _, l := range strings.Split(text, "\n") {
		l = strings.TrimSpace(markup.ReplaceAllString(l, ""))
		l = strings.TrimSpace(listMark.ReplaceAllString(l, ""))
		l = spaces.ReplaceAllString(l, " ")
		if strings.ContainsFunc(l, func(r rune) bool { return unicode.IsLetter(r) || unicode.IsDigit(r) }) {
			return l
		}
	}
	return ""
}

// firstSentence is line up to its first full stop, question or exclamation
// mark that ends a sentence (followed by a space), or "" when there is none.
func firstSentence(line string) string {
	for i, r := range line {
		if r != '.' && r != '?' && r != '!' {
			continue
		}
		next := i + 1
		if next < len(line) && line[next] != ' ' {
			continue // "v2.1", "e.g.x", a path
		}
		sentence := strings.TrimSpace(line[:next])
		if utf8.RuneCountInString(sentence) < 12 {
			continue // "Hi." or "Ok." opens it; keep looking
		}
		return sentence
	}
	return ""
}

// cut shortens line to at most limit runes at a word boundary, drops the
// small words a cut leaves dangling, and adds an ellipsis.
func cut(line string, limit int) string {
	runes := []rune(line)
	if len(runes) <= limit {
		return line
	}
	head := string(runes[:limit])
	if space := strings.LastIndex(head, " "); space > 0 {
		head = head[:space]
	} else {
		// One word longer than the limit: there is no word boundary to cut at.
		head = string(runes[:limit-1])
	}
	words := strings.Fields(head)
	for len(words) > 1 && dangling[strings.ToLower(strings.Trim(words[len(words)-1], ",.;:!?-—"))] {
		words = words[:len(words)-1]
	}
	return strings.TrimRight(strings.Join(words, " "), " ,.;:!?-—") + "…"
}

// capitalise upper-cases a lower-case first letter.
func capitalise(s string) string {
	r, size := utf8.DecodeRuneInString(s)
	if r == utf8.RuneError || !unicode.IsLower(r) {
		return s
	}
	return string(unicode.ToUpper(r)) + s[size:]
}
