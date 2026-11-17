// Package docstool is the help assistant's read-only view of the Lazybox
// documentation: the same pages Help → Documentation shows, read from the
// docs folder the Runner was given (an installed release ships one; Dev and
// Working name the checkout's). It lists, searches and reads Markdown pages
// inside that folder only; docs/internal is for people working on Lazybox and
// is never served.
package docstool

import (
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"unicode"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// Page is one documentation page: its path inside the docs folder and title.
type Page struct {
	Path  string `json:"path"`
	Title string `json:"title"`
}

// Hit is a page that matches a search, with the lines that matched.
type Hit struct {
	Path    string   `json:"path"`
	Title   string   `json:"title"`
	Excerpt []string `json:"excerpt"`
}

const (
	pageLimit = 256 << 10
	readLimit = 48 << 10
	hitLimit  = 6
)

// ErrUnavailable means this Runner was given no documentation.
var ErrUnavailable = errors.New("the Lazybox documentation is not available on this Runner; point the person to Help → Documentation in the app")

// Pages lists every page, in path order.
func Pages(dir string) ([]Page, error) {
	if dir == "" {
		return nil, ErrUnavailable
	}
	var pages []Page
	err := filepath.WalkDir(dir, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		rel, _ := filepath.Rel(dir, path)
		if d.IsDir() {
			if rel == "internal" || (rel != "." && strings.HasPrefix(d.Name(), ".")) {
				return filepath.SkipDir
			}
			return nil
		}
		if !d.Type().IsRegular() || filepath.Ext(path) != ".md" {
			return nil
		}
		text, err := readPage(path)
		if err != nil {
			return nil
		}
		pages = append(pages, Page{Path: filepath.ToSlash(rel), Title: title(rel, text)})
		return nil
	})
	if errors.Is(err, fs.ErrNotExist) {
		return nil, ErrUnavailable
	}
	sort.Slice(pages, func(i, j int) bool { return pages[i].Path < pages[j].Path })
	return pages, err
}

// Read returns one page's text, bounded. path is as Pages lists it.
func Read(dir, path string) (string, error) {
	if dir == "" {
		return "", ErrUnavailable
	}
	clean := filepath.Clean(filepath.FromSlash(strings.TrimPrefix(strings.TrimSpace(path), "docs/")))
	if clean == "." || filepath.IsAbs(clean) || clean == ".." || strings.HasPrefix(clean, ".."+string(filepath.Separator)) || filepath.Ext(clean) != ".md" || strings.HasPrefix(filepath.ToSlash(clean), "internal/") {
		return "", fmt.Errorf("%q is not a documentation page; use a path lazybox_search_docs or the page list gives, such as guides/chat.md", path)
	}
	full := filepath.Join(dir, clean)
	// A link inside the docs folder must not lead out of it.
	resolved, err := filepath.EvalSymlinks(full)
	if err != nil {
		return "", fmt.Errorf("no documentation page %q; search for the topic with lazybox_search_docs", path)
	}
	root, err := filepath.EvalSymlinks(dir)
	if err != nil {
		return "", ErrUnavailable
	}
	if rel, err := filepath.Rel(root, resolved); err != nil || strings.HasPrefix(rel, "..") {
		return "", fmt.Errorf("%q is not a documentation page", path)
	}
	text, err := readPage(resolved)
	if err != nil {
		return "", err
	}
	if len(text) > readLimit {
		text = text[:readLimit] + "\n\n[The rest of this page was left out: it is longer than the reader shows.]"
	}
	return text, nil
}

