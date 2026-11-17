package agent

import (
	"bytes"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"unicode/utf8"
)

// How an attached file reaches the agent (File.Delivery). The Runner decides
// it once, when it accepts the message, from the file's bytes and whether the
// agent can see images; the rule is the same for every provider, and each
// adapter only chooses the shape (ATTACHMENTS in docs/guides/chat.md):
const (
	// DeliverImage is an image the agent sees: the provider's own image input
	// (a Claude image block, a Codex image item, an ACP image block).
	DeliverImage = "image"
	// DeliverText is a file whose text goes in the message: an ACP embedded
	// text resource where the agent takes one, else a fenced block. Pasted
	// text is a text file, so it always arrives as its text.
	DeliverText = "text"
	// DeliverFile is a reference to the retained file, which the agent opens
	// with its own tools: a PDF, an archive, an image it cannot see, or text
	// too long to inline (an ACP resource link, else a line in the message).
	DeliverFile = "file"
)

// InlineTextBytes is the most text of one file put in a message; a longer
// file is delivered by reference. Four of them stay well inside any context.
const InlineTextBytes = 256 << 10

// IsText is bytes a person reads as text: valid UTF-8 with no NUL byte, and
// not a document format that happens to be ASCII (a PDF, PostScript).
func IsText(data []byte) bool {
	return utf8.Valid(data) && !bytes.ContainsRune(data, 0) && strings.HasPrefix(http.DetectContentType(data), "text/")
}

// Delivery is how a file with these bytes reaches the agent; image says the
// Runner is sending it as a native image (the agent sees images, and the file
// is one it can take).
func Delivery(data []byte, image bool) string {
	switch {
	case image:
		return DeliverImage
	case len(data) <= InlineTextBytes && IsText(data):
		return DeliverText
	}
	return DeliverFile
}

// Kinds of Part.
const (
	// PartText is the person's own text.
	PartText = "text"
	// PartImage is an image's bytes (Image), of an attached file or not.
	PartImage = "image"
	// PartFileText is an attached file's text (Text), for File.
	PartFileText = "file_text"
	// PartFileReference is an attached file the agent reads itself (File).
	PartFileReference = "file_reference"
)

// Part is one piece of a message as the agent receives it.
type Part struct {
	Kind  string
	Text  string
	Image *Image
	File  *File
}

// Parts is a message in the order the agent receives it, whatever the
// provider: the person's text first, then each attachment in the order the
// person arranged them, each as its delivery says. A file whose text cannot be
// read back is delivered by reference rather than dropped. An instruction
// update (op.Context) is not a part: each adapter puts it ahead, out of band
// where the provider allows.
func Parts(op Operation) []Part {
	parts := []Part{}
	if op.Text != "" {
		parts = append(parts, Part{Kind: PartText, Text: op.Text})
	}
	native := 0
	for _, f := range op.Files {
		if f.Delivery == DeliverImage {
			native++
		}
	}
	// Images sent without a file (an older client's) come first.
	loose := max(len(op.Images)-native, 0)
	for i := range loose {
		parts = append(parts, Part{Kind: PartImage, Image: &op.Images[i]})
	}
	next := loose
	for i := range op.Files {
		f := &op.Files[i]
		switch f.Delivery {
		case DeliverImage:
			if next < len(op.Images) {
				parts = append(parts, Part{Kind: PartImage, Image: &op.Images[next], File: f})
				next++
				continue
			}
		case DeliverText:
			if text, ok := readText(f.Path); ok {
				parts = append(parts, Part{Kind: PartFileText, Text: text, File: f})
				continue
			}
		}
		parts = append(parts, Part{Kind: PartFileReference, File: f})
	}
	return parts
}

func readText(path string) (string, bool) {
	f, err := os.Open(path)
	if err != nil {
		return "", false
	}
	defer f.Close()
	data, err := io.ReadAll(io.LimitReader(f, InlineTextBytes+1))
	if err != nil || len(data) > InlineTextBytes || !IsText(data) {
		return "", false
	}
	return string(data), true
}

// Inline is an attached file's text as a fenced block, for a provider that
// takes no text file of its own: a line naming the file, then the text in a
// fence longer than any run of backticks inside it, tagged with the language
// its name says.
func Inline(f File, text string) string {
	fence := strings.Repeat("`", max(3, longestRun(text, '`')+1))
	lines := strings.Count(strings.TrimRight(text, "\n"), "\n") + 1
	body := strings.TrimRight(text, "\n")
	return fmt.Sprintf("Attached file %q (%s):\n%s%s\n%s\n%s", f.Name, plural(lines, "line"), fence, Language(f.Name), body, fence)
}

// Reference is the line a provider without file links gets for a file the
// agent reads with its own tools.
func Reference(f File) string {
	return fmt.Sprintf("Attached file %q (%s, %s), retained on this Runner at %q: read it as needed.", f.Name, mediaOr(f.MediaType), humanBytes(f.Size), f.Path)
}

// Flatten is a message as one text, for a provider that takes only text and
// images: the person's text, then each file's text or reference, in order.
// Images are left out; the caller sends them as images.
func Flatten(parts []Part) string {
	out := []string{}
	for _, p := range parts {
		switch p.Kind {
		case PartText:
			out = append(out, p.Text)
		case PartFileText:
			out = append(out, Inline(*p.File, p.Text))
		case PartFileReference:
			out = append(out, Reference(*p.File))
		}
	}
	return strings.Join(out, "\n\n")
}

// FileURI is a retained file's file: URI.
func FileURI(path string) string {
	return (&url.URL{Scheme: "file", Path: filepath.ToSlash(path)}).String()
}

// Language is the fence tag a file's extension implies, or none.
func Language(name string) string {
	return languages[strings.ToLower(strings.TrimPrefix(filepath.Ext(name), "."))]
}

var languages = map[string]string{
	"go": "go", "ts": "typescript", "tsx": "tsx", "js": "javascript", "jsx": "jsx", "mjs": "javascript",
	"py": "python", "rs": "rust", "rb": "ruby", "java": "java", "kt": "kotlin", "swift": "swift",
	"c": "c", "h": "c", "cc": "cpp", "cpp": "cpp", "cs": "csharp", "php": "php", "sh": "sh", "bash": "bash", "zsh": "zsh",
	"json": "json", "yaml": "yaml", "yml": "yaml", "toml": "toml", "xml": "xml", "html": "html", "css": "css",
	"sql": "sql", "md": "markdown", "diff": "diff", "patch": "diff", "proto": "protobuf", "log": "log",
}

func longestRun(text string, r rune) int {
	best, run := 0, 0
	for _, c := range text {
		if c == r {
			run++
			best = max(best, run)
		} else {
			run = 0
		}
	}
	return best
}

func plural(n int, word string) string {
	if n == 1 {
		return "1 " + word
	}
	return fmt.Sprintf("%d %ss", n, word)
}

func mediaOr(mediaType string) string {
	if mediaType == "" {
		return "application/octet-stream"
	}
	return mediaType
}

func humanBytes(n int64) string {
	switch {
	case n < 1<<10:
		return fmt.Sprintf("%d B", n)
	case n < 1<<20:
		return fmt.Sprintf("%d KiB", (n+1023)>>10)
	}
	return fmt.Sprintf("%.1f MiB", float64(n)/(1<<20))
}
