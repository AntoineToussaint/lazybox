package power

import (
	"fmt"
	"regexp"
	"slices"
	"strconv"
	"strings"

	"gopkg.in/yaml.v3"
)

// settings.yaml: the app's preferences that are safe to keep in a file
// (POWER.md, "Config files"). Only what holds no secret and needs no
// approval lives here: credentials, subscriptions, devices, the Runner's own
// settings and anything owner-only stay in the app.
//
//	version: 1
//	notifications:
//	  needs-input: true
//	  failed: true
//	  review: false
//	  dock-badge: true
//	  sound-needs-you: true
//	  sound-done: false
//	appearance:
//	  mode: system
//	  dark-theme: dark
//	  accent: "#7fb0f0"
//	  density: compact
//	  chat-line-height: 1.4
//
// What the file leaves out is the default.

// NotificationFields maps the file's names to the app's (NotificationPreferences).
var NotificationFields = map[string]string{
	"needs-input":     "needsInput",
	"failed":          "failed",
	"review":          "review",
	"reminders":       "reminders",
	"dock-badge":      "badge",
	"sound-needs-you": "soundNeedsYou",
	"sound-done":      "soundDone",
}

// notificationOrder is the order the schema and the writer use.
var notificationOrder = []string{"needs-input", "failed", "review", "reminders", "dock-badge", "sound-needs-you", "sound-done"}

// appearanceField is one setting of the appearance section: its name in the
// app (AppearancePreferences, or its typography), what it holds, and how the
// app bounds it (packages/surfaces/src/appearance/preferences.ts).
type appearanceField struct {
	Name, App, Kind, Description string
	Enum                         []string
	Min, Max                     float64
}

// Appearance field kinds.
const (
	fieldEnum   = "enum"
	fieldNumber = "number"
	fieldBool   = "bool"
	fieldFont   = "font"
	fieldColor  = "color"
	fieldTheme  = "theme"
)

// AppearanceFields is the appearance section, in the order the schema, the
// starter and the app use. Colour overrides and the cached user themes stay
// on the device: they are not preferences a person types.
var AppearanceFields = []appearanceField{
	{Name: "mode", App: "mode", Kind: fieldEnum, Enum: []string{"system", "light", "dark"}, Description: "system follows the computer's light or dark appearance"},
	{Name: "dark-theme", App: "darkTheme", Kind: fieldTheme, Description: "a built-in dark theme ID or user:<file name> for one of your themes"},
	{Name: "light-theme", App: "lightTheme", Kind: fieldTheme, Description: "a built-in light theme ID or user:<file name> for one of your themes"},
	{Name: "accent", App: "accent", Kind: fieldColor, Description: "buttons, selection, focus and links: #rgb, #rrggbb, #rrggbbaa, rgb() or hsl(); leave out for the theme's"},
	{Name: "min-contrast", App: "minContrast", Kind: fieldNumber, Min: 0, Max: 21, Description: "0 (off), 3, 4.5 (WCAG AA, the default) or 7"},
	{Name: "motion", App: "motion", Kind: fieldEnum, Enum: []string{"system", "reduce", "full"}, Description: "reduce turns off animations and smooth scrolling"},
	{Name: "density", App: "density", Kind: fieldEnum, Enum: []string{"compact", "comfortable", "relaxed"}, Description: "row heights in lists; also the chat's line and paragraph spacing"},
	{Name: "interface-font", App: "uiFont", Kind: fieldFont, Description: `a font list such as "Inter, sans-serif"; "" for the system font`},
	{Name: "interface-font-size", App: "uiFontSize", Kind: fieldNumber, Min: 11, Max: 16, Description: "px"},
	{Name: "code-font", App: "codeFont", Kind: fieldFont, Description: `the editor, code blocks, diffs and command output; "" for the system monospace`},
	{Name: "code-font-size", App: "codeFontSize", Kind: fieldNumber, Min: 10, Max: 20, Description: "px"},
	{Name: "ligatures", App: "ligatures", Kind: fieldBool, Description: "join => and != when the code font has them"},
	{Name: "chat-font-size", App: "chatFontSize", Kind: fieldNumber, Min: 11, Max: 22, Description: "px"},
	{Name: "chat-line-height", App: "chatLineHeight", Kind: fieldNumber, Min: 1.1, Max: 2.2, Description: "compact 1.35, comfortable 1.5, relaxed 1.7"},
	{Name: "chat-paragraph-gap", App: "chatParagraphGap", Kind: fieldNumber, Min: 0, Max: 32, Description: "px"},
	{Name: "chat-line-width", App: "chatMaxWidth", Kind: fieldNumber, Min: 0, Max: 200, Description: "characters per line, centred; 0 for the full width"},
	{Name: "zoom", App: "zoom", Kind: fieldNumber, Min: 0.5, Max: 2, Description: "the desktop app's zoom, every window (1 is 100 %); a browser uses its own zoom"},
}

