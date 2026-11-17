package main

import (
	"errors"
	"os"
	"path/filepath"
	"strconv"
	"strings"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/power"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// `lazybox extension`: sandboxed panel extensions (EXTENSIONS.md) from a
// terminal. new and validate work on the folders themselves, with the
// Runner's own parser, so they need no Runner; list asks the Runner, which
// knows what the person approved. Approving happens in the app, where the
// sheet shows exactly what an extension may read and do.

const extensionUsage = "usage: lazybox extension new <id> [--name N] [--example agent-board|cost-dashboard] | validate [<id>|PATH…] | list | path"

func (c *cli) extension(args []string) error {
	f := c.flags("extension")
	name := f.String("name", "", "new: the extension's name (default: from its id)")
	example := f.String("example", "", "new: start from a bundled example ("+strings.Join(exampleIDs(), ", ")+")")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if c.subcommandHelp(positional, extensionUsage) {
		return nil
	}
	sub, rest := positional[0], positional[1:]
	dir := filepath.Join(c.configFolder(), "extensions")
	switch sub {
	case "path":
		c.out("%s\n", dir)
		return nil
	case "new":
		if len(rest) != 1 {
			return fail(exitUsage, "usage: lazybox extension new <id> [--name N] [--example E]")
		}
		folder, err := power.WriteExtension(dir, rest[0], *name, *example)
		if err != nil {
			var refused *power.ExtensionError
			if errors.As(err, &refused) {
				return fail(exitUsage, "%s", err)
			}
			return err
		}
		if c.json {
			return c.printJSON(map[string]any{"path": folder, "id": rest[0]})
		}
		c.out("Created %s\n", folder)
		c.out("  Edit its files; Lazybox reloads an open panel when you save.\n")
		c.out("  Check it:   lazybox extension validate %s\n", rest[0])
		c.out("  Turn it on: Settings → Extensions, where you approve what it may read and do.\n")
		return nil
	case "validate":
		return c.extensionValidate(dir, rest)
	case "list", "ls":
		return c.extensionList()
	}
	return fail(exitUsage, "%s", extensionUsage)
}

func exampleIDs() []string {
	var ids []string
	for _, x := range power.ExtensionExamples() {
		ids = append(ids, x.ID)
	}
	return ids
}

// extensionValidate checks folders as the Runner reads them, printing each
// problem as path:line; it fails (exit 2) when one does not load.
func (c *cli) extensionValidate(dir string, args []string) error {
	var folders []string
	if len(args) == 0 {
		entries, _ := os.ReadDir(dir)
		for _, e := range entries {
			if e.IsDir() && !strings.HasPrefix(e.Name(), ".") {
				folders = append(folders, filepath.Join(dir, e.Name()))
			}
		}
		if len(folders) == 0 {
			c.out("No extensions yet in %s. `lazybox extension new my-panel` creates one.\n", dir)
			return nil
		}
	}
	for _, arg := range args {
		folder := arg
		if power.ExtensionIDPattern.MatchString(arg) && !strings.Contains(arg, string(filepath.Separator)) {
			if _, err := os.Stat(arg); err != nil {
				folder = filepath.Join(dir, arg)
			}
		}
		if filepath.Base(folder) == power.ExtensionManifestName {
			folder = filepath.Dir(folder)
		}
		abs, err := filepath.Abs(folder)
		if err != nil {
			return err
		}
		if st, err := os.Stat(abs); err != nil || !st.IsDir() {
			return fail(exitNotFound, "%s is not an extension folder", folder)
		}
		folders = append(folders, abs)
	}
	failed := false
	var report []map[string]any
	for _, folder := range folders {
		info := power.InspectExtension(folder)
		manifest := filepath.Join(folder, power.ExtensionManifestName)
		bad := info.State == power.ExtensionBroken
		failed = failed || bad
		report = append(report, map[string]any{"id": info.ID, "path": folder, "valid": !bad, "error": info.Err, "issues": info.Issues, "manifest": info.Manifest})
		if c.json {
			continue
		}
		if info.Err != "" {
			c.out("%s: %s\n", folder, c.paint(red, info.Err))
		}
		for _, i := range info.Issues {
			colour := yellow
			if i.Severity == "error" {
				colour = red
			}
			c.out("%s:%d: %s %s (%s)\n", manifest, i.Line, c.paint(colour, i.Severity+":"), i.Message, i.Path)
		}
		if !bad {
			asks := "nothing"
			if len(info.Manifest.Grants) > 0 {
				asks = strings.Join(info.Manifest.Grants, ", ")
			}
			network := ""
			if len(info.Manifest.Hosts) > 0 {
				network = "; network: " + strings.Join(info.Manifest.Hosts, ", ")
			}
			c.out("%s %s — %d %s; asks for %s%s\n", c.paint(green, "✓"), info.Manifest.Name, len(info.Manifest.Panels), plural(len(info.Manifest.Panels), "panel", "panels"), asks, network)
		}
	}
	if c.json {
		if err := c.printJSON(report); err != nil {
			return err
		}
	}
	if failed {
		return exitError{exitUsage, errors.New("an extension does not load; it stays off until it does")}
	}
	return nil
}

func plural(n int, one, many string) string {
	if n == 1 {
		return one
	}
	return many
}

func (c *cli) extensionList() error {
	cl, err := c.dial()
	if err != nil {
		return err
	}
	r, err := got(cl.ListExtensions(c.ctx, connect.NewRequest(&pb.ListExtensionsRequest{})))
	if err != nil {
		return cl.explain(err)
	}
	if c.json {
		return c.printProtoJSON(r)
	}
	if len(r.Extensions) == 0 {
		c.out("No extensions in %s. `lazybox extension new my-panel` creates one", r.Directory)
		if len(r.Examples) > 0 {
			c.out(", or `--example %s`", r.Examples[0].Id)
		}
		c.out(".\n")
		return nil
	}
	states := map[string]string{power.ExtensionEnabled: green, power.ExtensionReview: yellow, power.ExtensionNew: yellow, power.ExtensionBroken: red, power.ExtensionDisabled: dim}
	labels := map[string]string{power.ExtensionEnabled: "on", power.ExtensionReview: "on · asks for more", power.ExtensionNew: "needs approval", power.ExtensionBroken: "does not load", power.ExtensionDisabled: "off"}
	for _, e := range r.Extensions {
		name := e.Name
		if name == "" {
			name = e.Id
		}
		c.out("%-20s %-22s %s\n", e.Id, c.paint(states[e.State], labels[e.State]), name)
		if e.State == power.ExtensionBroken {
			problem := e.Error
			for _, i := range e.Issues {
				if problem == "" && i.Severity == "error" {
					problem = "line " + strconv.Itoa(int(i.Line)) + ": " + i.Message
				}
			}
			c.out("%-20s %s\n", "", c.paint(dim, problem))
		}
	}
	return nil
}
