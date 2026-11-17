package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"time"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/power"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"gopkg.in/yaml.v3"
)

// `lazybox config`: the person's config files (POWER.md, "Config files") —
// keys.yaml, settings.yaml, layouts/*.yaml and actions.yaml — from a terminal.
// It works on the files themselves, with the Runner's own parser, so it needs
// no Runner: one that runs reloads what this writes within a second. The
// folder is the Runner's when one answers, else $XDG_CONFIG_HOME/lazybox.

const configUsage = "usage: lazybox config [path [FILE] | edit FILE | validate [FILE…] | get FILE [KEY] | set FILE KEY VALUE | export [--out F] | import F|-]\n  FILE is keys, settings, actions, or layouts/NAME (also layout NAME)"

// configFolder is where the config files live: the Runner's folder when one
// answers, else the person's configuration directory.
func (c *cli) configFolder() string {
	if t, err := resolveTarget(c.getenv, c.endpoint, c.runner); err == nil {
		ctx, cancel := context.WithTimeout(c.ctx, probeTimeout)
		defer cancel()
		if r, err := dial(t).GetConfigFiles(ctx, connect.NewRequest(&pb.GetConfigFilesRequest{})); err == nil && r.Msg.Directory != "" {
			return r.Msg.Directory
		}
	}
	return configDir(c.getenv)
}

// configFile is one file named on the command line.
type configFile struct{ name, kind, path string }

// resolveConfigFile names a file: keys, settings, actions, layouts/NAME,
// layout NAME (consumed from args), or a path to one of them.
func resolveConfigFile(dir string, args []string) (configFile, []string, error) {
	if len(args) == 0 {
		return configFile{}, nil, fail(exitUsage, "name a file: keys, settings, actions or layouts/NAME")
	}
	name, rest := args[0], args[1:]
	// A path to a file (./keys.yaml, /tmp/review.yaml): its kind is its name's.
	if strings.HasSuffix(name, ".yaml") && (filepath.IsAbs(name) || strings.HasPrefix(name, ".") || (strings.Contains(name, "/") && !strings.HasPrefix(name, "layouts/"))) {
		kind := power.KindLayout
		switch filepath.Base(name) {
		case "keys.yaml":
			kind = power.KindKeys
		case "settings.yaml":
			kind = power.KindSettings
		case "actions.yaml":
			kind = power.KindActions
		}
		return configFile{name, kind, name}, rest, nil
	}
	if name == "layout" && len(rest) > 0 {
		name, rest = "layouts/"+rest[0], rest[1:]
	}
	name = strings.TrimSuffix(name, ".yaml")
	if name == "actions" {
		return configFile{"actions", power.KindActions, filepath.Join(dir, "actions.yaml")}, rest, nil
	}
	kind, path, err := power.NewConfig(dir).Resolve(name)
	if err != nil {
		return configFile{}, nil, fail(exitUsage, "%s", err)
	}
	return configFile{name, kind, path}, rest, nil
}

func (c *cli) config(args []string) error {
	f := c.flags("config")
	out := f.String("out", "", "export: write the bundle to this file instead of stdout")
	withActions := f.Bool("actions", false, "export: include actions.yaml (its webhook headers may be secrets)")
	dryRun := f.Bool("dry-run", false, "import: say what would change, write nothing")
	remove := f.Bool("remove", false, "set: remove the key instead (the default applies)")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	sub := "path"
	if len(positional) > 0 {
		sub, positional = positional[0], positional[1:]
	}
	dir := c.configFolder()
	switch sub {
	case "path":
		if len(positional) == 0 {
			if c.json {
				return c.printJSON(map[string]any{"directory": dir, "keys": filepath.Join(dir, "keys.yaml"), "settings": filepath.Join(dir, "settings.yaml"), "layouts": filepath.Join(dir, "layouts"), "actions": filepath.Join(dir, "actions.yaml")})
			}
			c.out("%s\n", dir)
			return nil
		}
		if positional[0] == "layouts" {
			c.out("%s\n", filepath.Join(dir, "layouts"))
			return nil
		}
		file, _, err := resolveConfigFile(dir, positional)
		if err != nil {
			return err
		}
		c.out("%s\n", file.path)
		return nil
	case "edit":
		return c.configEdit(dir, positional)
	case "validate":
		return c.configValidate(dir, positional)
	case "get":
		return c.configGet(dir, positional)
	case "set":
		return c.configSet(dir, positional, *remove)
	case "export":
		return c.configExport(dir, *out, *withActions)
	case "import":
		return c.configImport(dir, positional, *dryRun)
	}
	return fail(exitUsage, "%s", configUsage)
}