// builtinThemes are the app's themes and their appearance (appearance/themes.ts).
var builtinThemes = map[string]string{
	"dark": "dark", "dark-high-contrast": "dark",
	"dracula": "dark", "one-dark-pro": "dark", "tokyo-night": "dark", "catppuccin-mocha": "dark", "github-dark-default": "dark",
	"catppuccin-latte": "light", "github-light-default": "light",
	"light": "light", "solarized-light": "light",
	"light-high-contrast": "light",
}

var retiredThemes = map[string]string{
	"midnight-blue": "dracula",
	"pine":          "dracula",
	"ember":         "dracula",
	"aubergine":     "dracula",
	"graphite":      "dracula",
	"deep-ocean":    "dracula",
	"rosewood":      "dracula",
	"ink":           "dracula",
	"paper":         "github-light-default",
	"glacier":       "github-light-default",
	"sage":          "github-light-default",
	"rose-quartz":   "github-light-default",
	"lavender-mist": "github-light-default",
	"sand":          "github-light-default",
	"sea-glass":     "github-light-default",
}

var (
	fontPattern  = regexp.MustCompile(`^[\w\s,'"\-.]{0,200}$`)
	themePattern = regexp.MustCompile(`^(?:user:[^\x00-\x1f/\\]{1,100}|[a-z0-9][a-z0-9-]{0,63})$`)
	hexColor     = regexp.MustCompile(`^#(?:[0-9a-fA-F]{3}|[0-9a-fA-F]{4}|[0-9a-fA-F]{6}|[0-9a-fA-F]{8})$`)
	funcColor    = regexp.MustCompile(`^(?i:rgba?|hsla?)\(\s*[0-9.%deg\s,/+-]+\)$`)
)

// Settings is what settings.yaml declares: only the values it sets.
type Settings struct {
	Notifications map[string]bool `json:"notifications"`
	// Appearance holds the app's names (AppearanceFields' App) and values.
	Appearance map[string]any `json:"appearance"`
}

// ParseSettings reads settings.yaml.
func ParseSettings(data []byte) (Settings, []Issue, error) {
	out := Settings{Notifications: map[string]bool{}, Appearance: map[string]any{}}
	root, err := parseRoot(data, "the file must be a mapping with version, notifications and appearance")
	if err != nil || root == nil {
		return out, nil, err
	}
	p := &parser{}
	sections := []string{"version", "notifications", "appearance"}
	for key, value := range pairs(root) {
		switch key.Value {
		case "version":
			if value.Value != "1" {
				p.fail(value, "version", "only version 1 exists")
			}
		case "notifications":
			if value.Tag == "!!null" {
				continue
			}
			if value.Kind != yaml.MappingNode {
				p.fail(value, "notifications", "notifications is a mapping of %v to true or false", notificationOrder)
				continue
			}
			for field, v := range pairs(value) {
				at := "notifications." + field.Value
				name, ok := NotificationFields[field.Value]
				if !ok {
					p.fail(field, at, "unknown setting %q%s", field.Value, didYouMean(field.Value, notificationOrder))
					continue
				}
				p.failed = false
				on := p.boolean(v, at)
				if !p.failed {
					out.Notifications[name] = on
				}
			}
		case "appearance":
			if value.Tag == "!!null" {
				continue
			}
			if value.Kind != yaml.MappingNode {
				p.fail(value, "appearance", "appearance is a mapping such as {mode: dark, accent: \"#7fb0f0\"}")
				continue
			}
			p.appearance(value, out.Appearance)
		default:
			p.warn(key, key.Value, "unknown section %q is ignored (expected version, notifications, appearance)%s", key.Value, didYouMean(key.Value, sections))
		}
	}
	return out, p.issues, nil
}

