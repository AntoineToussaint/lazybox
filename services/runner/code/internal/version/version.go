// Package version is the one version string a Lazybox release carries: the
// Runner, the `lazybox` CLI and the desktop app are built from the same tag
// and report the same value (Settings → About, `lazybox --version`, the
// Runner's Version RPC).
//
// A release build sets it with the linker:
//
//	go build -ldflags "-X github.com/AntoineToussaint/lazybox/services/runner/code/internal/version.Version=0.1.0"
//
// Anything else (Dev, Working, tests, `go run`) reports "dev".
package version

// Version is the release this binary was built as, or "dev".
var Version = "dev"

// Released reports whether this binary was built as a numbered release.
func Released() bool { return Version != "" && Version != "dev" }

// String is Version, never empty.
func String() string {
	if Version == "" {
		return "dev"
	}
	return Version
}

// Product names this Lazybox in `lazybox --version`, so an agent (or a
// person) can tell this CLI from another lazybox on PATH, such as V1's.
const Product = "Lazybox V2"

// Line is what `lazybox --version` prints: "lazybox 0.2.0 (Lazybox V2)".
func Line() string { return "lazybox " + String() + " (" + Product + ")" }