// configEdit opens a file in the person's editor, creating it from its
// starter first, then checks it.
func (c *cli) configEdit(dir string, args []string) error {
	file, rest, err := resolveConfigFile(dir, args)
	if err != nil {
		return err
	}
	if len(rest) > 0 {
		return fail(exitUsage, "usage: lazybox config edit FILE")
	}
	if _, err := os.Stat(file.path); errors.Is(err, os.ErrNotExist) {
		starter := power.Starter(file.kind)
		if file.kind == power.KindActions {
			if _, err := power.NewManager(file.path).Init(); err != nil {
				return err
			}
		} else if err := os.MkdirAll(filepath.Dir(file.path), 0o700); err != nil {
			return err
		} else if err := os.WriteFile(file.path, []byte(starter), 0o600); err != nil {
			return err
		}
		c.note("Created %s\n", file.path)
	}
	if err := power.WriteSchemas(dir); err != nil {
		return err
	}
	editor := c.getenv("VISUAL")
	if editor == "" {
		editor = c.getenv("EDITOR")
	}
	if editor == "" {
		editor = "vi"
	}
	cmd := exec.CommandContext(c.ctx, "/bin/sh", "-c", editor+` "$1"`, "lazybox-config-edit", file.path)
	cmd.Stdin, cmd.Stdout, cmd.Stderr = c.stdin, c.stdout, c.stderr
	if err := cmd.Run(); err != nil {
		return fail(exitFailure, "the editor (%s) failed: %v", editor, err)
	}
	return c.checkFiles([]configFile{file}, false)
}

// configValidate checks every config file (or the ones named), by line.
func (c *cli) configValidate(dir string, args []string) error {
	var files []configFile
	if len(args) == 0 {
		for _, name := range []string{"keys", "settings", "actions"} {
			file, _, _ := resolveConfigFile(dir, []string{name})
			if _, err := os.Stat(file.path); err == nil {
				files = append(files, file)
			}
		}
		entries, _ := os.ReadDir(filepath.Join(dir, "layouts"))
		for _, e := range entries {
			if strings.HasSuffix(e.Name(), ".yaml") && !e.IsDir() {
				files = append(files, configFile{"layouts/" + strings.TrimSuffix(e.Name(), ".yaml"), power.KindLayout, filepath.Join(dir, "layouts", e.Name())})
			}
		}
		if len(files) == 0 {
			c.out("No config files yet in %s. `lazybox config edit keys` creates one.\n", dir)
			return nil
		}
	}
	for len(args) > 0 {
		file, rest, err := resolveConfigFile(dir, args)
		if err != nil {
			return err
		}
		files, args = append(files, file), rest
	}
	return c.checkFiles(files, true)
}