// Search finds the pages that best match query: every word of the query
// counts, a word in a title or heading most. It returns at most a few pages,
// each with the lines that matched.
func Search(dir, query string) ([]Hit, error) {
	terms := words(query)
	if len(terms) == 0 {
		return nil, errors.New("query required: a few words about the topic, such as \"switch subscription limit\"")
	}
	pages, err := Pages(dir)
	if err != nil {
		return nil, err
	}
	type scored struct {
		hit   Hit
		score int
	}
	var found []scored
	for _, page := range pages {
		text, err := readPage(filepath.Join(dir, filepath.FromSlash(page.Path)))
		if err != nil {
			continue
		}
		score, matched := 0, map[string]bool{}
		var excerpt []string
		for _, line := range strings.Split(text, "\n") {
			lower := strings.ToLower(line)
			hits := 0
			for _, term := range terms {
				if strings.Contains(lower, term) {
					hits++
					matched[term] = true
				}
			}
			if hits == 0 {
				continue
			}
			weight := 1
			if strings.HasPrefix(strings.TrimSpace(line), "#") {
				weight = 4
			}
			score += hits * weight
			if len(excerpt) < 4 && strings.TrimSpace(line) != "" {
				excerpt = append(excerpt, clip(strings.TrimSpace(line), 240))
			}
		}
		for _, term := range terms {
			if strings.Contains(strings.ToLower(page.Title+" "+page.Path), term) {
				score += 6
				matched[term] = true
			}
		}
		if score == 0 {
			continue
		}
		// Pages that match more of the query's words come first.
		score += len(matched) * 20
		found = append(found, scored{Hit{Path: page.Path, Title: page.Title, Excerpt: excerpt}, score})
	}
	sort.SliceStable(found, func(i, j int) bool { return found[i].score > found[j].score })
	hits := []Hit{}
	for i := 0; i < len(found) && i < hitLimit; i++ {
		hits = append(hits, found[i].hit)
	}
	return hits, nil
}

// Handle runs one documentation tool call and returns its JSON result.
func Handle(dir, name string, args json.RawMessage) (string, error) {
	var p struct {
		Query string `json:"query"`
		Path  string `json:"path"`
	}
	if len(args) > 0 && string(args) != "null" {
		if err := json.Unmarshal(args, &p); err != nil {
			return "", fmt.Errorf("invalid arguments: %v", err)
		}
	}
	var value any
	switch name {
	case agent.DocsSearchTool:
		hits, err := Search(dir, p.Query)
		if err != nil {
			return "", err
		}
		value = map[string]any{"pages": hits, "note": "Read a page with lazybox_read_doc before answering from it. People open these pages from Help → Documentation."}
	case agent.DocsReadTool:
		text, err := Read(dir, p.Path)
		if err != nil {
			return "", err
		}
		return text, nil
	default:
		return "", agent.ErrUnsupported
	}
	data, err := json.Marshal(value)
	return string(data), err
}

// Index is the page list for an assistant's briefing: one "path — title"
// line each, or "" without documentation.
func Index(dir string) string {
	pages, err := Pages(dir)
	if err != nil || len(pages) == 0 {
		return ""
	}
	var b strings.Builder
	for _, page := range pages {
		fmt.Fprintf(&b, "- %s — %s\n", page.Path, page.Title)
	}
	return b.String()
}

func readPage(path string) (string, error) {
	info, err := os.Stat(path)
	if err != nil {
		return "", err
	}
	if !info.Mode().IsRegular() || info.Size() > pageLimit {
		return "", fmt.Errorf("%s is not a readable page", filepath.Base(path))
	}
	data, err := os.ReadFile(path)
	return string(data), err
}

// title is a page's first heading, else its file name.
func title(path, text string) string {
	for _, line := range strings.Split(text, "\n") {
		if strings.HasPrefix(line, "# ") {
			return strings.TrimSpace(line[2:])
		}
	}
	return strings.TrimSuffix(filepath.Base(path), ".md")
}

// stopWords say nothing about a topic ("how do I …").
var stopWords = map[string]bool{"a": true, "an": true, "and": true, "are": true, "at": true, "be": true, "can": true, "do": true, "does": true, "for": true, "how": true, "i": true, "in": true, "is": true, "it": true, "lazybox": true, "me": true, "my": true, "of": true, "on": true, "or": true, "the": true, "this": true, "that": true, "to": true, "what": true, "when": true, "where": true, "why": true, "with": true, "you": true}

// words are the query's search terms: lowercased words of two letters or more.
func words(query string) []string {
	seen := map[string]bool{}
	var terms []string
	for _, word := range strings.FieldsFunc(strings.ToLower(query), func(r rune) bool { return !unicode.IsLetter(r) && !unicode.IsDigit(r) && r != '-' && r != '.' }) {
		if len([]rune(word)) < 2 || stopWords[word] || seen[word] || len(terms) >= 12 {
			continue
		}
		seen[word] = true
		terms = append(terms, word)
	}
	return terms
}

func clip(text string, n int) string {
	runes := []rune(text)
	if len(runes) <= n {
		return text
	}
	return string(runes[:n]) + "…"
}
