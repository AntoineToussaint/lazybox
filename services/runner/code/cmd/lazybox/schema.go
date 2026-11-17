package main

import (
	"os"
	"path/filepath"
	"sort"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/power"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/process"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/workspaceruntime"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/themes"
)

// The JSON Schemas of the files a person writes, shipped with the CLI. They
// are identified by URNs, not web addresses: an editor reads them from a
// file (`lazybox schema --write DIR`), so nothing is fetched and nothing
// depends on a site being up.
type schemaFile struct {
	name, file, what, use string
	data                  func() []byte
}

var schemas = []schemaFile{
	{"actions", power.SchemaName, "~/.config/lazybox/actions.yaml: your actions and hooks", "written beside actions.yaml by `lazybox actions init`", func() []byte { return []byte(power.Schema) }},
	{"process", process.SchemaFile, ".lazybox/process.yaml: how a mission runs", "process files name it as `# yaml-language-server: $schema=process.schema.json`; write it beside them", process.Schema},
	{"workspace", "workspace.schema.json", ".lazybox/workspace.yaml: a workspace's scripts and services", "`# yaml-language-server: $schema=workspace.schema.json` beside the file", func() []byte { return workspaceruntime.Schema }},
	{"keys", power.SchemaFile(power.KindKeys), "~/.config/lazybox/keys.yaml: your shortcuts, ]] menu and Vim editing", "written beside keys.yaml by the app and `lazybox config`", func() []byte { return []byte(power.SchemaFor(power.KindKeys)) }},
	{"settings", power.SchemaFile(power.KindSettings), "~/.config/lazybox/settings.yaml: notifications and sounds", "written beside settings.yaml by the app and `lazybox config`", func() []byte { return []byte(power.SchemaFor(power.KindSettings)) }},
	{"layout", power.SchemaFile(power.KindLayout), "~/.config/lazybox/layouts/*.yaml: one whole-window layout each", "written beside layouts/ by the app and `lazybox config`", func() []byte { return []byte(power.SchemaFor(power.KindLayout)) }},
	{"theme", "theme.schema.json", "a theme in the Runner's themes folder (Settings → Appearance)", "`\"$schema\": \"./theme.schema.json\"` in JSON, `#:schema ./theme.schema.json` in TOML", func() []byte { return themes.Schema }},
}

func findSchema(name string) *schemaFile {
	for i := range schemas {
		if schemas[i].name == name {
			return &schemas[i]
		}
	}
	return nil
}

func (c *cli) schema(args []string) error {
	f := c.flags("schema")
	write := f.String("write", "", "write the schema (every schema without a name) into this directory instead of printing it")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) > 1 {
		return fail(exitUsage, "lazybox schema [NAME] [--write DIR]")
	}
	selected := schemas
	if len(positional) == 1 {
		s := findSchema(positional[0])
		if s == nil {
			names := make([]string, len(schemas))
			for i, s := range schemas {
				names[i] = s.name
			}
			sort.Strings(names)
			return fail(exitNotFound, "no schema named %q; there are: %v", positional[0], names)
		}
		selected = []schemaFile{*s}
	}
	if *write != "" {
		if err = os.MkdirAll(*write, 0o755); err != nil {
			return err
		}
		written := map[string]string{}
		for _, s := range selected {
			path := filepath.Join(*write, s.file)
			if err = os.WriteFile(path, s.data(), 0o644); err != nil {
				return err
			}
			written[s.name] = path
		}
		if c.json {
			return c.printJSON(written)
		}
		for _, s := range selected {
			c.out("%s %s → %s\n  %s\n", c.paint(green, "✓"), s.name, written[s.name], c.paint(dim, s.use))
		}
		return nil
	}
	if len(positional) == 1 {
		_, err = c.stdout.Write(selected[0].data())
		return err
	}
	if c.json {
		out := []map[string]string{}
		for _, s := range schemas {
			out = append(out, map[string]string{"name": s.name, "file": s.file, "describes": s.what})
		}
		return c.printJSON(out)
	}
	for _, s := range schemas {
		c.out("  %-10s %s\n", s.name, s.what)
	}
	c.out("\n`lazybox schema NAME` prints one; `lazybox schema [NAME] --write DIR` writes it for your editor.\n")
	return nil
}