// checkFiles parses each file as the Runner does and prints its problems as
// path:line; it fails (exit 2) when any has an error.
func (c *cli) checkFiles(files []configFile, quietOK bool) error {
	failed := false
	var report []map[string]any
	for _, file := range files {
		data, err := os.ReadFile(file.path)
		if errors.Is(err, os.ErrNotExist) {
			return fail(exitNotFound, "%s does not exist (lazybox config edit %s creates it)", file.path, file.name)
		}
		if err != nil {
			return err
		}
		_, issues, parseErr := power.Check(file.kind, data)
		bad := parseErr != nil
		for _, i := range issues {
			bad = bad || i.Severity == "error"
		}
		failed = failed || bad
		entry := map[string]any{"path": file.path, "kind": file.kind, "valid": !bad, "issues": issues}
		if parseErr != nil {
			entry["error"] = parseErr.Error()
		}
		report = append(report, entry)
		if c.json {
			continue
		}
		if parseErr != nil && len(issues) == 0 {
			c.out("%s: %s\n", file.path, c.paint(red, parseErr.Error()))
		}
		for _, i := range issues {
			colour := yellow
			if i.Severity == "error" {
				colour = red
			}
			c.out("%s:%d: %s %s (%s)\n", file.path, i.Line, c.paint(colour, i.Severity+":"), i.Message, i.Path)
		}
		if !bad && (quietOK || len(issues) == 0) {
			c.out("%s %s\n", file.path, c.paint(green, "✓"))
		}
	}
	if c.json {
		if err := c.printJSON(report); err != nil {
			return err
		}
	}
	if failed {
		return exitError{exitUsage, errors.New("a config file has errors; the Runner keeps using the last version that loaded")}
	}
	return nil
}

// keyPath turns KEY into the path of keys in the file. For keys.yaml an
// action id (palette.open) is keys.<scope>.<id>; otherwise dots separate.
func keyPath(kind, key string) []string {
	if kind == power.KindKeys {
		if a, ok := power.Registry().Action(key); ok {
			return []string{"keys", a.Scope, a.ID}
		}
	}
	return strings.Split(key, ".")
}

func (c *cli) configGet(dir string, args []string) error {
	file, rest, err := resolveConfigFile(dir, args)
	if err != nil {
		return err
	}
	if len(rest) > 1 {
		return fail(exitUsage, "usage: lazybox config get FILE [KEY]")
	}
	data, err := os.ReadFile(file.path)
	if errors.Is(err, os.ErrNotExist) {
		data, err = nil, nil
	}
	if err != nil {
		return err
	}
	value, issues, parseErr := power.Check(file.kind, data)
	if parseErr != nil && !(file.kind == power.KindLayout && len(issues) > 0) {
		return fail(exitUsage, "%s does not load: %s", file.path, parseErr)
	}
	if len(rest) == 0 {
		if c.json {
			return c.printJSON(value)
		}
		if len(data) == 0 {
			c.out("# %s does not exist yet: everything is the default\n", file.path)
			return nil
		}
		_, err := c.stdout.Write(data)
		return err
	}
	key := rest[0]
	// A keys.yaml action: its keys, from the file or the preset.
	if keys, ok := value.(power.Keys); ok {
		if a, found := power.Registry().Action(key); found {
			source, bound := "preset "+keys.Preset, power.Registry().Keymap(keys.Preset)[a.ID]
			for _, b := range keys.Bindings {
				if b.Action == a.ID {
					source, bound = fmt.Sprintf("%s:%d", file.path, b.Line), strings.Join(b.Keys, " or ")
				}
			}
			if c.json {
				return c.printJSON(map[string]any{"action": a.ID, "title": a.Title, "keys": bound, "source": source})
			}
			if bound == "" {
				bound = "(no shortcut)"
			}
			c.out("%s\t%s\n", bound, c.paint(dim, source))
			return nil
		}
	}
	var doc yaml.Node
	if err := yaml.Unmarshal(data, &doc); err != nil || doc.Kind == 0 {
		return fail(exitNotFound, "%s does not set %s (the default applies)", file.path, key)
	}
	node := doc.Content[0]
	for _, part := range keyPath(file.kind, key) {
		next := (*yaml.Node)(nil)
		for i := 0; node.Kind == yaml.MappingNode && i+1 < len(node.Content); i += 2 {
			if node.Content[i].Value == part {
				next = node.Content[i+1]
			}
		}
		if next == nil {
			return fail(exitNotFound, "%s does not set %s (the default applies)", file.path, key)
		}
		node = next
	}
	var v any
	if err := node.Decode(&v); err != nil {
		return err
	}
	if c.json {
		return c.printJSON(v)
	}
	if node.Kind == yaml.ScalarNode {
		c.out("%s\n", node.Value)
		return nil
	}
	enc := yaml.NewEncoder(c.stdout)
	enc.SetIndent(2)
	return errors.Join(enc.Encode(node), enc.Close())
}

