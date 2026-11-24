package github

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"strings"

	sdk "github.com/google/go-github/v93/github"
)

// GitHub serves lists a page (at most 100 entries) at a time. Every list
// Lazybox decides on is read to its end, or as far as MaxPages; a read
// that stops there says so, and is never taken for the whole list.

// MaxPages bounds one list read: 1000 check runs, commit statuses,
// reviews, review comments or branch pull requests.
const MaxPages = 10

const perPage = 100

// MaxListed is the most entries one list read takes.
const MaxListed = MaxPages * perPage

// getPage reads one page of path into out, conditionally when etag is set.
// next is the following page GitHub's Link header names, 0 for none.
func getPage(ctx context.Context, client *sdk.Client, path, etag string, out any) (newETag string, notModified bool, next int, err error) {
	req, err := client.NewRequest(ctx, http.MethodGet, path, nil)
	if err != nil {
		return "", false, 0, errors.New("invalid GitHub request")
	}
	if etag != "" {
		req.Header.Set("If-None-Match", etag)
	}
	response, err := client.Do(req, out)
	if response != nil && response.Response != nil && response.StatusCode == http.StatusNotModified {
		return etag, true, 0, nil
	}
	if err != nil {
		return "", false, 0, apiError(err, response)
	}
	return response.Header.Get("ETag"), false, response.NextPage, nil
}

// pagedRead is every page of a list, as getPages read it.
type pagedRead[T any] struct {
	Pages []T
	// Tags are the pages' validators, space-separated (an entity tag holds
	// no space); empty when the read is incomplete, so the next read is
	// complete or says again that it is not.
	Tags string
	// Same: every page was unchanged (304) and Pages is empty.
	Same bool
	// Complete: the read reached the list's last page.
	Complete bool
}

// getPages reads every page of the list at path (whose query sets per_page),
// conditionally on tags, the validators of the previous read's pages. Only
// lists whose first page changes when the list grows or shrinks qualify
// (a total_count, or a newest-first order): then every page unchanged means
// the list is. A page that changed after unchanged ones were skipped makes
// the read start again from the first page, unconditionally.
func getPages[T any](ctx context.Context, client *sdk.Client, path, tags string) (pagedRead[T], error) {
	previous := strings.Fields(tags)
	conditional := len(previous) > 0 && len(previous) <= MaxPages
	var pages []T
	var seen []string
	for page := 1; ; {
		tag := ""
		if conditional && page <= len(previous) {
			tag = previous[page-1]
		}
		var body T
		etag, notModified, next, err := getPage(ctx, client, withPage(path, page), tag, &body)
		if err != nil {
			return pagedRead[T]{}, err
		}
		if notModified {
			seen = append(seen, etag)
			if page == len(previous) {
				return pagedRead[T]{Tags: strings.Join(seen, " "), Same: true, Complete: true}, nil
			}
			page++
			continue
		}
		if conditional && page > 1 {
			// The pages before this one were skipped as unchanged: read them.
			conditional, pages, seen, page = false, nil, nil, 1
			continue
		}
		conditional = false
		pages, seen = append(pages, body), append(seen, etag)
		if next == 0 {
			return pagedRead[T]{Pages: pages, Tags: strings.Join(seen, " "), Complete: true}, nil
		}
		if len(pages) >= MaxPages {
			return pagedRead[T]{Pages: pages}, nil
		}
		page = next
	}
}

func withPage(path string, page int) string {
	separator := "?"
	if strings.Contains(path, "?") {
		separator = "&"
	}
	return fmt.Sprintf("%s%spage=%d", path, separator, page)
}

// listNewest reads an oldest-first list from its newest end: all of it when
// it has at most MaxListed entries, else its newest MaxListed, and then
// complete is false.
func listNewest[T any](read func(page int) ([]T, *sdk.Response, error)) (out []T, complete bool, err error) {
	first, response, err := read(1)
	if err != nil {
		return nil, false, err
	}
	if response == nil || response.NextPage == 0 {
		return first, true, nil
	}
	out, page, complete := first, response.NextPage, true
	// The last page may be short: the newest MaxListed entries can start a
	// page earlier than the last MaxPages pages.
	if start := response.LastPage - MaxPages; start > 1 {
		out, page, complete = nil, start, false
	}
	for reads := 1; ; reads++ {
		items, response, err := read(page)
		if err != nil {
			return nil, false, err
		}
		out = append(out, items...)
		if response == nil || response.NextPage == 0 {
			break
		}
		// The list grew while it was read, past what one read takes.
		if reads > MaxPages {
			complete = false
			break
		}
		page = response.NextPage
	}
	if len(out) > MaxListed {
		out, complete = out[len(out)-MaxListed:], false
	}
	return out, complete, nil
}
