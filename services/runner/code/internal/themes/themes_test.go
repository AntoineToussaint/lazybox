package themes

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func write(t *testing.T, dir, name, content string) {
	t.Helper()
	if err := os.WriteFile(filepath.Join(dir, name), []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
}

func list(t *testing.T, dir string) Folder {
	t.Helper()
	folder, err := List(dir)
	if err != nil {
		t.Fatal(err)
	}
	return folder
}

func only(t *testing.T, folder Folder) File {
	t.Helper()
	if len(folder.Files) != 1 {
		t.Fatalf("want one file, got %+v", folder.Files)
	}
	return folder.Files[0]
}

func TestListMissingFolderIsEmpty(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "themes")
	folder := list(t, dir)
	if folder.Directory != dir || len(folder.Files) != 0 || folder.Revision == "" {
		t.Fatalf("%+v", folder)
	}
	if _, err := os.Stat(dir); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("List created the folder", err)
	}
}

func TestListParsesJSONToCompactJSON(t *testing.T) {
	dir := t.TempDir()
	write(t, dir, "Dusk.JSON", "{\n  \"name\": \"Dusk\",\n  \"tokens\": {\"accent\": \"#c0ffee\", \"radius\": 1.50}\n}\n")
	f := only(t, list(t, dir))
	if f.Name != "Dusk.JSON" || f.Format != "json" || f.Error != "" || f.Modified.IsZero() {
		t.Fatalf("%+v", f)
	}
	if want := `{"name":"Dusk","tokens":{"accent":"#c0ffee","radius":1.50}}`; f.JSON != want {
		t.Fatalf("json %s, want %s", f.JSON, want)
	}
}

func TestListParsesTOMLWithNestedTables(t *testing.T) {
	dir := t.TempDir()
	write(t, dir, "dawn.toml", `name = "Dawn"
dark = false
updated = 2026-11-16
at = 2026-11-16T08:30:00Z
glow = inf

[tokens]
accent = "#ffaa00"
"panel.radius" = 6

[tokens.chat]
gap = 0.5
`)
	f := only(t, list(t, dir))
	if f.Format != "toml" || f.Error != "" {
		t.Fatalf("%+v", f)
	}
	want := `{"at":"2026-11-16T08:30:00Z","dark":false,"glow":"inf","name":"Dawn","tokens":{"accent":"#ffaa00","chat":{"gap":0.5},"panel.radius":6},"updated":"2026-11-16"}`
	if f.JSON != want {
		t.Fatalf("json\n %s\nwant\n %s", f.JSON, want)
	}
}

func TestListReportsSyntaxErrorsWithTheirLine(t *testing.T) {
	dir := t.TempDir()
	write(t, dir, "a.json", "{\n  \"name\": \"A\",\n  \"tokens\": {\"accent\" \"#fff\"}\n}")
	write(t, dir, "b.json", "[1, 2]")
	write(t, dir, "c.json", "{}\n {}")
	write(t, dir, "d.toml", "name = \"D\"\n[tokens]\naccent = = \"#fff\"\n")
	files := list(t, dir).Files
	if len(files) != 4 {
		t.Fatalf("%+v", files)
	}
	for i, want := range []string{"line 3, column 23: invalid character", "line 1, column 1: a theme is one JSON object", "line 2, column 2: unexpected content", "line 3, column 10:"} {
		f := files[i]
		if f.JSON != "" || !strings.HasPrefix(f.Error, want) {
			t.Errorf("%s: error %q, want prefix %q", f.Name, f.Error, want)
		}
	}
}

func TestListReportsOversizeFileWithoutFailing(t *testing.T) {
	dir := t.TempDir()
	write(t, dir, "big.json", `{"pad":"`+strings.Repeat("x", MaxFileBytes)+`"}`)
	write(t, dir, "ok.json", `{}`)
	files := list(t, dir).Files
	if len(files) != 2 || files[0].Error == "" || files[0].JSON != "" || files[1].Error != "" || files[1].JSON != "{}" {
		t.Fatalf("%+v", files)
	}
}

func TestListSkipsLinksHiddenFoldersAndOtherFiles(t *testing.T) {
	dir, outside := t.TempDir(), t.TempDir()
	write(t, outside, "secret.json", `{"secret":true}`)
	if err := os.Symlink(filepath.Join(outside, "secret.json"), filepath.Join(dir, "linked.json")); err != nil {
		t.Fatal(err)
	}
	write(t, dir, ".hidden.json", `{}`)
	write(t, dir, "notes.txt", `{}`)
	write(t, dir, "bad$name.json", `{}`)
	if err := os.Mkdir(filepath.Join(dir, "folder.json"), 0o700); err != nil {
		t.Fatal(err)
	}
	write(t, dir, "kept.toml", `a = 1`)
	if f := only(t, list(t, dir)); f.Name != "kept.toml" || f.JSON != `{"a":1}` {
		t.Fatalf("%+v", f)
	}
}