func (c *cli) configSet(dir string, args []string, remove bool) error {
	file, rest, err := resolveConfigFile(dir, args)
	if err != nil {
		return err
	}
	if len(rest) != 2 && !(remove && len(rest) == 1) {
		return fail(exitUsage, "usage: lazybox config set FILE KEY VALUE (or KEY --remove)")
	}
	if file.kind == power.KindActions {
		return fail(exitUsage, "actions.yaml is edited as a whole: lazybox config edit actions")
	}
	edit := power.Edit{Path: keyPath(file.kind, rest[0])}
	if !remove {
		// The value is YAML: true, 3, [Mod+k, Ctrl+k], or text ("] ] p" stays text).
		var v any
		// A value YAML reads as nothing but was not written as null ("#7fb0f0",
		// a comment to YAML) is the text typed.
		if err := yaml.Unmarshal([]byte(rest[1]), &v); err != nil || (v == nil && !slices.Contains([]string{"", "null", "~", "Null", "NULL"}, strings.TrimSpace(rest[1]))) {
			v = rest[1]
		}
		data, err := json.Marshal(v)
		if err != nil {
			return fail(exitUsage, "%s cannot be written: %v", rest[1], err)
		}
		edit.JSON = string(data)
	}
	if err := power.EditFile(file.kind, file.path, []power.Edit{edit}); err != nil {
		var refused *power.RefusedError
		if errors.As(err, &refused) {
			// The lines are the change's, not the file's: name the key instead.
			for _, i := range refused.Issues {
				c.out("%s %s: %s\n", c.paint(red, "error:"), i.Path, i.Message)
			}
			return exitError{exitUsage, fmt.Errorf("%s was not changed: %s", filepath.Base(file.path), refused.Reason)}
		}
		return err
	}
	if err := power.WriteSchemas(dir); err != nil {
		return err
	}
	if c.json {
		return c.printJSON(map[string]any{"path": file.path, "key": edit.Path, "removed": remove})
	}
	c.note("Saved %s; Lazybox applies it within a second.\n", file.path)
	return nil
}

// bundle is `lazybox config export`'s output: each file's text, comments and all.
type bundle struct {
	Version int               `yaml:"lazybox-config"`
	Files   map[string]string `yaml:"files"`
}

func (c *cli) configExport(dir, out string, withActions bool) error {
	b := bundle{Version: 1, Files: map[string]string{}}
	names := []string{"keys.yaml", "settings.yaml"}
	if withActions {
		names = append(names, "actions.yaml")
	}
	entries, _ := os.ReadDir(filepath.Join(dir, "layouts"))
	for _, e := range entries {
		if strings.HasSuffix(e.Name(), ".yaml") && !e.IsDir() {
			names = append(names, "layouts/"+e.Name())
		}
	}
	for _, name := range names {
		data, err := os.ReadFile(filepath.Join(dir, filepath.FromSlash(name)))
		if errors.Is(err, os.ErrNotExist) {
			continue
		}
		if err != nil {
			return err
		}
		b.Files[name] = string(data)
	}
	// Literal blocks, so every file reads as itself.
	files := &yaml.Node{Kind: yaml.MappingNode}
	keys := make([]string, 0, len(b.Files))
	for name := range b.Files {
		keys = append(keys, name)
	}
	slices.Sort(keys)
	for _, name := range keys {
		files.Content = append(files.Content, &yaml.Node{Kind: yaml.ScalarNode, Value: name}, &yaml.Node{Kind: yaml.ScalarNode, Tag: "!!str", Style: yaml.LiteralStyle, Value: b.Files[name]})
	}
	doc := &yaml.Node{Kind: yaml.MappingNode, HeadComment: fmt.Sprintf("Lazybox config, exported %s from %s.\nlazybox config import FILE writes it back (each file checked first).", c.now().Format(time.DateOnly), dir), Content: []*yaml.Node{
		{Kind: yaml.ScalarNode, Value: "lazybox-config"}, {Kind: yaml.ScalarNode, Tag: "!!int", Value: "1"},
		{Kind: yaml.ScalarNode, Value: "files"}, files,
	}}
	var buf bytes.Buffer
	enc := yaml.NewEncoder(&buf)
	enc.SetIndent(2)
	if err := errors.Join(enc.Encode(doc), enc.Close()); err != nil {
		return err
	}
	if out == "" {
		_, err := c.stdout.Write(buf.Bytes())
		return err
	}
	if err := os.WriteFile(out, buf.Bytes(), 0o600); err != nil {
		return err
	}
	c.note("Exported %d files to %s\n", len(keys), out)
	return nil
}