// appearance reads the appearance section into out; a value with a mistake
// is left out (the app keeps its default) and reported by line.
func (p *parser) appearance(n *yaml.Node, out map[string]any) {
	names := make([]string, len(AppearanceFields))
	for i, f := range AppearanceFields {
		names[i] = f.Name
	}
	for key, v := range pairs(n) {
		at := "appearance." + key.Value
		i := slices.IndexFunc(AppearanceFields, func(f appearanceField) bool { return f.Name == key.Value })
		if i < 0 {
			p.fail(key, at, "unknown setting %q%s", key.Value, didYouMean(key.Value, names))
			continue
		}
		f := AppearanceFields[i]
		if v.Kind != yaml.ScalarNode || (v.Tag == "!!null" && f.Kind != fieldColor) {
			p.fail(v, at, "must be a single value (%s)", f.Description)
			continue
		}
		if v.Tag == "!!null" {
			continue // accent: ~ is the theme's accent.
		}
		switch f.Kind {
		case fieldEnum:
			if p.oneOf(v, at, v.Value, f.Enum) {
				out[f.App] = v.Value
			}
		case fieldBool:
			p.failed = false
			on := p.boolean(v, at)
			if !p.failed {
				out[f.App] = on
			}
		case fieldNumber:
			number, err := strconv.ParseFloat(v.Value, 64)
			if (v.Tag != "!!int" && v.Tag != "!!float") || err != nil {
				p.fail(v, at, "must be a number from %s to %s", trimNumber(f.Min), trimNumber(f.Max))
				continue
			}
			if number < f.Min || number > f.Max {
				p.fail(v, at, "%s is outside %s to %s", v.Value, trimNumber(f.Min), trimNumber(f.Max))
				continue
			}
			out[f.App] = number
		case fieldFont:
			if !fontPattern.MatchString(v.Value) || strings.ContainsAny(v.Value, ";{}()<>\\") {
				p.fail(v, at, "must be a font list such as \"Inter, sans-serif\": names, quotes, commas and spaces only")
				continue
			}
			out[f.App] = strings.TrimSpace(v.Value)
		case fieldColor:
			color := strings.TrimSpace(v.Value)
			if !hexColor.MatchString(color) && !funcColor.MatchString(color) {
				p.fail(v, at, "%q is not a colour: #rgb, #rrggbb, #rrggbbaa, rgb() or hsl() (quote a #colour, or YAML reads it as a comment)", v.Value)
				continue
			}
			out[f.App] = color
		case fieldTheme:
			want := strings.TrimSuffix(f.Name, "-theme")
			if !themePattern.MatchString(v.Value) {
				p.fail(v, at, "%q is not a theme: a built-in one (%s) or user:<file name> for one of your themes", v.Value, themesOf(want))
				continue
			}
			id := v.Value
			if replacement, retired := retiredThemes[id]; retired {
				id = replacement
			}
			if scheme, builtin := builtinThemes[id]; builtin && scheme != want {
				p.fail(v, at, "%q is a %s theme; %s takes a %s one (%s)", v.Value, scheme, f.Name, want, themesOf(want))
				continue
			}
			if _, builtin := builtinThemes[id]; !builtin && !strings.HasPrefix(v.Value, "user:") {
				p.fail(v, at, "there is no built-in theme %q (%s; your own themes are user:<file name>)%s", v.Value, themesOf(want), didYouMean(v.Value, sortedKeys(builtinThemes)))
				continue
			}
			out[f.App] = id
		}
	}
}

func themesOf(scheme string) string {
	var ids []string
	for _, id := range sortedKeys(builtinThemes) {
		if builtinThemes[id] == scheme {
			ids = append(ids, id)
		}
	}
	return strings.Join(ids, ", ")
}

func sortedKeys[T any](m map[string]T) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	slices.Sort(keys)
	return keys
}

func trimNumber(v float64) string { return strconv.FormatFloat(v, 'f', -1, 64) }

// appearanceSchema is the appearance section's JSON Schema.
func appearanceSchema() map[string]any {
	props := map[string]any{}
	for _, f := range AppearanceFields {
		var s map[string]any
		switch f.Kind {
		case fieldEnum:
			s = map[string]any{"enum": f.Enum}
		case fieldBool:
			s = map[string]any{"type": "boolean"}
		case fieldNumber:
			s = map[string]any{"type": "number", "minimum": f.Min, "maximum": f.Max}
		case fieldFont:
			s = map[string]any{"type": "string", "maxLength": 200, "pattern": `^[\w\s,'"\-.]*$`}
		case fieldColor:
			s = map[string]any{"type": []string{"string", "null"}, "pattern": `^(#[0-9a-fA-F]{3,8}|(rgb|rgba|hsl|hsla)\(.*\))$`}
		case fieldTheme:
			want := strings.TrimSuffix(f.Name, "-theme")
			var builtins []string
			for _, id := range sortedKeys(builtinThemes) {
				if builtinThemes[id] == want {
					builtins = append(builtins, id)
				}
			}
			s = map[string]any{"anyOf": []any{map[string]any{"enum": builtins}, map[string]any{"type": "string", "pattern": "^user:.+$"}}}
		}
		s["description"] = f.Description
		props[f.Name] = s
	}
	return map[string]any{"type": "object", "additionalProperties": false, "properties": props, "description": "How Lazybox looks (APPEARANCE.md). What you leave out follows the theme, then the defaults."}
}

// appearanceStarter is the starter's commented example of the section.
func appearanceStarter() string {
	return fmt.Sprintf(`#
# appearance:                # Settings → Appearance and → Chat write here too
#   mode: system             # system, light or dark
#   dark-theme: dark         # a built-in dark theme ID or user:<file>
#   light-theme: light       # a built-in light theme ID or user:<file>
#   accent: "#7fb0f0"        # quoted: YAML reads an unquoted # as a comment
#   density: comfortable     # compact, comfortable or relaxed
#   interface-font: Inter
#   code-font: JetBrains Mono
#   chat-line-height: 1.5
#   chat-line-width: 100     # characters; 0 for the full width
#   zoom: 1.1                # the desktop app (%d settings in all: see settings.schema.json)
`, len(AppearanceFields))
}
