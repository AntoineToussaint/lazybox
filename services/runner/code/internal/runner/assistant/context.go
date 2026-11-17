// Package assistant owns the product context supplied to app-wide conversations.
package assistant

import (
	_ "embed"
	"strings"
)

//go:embed context.md
var Context string

// Settings tells the help assistant what its settings tools can change and how
// a change reaches the user. Only the help assistant has those tools.
//
//go:embed settings.md
var Settings string

//go:embed docs.md
var docs string

// Docs tells the help assistant how to answer from the documentation, with
// the page list (one "path — title" line each) when the Runner has the docs.
func Docs(index string) string {
	if strings.TrimSpace(index) == "" {
		return docs + "- The documentation is not available on this Runner, so the documentation tools will say so. Answer from the supplied context and point the person to Help → Documentation in the app.\n"
	}
	return docs + "- Pages (path — title):\n" + index
}