func TestListCapsTheFolderByName(t *testing.T) {
	dir := t.TempDir()
	for i := range MaxFiles + 3 {
		write(t, dir, fmt.Sprintf("t%03d.json", i), `{}`)
	}
	files := list(t, dir).Files
	if len(files) != MaxFiles || files[0].Name != "t000.json" || files[MaxFiles-1].Name != fmt.Sprintf("t%03d.json", MaxFiles-1) {
		t.Fatalf("%d files, first %s", len(files), files[0].Name)
	}
}

func TestRevisionChangesOnAddEditAndRemove(t *testing.T) {
	dir := t.TempDir()
	empty := list(t, dir).Revision
	write(t, dir, "a.json", `{"a":1}`)
	added := list(t, dir).Revision
	if added == empty || list(t, dir).Revision != added {
		t.Fatal("revision must change on add and be stable otherwise")
	}
	write(t, dir, "a.json", `{"a":2}`) // same size: the modification time changes it
	later := time.Now().Add(time.Second)
	if err := os.Chtimes(filepath.Join(dir, "a.json"), later, later); err != nil {
		t.Fatal(err)
	}
	edited := list(t, dir).Revision
	if edited == added {
		t.Fatal("revision unchanged after an edit")
	}
	if err := os.Remove(filepath.Join(dir, "a.json")); err != nil {
		t.Fatal(err)
	}
	if list(t, dir).Revision != empty {
		t.Fatal("an emptied folder has the empty revision")
	}
}

func TestCreateWritesANewTOMLTheme(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "state", "themes")
	file, path, err := Create(dir, "My Theme-2", "[tokens]\naccent = \"#123456\"\n")
	if err != nil {
		t.Fatal(err)
	}
	if file != "My Theme-2.toml" || path != filepath.Join(dir, file) || !filepath.IsAbs(path) {
		t.Fatal(file, path)
	}
	info, err := os.Stat(path)
	if err != nil || info.Mode().Perm() != 0o600 {
		t.Fatal(info, err)
	}
	if info, err = os.Stat(dir); err != nil || info.Mode().Perm() != 0o700 {
		t.Fatal(info, err)
	}
	if f := only(t, list(t, dir)); f.JSON != `{"tokens":{"accent":"#123456"}}` {
		t.Fatalf("%+v", f)
	}
}

func TestCreateNeverReplacesAFile(t *testing.T) {
	dir, outside := t.TempDir(), t.TempDir()
	write(t, dir, "dusk.toml", "a = 1\n")
	if _, _, err := Create(dir, "dusk", "a = 2\n"); !errors.Is(err, ErrExists) {
		t.Fatal(err)
	}
	if data, _ := os.ReadFile(filepath.Join(dir, "dusk.toml")); string(data) != "a = 1\n" {
		t.Fatalf("replaced: %q", data)
	}
	// Nor writes through a link to somewhere else.
	if err := os.Symlink(filepath.Join(outside, "target.toml"), filepath.Join(dir, "linked.toml")); err != nil {
		t.Fatal(err)
	}
	if _, _, err := Create(dir, "linked", "a = 2\n"); !errors.Is(err, ErrExists) {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(outside, "target.toml")); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("wrote through the link", err)
	}
}

func TestCreateRefusesBadNamesAndContent(t *testing.T) {
	dir := t.TempDir()
	for _, name := range []string{"", ".hidden", "a/b", "../escape", "tab\there", "é", strings.Repeat("n", MaxNameLength+1)} {
		if _, _, err := Create(dir, name, "a = 1\n"); !errors.Is(err, ErrInvalid) {
			t.Errorf("name %q: %v", name, err)
		}
	}
	if _, _, err := Create(dir, "broken", "a = = 1\n"); !errors.Is(err, ErrInvalid) || !strings.Contains(err.Error(), "line 1") {
		t.Errorf("unparseable content: %v", err)
	}
	if _, _, err := Create(dir, "big", "a = \""+strings.Repeat("x", MaxFileBytes)+"\"\n"); !errors.Is(err, ErrInvalid) {
		t.Errorf("oversize content: %v", err)
	}
	if entries, _ := os.ReadDir(dir); len(entries) != 0 {
		t.Fatalf("a refused theme left %v", entries)
	}
}

func TestCreateRefusesAFullFolder(t *testing.T) {
	dir := t.TempDir()
	for i := range MaxFiles {
		write(t, dir, fmt.Sprintf("t%03d.toml", i), "")
	}
	if _, _, err := Create(dir, "one-more", "a = 1\n"); !errors.Is(err, ErrFull) {
		t.Fatal(err)
	}
}

func TestNormalizeKeepsLocalTimesLocal(t *testing.T) {
	dir := t.TempDir()
	write(t, dir, "times.toml", "dt = 2026-11-16T08:30:00\nt = 07:15:00\nn = nan\nlist = [1, 2]\n[[layers]]\nx = -inf\n")
	want := `{"dt":"2026-11-16T08:30:00","layers":[{"x":"-inf"}],"list":[1,2],"n":"nan","t":"07:15:00"}`
	if f := only(t, list(t, dir)); f.JSON != want {
		t.Fatalf("%s\nwant %s", f.JSON, want)
	}
}