func (c *cli) configImport(dir string, args []string, dryRun bool) error {
	if len(args) != 1 {
		return fail(exitUsage, "usage: lazybox config import FILE|- [--dry-run]")
	}
	var data []byte
	var err error
	if args[0] == "-" {
		data, err = io.ReadAll(io.LimitReader(c.stdin, 4<<20))
	} else {
		data, err = os.ReadFile(args[0])
	}
	if err != nil {
		return err
	}
	var b bundle
	if err := yaml.Unmarshal(data, &b); err != nil || b.Version != 1 || len(b.Files) == 0 {
		return fail(exitUsage, "not a Lazybox config bundle (lazybox config export writes one)")
	}
	type plan struct{ name, kind, path, content, change string }
	var plans []plan
	bad := false
	names := make([]string, 0, len(b.Files))
	for name := range b.Files {
		names = append(names, name)
	}
	slices.Sort(names)
	for _, name := range names {
		file, _, err := resolveConfigFile(dir, []string{strings.TrimSuffix(name, ".yaml")})
		if err != nil || filepath.IsAbs(name) || strings.Contains(name, "..") {
			c.out("%s: %s\n", name, c.paint(red, "not a config file name (keys.yaml, settings.yaml, actions.yaml, layouts/NAME.yaml)"))
			bad = true
			continue
		}
		_, issues, parseErr := power.Check(file.kind, []byte(b.Files[name]))
		if parseErr != nil && len(issues) == 0 {
			c.out("%s: %s\n", name, c.paint(red, parseErr.Error()))
			bad = true
		}
		for _, i := range issues {
			if i.Severity == "error" {
				c.out("%s:%d: %s %s (%s)\n", name, i.Line, c.paint(red, "error:"), i.Message, i.Path)
				bad = true
			}
		}
		change := "new"
		if current, err := os.ReadFile(file.path); err == nil {
			change = "replaced"
			if string(current) == b.Files[name] {
				change = "unchanged"
			}
		}
		plans = append(plans, plan{name, file.kind, file.path, b.Files[name], change})
	}
	if bad {
		return exitError{exitUsage, errors.New("nothing was imported: fix the bundle first")}
	}
	for _, p := range plans {
		if p.change == "unchanged" {
			c.out("  %-28s %s\n", p.name, c.paint(dim, "unchanged"))
			continue
		}
		if !dryRun {
			if p.change == "replaced" {
				current, _ := os.ReadFile(p.path)
				if err := os.WriteFile(p.path+".bak", current, 0o600); err != nil {
					return err
				}
			}
			if err := os.MkdirAll(filepath.Dir(p.path), 0o700); err != nil {
				return err
			}
			if err := os.WriteFile(p.path, []byte(p.content), 0o600); err != nil {
				return err
			}
		}
		note := p.change
		if p.change == "replaced" {
			note += " (the previous one is " + filepath.Base(p.path) + ".bak)"
		}
		c.out("  %-28s %s\n", p.name, note)
	}
	if !dryRun {
		if err := power.WriteSchemas(dir); err != nil {
			return err
		}
		c.note("Imported into %s; Lazybox applies it within a second.\n", dir)
	} else {
		c.note("Dry run: nothing was written.\n")
	}
	return nil
}
