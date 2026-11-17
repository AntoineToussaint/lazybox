package agent

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestDeliveryIsTheSameRuleForEveryProvider(t *testing.T) {
	for _, tc := range []struct {
		name  string
		data  []byte
		image bool
		want  string
	}{
		{"an image the agent sees", []byte("\x89PNG\r\n\x1a\n"), true, DeliverImage},
		{"pasted text", []byte("line one\nline two\n"), false, DeliverText},
		{"source with unicode", []byte("// café ☕\npackage main\n"), false, DeliverText},
		{"an ASCII PDF is still a PDF", []byte("%PDF-1.7\n1 0 obj\n"), false, DeliverFile},
		{"binary", []byte{0x1f, 0x8b, 0, 1}, false, DeliverFile},
		{"invalid UTF-8", []byte{'a', 0xff, 'b'}, false, DeliverFile},
		{"text too long to inline", []byte(strings.Repeat("x", InlineTextBytes+1)), false, DeliverFile},
	} {
		if got := Delivery(tc.data, tc.image); got != tc.want {
			t.Errorf("%s: got %q, want %q", tc.name, got, tc.want)
		}
	}
}

func TestPartsKeepThePersonsTextThenEachAttachmentInOrder(t *testing.T) {
	dir := t.TempDir()
	write := func(name, content string) string {
		path := filepath.Join(dir, name)
		if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
			t.Fatal(err)
		}
		return path
	}
	op := Operation{
		Text: "Why does this fail?",
		Files: []File{
			{Name: "Pasted text 2026-11-17 at 10.31.22.py", MediaType: "text/plain", Path: write("paste.py", "def f():\n    return 1\n"), Size: 22, Delivery: DeliverText},
			{Name: "shot.png", MediaType: "image/png", Path: "/r/shot.png", Size: 10, Delivery: DeliverImage},
			{Name: "report.pdf", MediaType: "application/pdf", Path: "/r/report.pdf", Size: 2048, Delivery: DeliverFile},
			{Name: "gone.txt", MediaType: "text/plain", Path: filepath.Join(dir, "missing.txt"), Size: 4, Delivery: DeliverText},
		},
		// An older client's image without a file, then the file's image.
		Images: []Image{{MediaType: "image/gif", Data: "loose"}, {MediaType: "image/png", Data: "shot"}},
	}
	parts := Parts(op)
	kinds := []string{}
	for _, p := range parts {
		kinds = append(kinds, p.Kind)
	}
	if strings.Join(kinds, ",") != "text,image,file_text,image,file_reference,file_reference" {
		t.Fatalf("order: %v", kinds)
	}
	if parts[0].Text != op.Text || parts[1].Image.Data != "loose" || parts[2].Text != "def f():\n    return 1\n" || parts[3].Image.Data != "shot" || parts[3].File.Name != "shot.png" || parts[4].File.Name != "report.pdf" {
		t.Fatalf("parts: %+v", parts)
	}
	// Text that cannot be read back is referenced, not dropped.
	if parts[5].File.Name != "gone.txt" {
		t.Fatalf("unreadable text dropped: %+v", parts[5])
	}
	flat := Flatten(parts)
	if !strings.HasPrefix(flat, "Why does this fail?\n\nAttached file \"Pasted text 2026-11-17 at 10.31.22.py\" (2 lines):\n```python\ndef f():\n    return 1\n```\n\nAttached file \"report.pdf\" (application/pdf, 2 KiB)") || strings.Contains(flat, "shot") {
		t.Fatalf("flattened: %q", flat)
	}
}

func TestInlineFencesAroundAnyBackticksInside(t *testing.T) {
	got := Inline(File{Name: "notes.md"}, "Run:\n```sh\nmake\n```\n")
	want := "Attached file \"notes.md\" (4 lines):\n````markdown\nRun:\n```sh\nmake\n```\n````"
	if got != want {
		t.Fatalf("got %q\nwant %q", got, want)
	}
	if got := Inline(File{Name: "Pasted text.txt"}, "one line"); got != "Attached file \"Pasted text.txt\" (1 line):\n```\none line\n```" {
		t.Fatalf("plain: %q", got)
	}
}

func TestFileURIEscapesThePath(t *testing.T) {
	if got := FileURI("/state/attachments/x/Pasted text #1.txt"); got != "file:///state/attachments/x/Pasted%20text%20%231.txt" {
		t.Fatal(got)
	}
}
