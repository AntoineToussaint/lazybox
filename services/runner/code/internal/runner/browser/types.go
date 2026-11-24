package browser

import "time"

// PageState is what the page shows, as the service reports it.
type PageState struct {
	URL            string `json:"url"`
	Title          string `json:"title"`
	Loading        bool   `json:"loading"`
	CanGoBack      bool   `json:"canGoBack"`
	CanGoForward   bool   `json:"canGoForward"`
	ViewportWidth  int    `json:"viewportWidth"`
	ViewportHeight int    `json:"viewportHeight"`
	// Origin is the site the agent may act on without asking: one the person
	// allowed, or went to themselves. Empty when there is none.
	Origin string `json:"origin,omitempty"`
}

// Box is where an action happened, in the page's CSS pixels.
type Box struct {
	X      float64 `json:"x"`
	Y      float64 `json:"y"`
	Width  float64 `json:"width"`
	Height float64 `json:"height"`
}

// State is one conversation's browser as the person sees it.
type State struct {
	Conversation, Session, Workspace string
	// Status: closed | starting | open | stopped | failed.
	Status string
	// Controller: agent | person.
	Controller                       string
	URL, Title                       string
	Loading, CanGoBack, CanGoForward bool
	BrowseFreely                     bool
	Problem                          string
	Current                          *Action
	ViewportWidth, ViewportHeight    int
	Updated                          time.Time
	Agent                            string
	Waiting                          bool
}

// Action is one line of the browser's action log.
type Action struct {
	Seq uint64
	// Kind: open, navigate, snapshot, click, type, select, press, scroll,
	// screenshot, back, forward, wait, close, blocked, download, popup,
	// take_over, give_back, stop, site.
	Kind string
	// Actor: agent | person | lazybox.
	Actor, Summary, URL string
	Box                 *Box
	// Status: running | done | failed | blocked.
	Status, Error string
	At            time.Time
	Duration      time.Duration
}

// Frame is one screencast frame.
type Frame struct {
	JPEG          []byte
	Width, Height int
	Seq           uint64
}

// Event is what a watcher receives.
type Event struct {
	State   *State
	Frame   *Frame
	Action  *Action
	History []Action
}

// Request is a site, download or sign-in waiting for the person.
type Request struct {
	ID, Session, Conversation, Workspace string
	// Kind: site | download | sign_in.
	Kind, URL, Origin, Reason, FileName string
	Permission, Agent                   string
	Created                             time.Time
}
