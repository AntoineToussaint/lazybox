package attachments

import (
	"bytes"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// pngBytes is a 1×1 PNG: enough for content sniffing to call it image/png.
var pngBytes = []byte("\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x06\x00\x00\x00\x1f\x15\xc4\x89\x00\x00\x00\rIDATx\x9cc\xf8\xff\xff?\x00\x05\xfe\x02\xfe\xa7\x35\x81\x84\x00\x00\x00\x00IEND\xaeB`\x82")

func TestReadReturnsOnlyRetainedDisplayableImages(t *testing.T) {
	root := t.TempDir()
	store, err := New(root)
	if err != nil {
		t.Fatal(err)
	}
	// The sender's media type is not trusted: a PNG declared as text is still a PNG,
	// and a text file declared as an image is not one.
	files, err := store.Put("execution", []Input{{Name: "shot.png", MediaType: "text/plain", Data: pngBytes}, {Name: "fake.png", MediaType: "image/png", Data: []byte("<svg onload=alert(1)>")}, {Name: "notes.txt", Data: []byte("plain")}, {Name: "archive.bin", Data: []byte{0x1f, 0x8b, 0, 0xff}}})
	if err != nil {
		t.Fatal(err)
	}
	file, data, err := store.Read(files[0].Path)
	if err != nil || !bytes.Equal(data, pngBytes) || file.MediaType != "image/png" || file.Name != "shot.png" || file.Size != int64(len(pngBytes)) {
		t.Fatalf("retained image not readable: %+v %v", file, err)
	}
	// Text is previewable, always as plain text: markup declared as an image
	// is shown as its source, never rendered.
	for _, path := range []string{files[1].Path, files[2].Path} {
		if file, _, err := store.Read(path); err != nil || file.MediaType != TextMediaType {
			t.Fatalf("%s: text not previewable as plain text: %+v %v", path, file, err)
		}
	}
	if _, _, err := store.Read(files[3].Path); !errors.Is(err, ErrNotPreviewable) {
		t.Fatalf("binary readable for preview: %v", err)
	}
	// A restarted Runner (a new Store over the same root) still serves it.
	again, err := New(root)
	if err != nil {
		t.Fatal(err)
	}
	if _, _, err := again.Read(files[0].Path); err != nil {
		t.Fatalf("image unreadable after restart: %v", err)
	}
	// Deleting the conversation removes what it sent.
	if err := store.Delete("execution"); err != nil {
		t.Fatal(err)
	}
	if _, _, err := store.Read(files[0].Path); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("deleted attachment readable: %v", err)
	}
}

func TestReadRefusesEveryPathPutDidNotMake(t *testing.T) {
	root := t.TempDir()
	store, err := New(root)
	if err != nil {
		t.Fatal(err)
	}
	files, err := store.Put("execution", []Input{{Name: "shot.png", Data: pngBytes}})
	if err != nil {
		t.Fatal(err)
	}
	stored := files[0].Path
	digest := filepath.Base(filepath.Dir(stored))
	outside := filepath.Join(t.TempDir(), "secret.png")
	if err := os.WriteFile(outside, pngBytes, 0600); err != nil {
		t.Fatal(err)
	}
	// A link inside the store that points out of it.
	linkDir := filepath.Join(root, "execution", strings.Repeat("a", 64))
	if err := os.MkdirAll(linkDir, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(outside, filepath.Join(linkDir, "linked.png")); err != nil {
		t.Fatal(err)
	}
	for name, path := range map[string]string{
		"empty":            "",
		"relative":         "execution/" + digest + "/shot.png",
		"outside the root": outside,
		"traversal":        filepath.Join(root, "execution", digest, "..", "..", "..", filepath.Base(outside)),
		"unclean":          root + "/execution/./" + digest + "/shot.png",
		"too shallow":      filepath.Join(root, "execution", "shot.png"),
		"too deep":         filepath.Join(root, "execution", digest, "shot.png", "x"),
		"not a digest":     filepath.Join(root, "execution", "not-a-digest", "shot.png"),
		"upper-case hex":   filepath.Join(root, "execution", strings.ToUpper(digest), "shot.png"),
		"the root itself":  root,
		"symlink out":      filepath.Join(linkDir, "linked.png"),
	} {
		if _, data, err := store.Read(path); err == nil || data != nil {
			t.Errorf("%s: %q was readable", name, path)
		}
	}
	// Content that no longer matches its digest is not served as that attachment.
	if err := os.WriteFile(stored, append(append([]byte{}, pngBytes...), 0), 0600); err != nil {
		t.Fatal(err)
	}
	if _, _, err := store.Read(stored); err == nil {
		t.Fatal("changed attachment served under its old digest")
	}
	// A file over the per-file bound is refused before it is read.
	big := filepath.Join(root, "execution", strings.Repeat("b", 64), "big.png")
	if err := os.MkdirAll(filepath.Dir(big), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(big, pngBytes, 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.Truncate(big, MaxFileBytes+1); err != nil {
		t.Fatal(err)
	}
	if _, _, err := store.Read(big); !errors.Is(err, ErrTooLarge) {
		t.Fatalf("oversized attachment: %v", err)
	}
}
