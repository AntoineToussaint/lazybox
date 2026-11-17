// Package serverlike tells whether a command looks like one that does not
// end by itself: a dev server, a watcher, a log follower. `lazybox run` uses
// it to move such a command to the background once its grace period is over
// (COMMAND_OUTPUT.md, "Servers and watchers"); the client mirrors the same
// rules for Run on a code block (packages/surfaces/src/code-run.ts), and both
// are pinned by testdata/commands.json.
//
// A match is a guess, never a promise: a command that ends within the grace
// period is an ordinary run whatever it looked like, and one that opens a
// listening port is a server whatever it is called.
package serverlike

import (
	"path"
	"regexp"
	"strings"
)

// Match reports whether a shell script looks like a server or a watcher, and
// the simple command that says so.
func Match(script string) (bool, string) {
	for _, command := range simpleCommands(script) {
		if matches(words(command)) {
			return true, strings.TrimSpace(command)
		}
	}
	return false, ""
}

// MatchArgv is Match for a command line: the script of `sh -c SCRIPT` (or
// bash, zsh, with -lc), else the words joined.
func MatchArgv(argv []string) (bool, string) {
	if len(argv) >= 3 {
		switch path.Base(argv[0]) {
		case "sh", "bash", "zsh", "dash", "fish":
			for i := 1; i < len(argv)-1; i++ {
				if f := argv[i]; strings.HasPrefix(f, "-") && strings.Contains(f, "c") {
					return Match(argv[i+1])
				}
			}
		}
	}
	return Match(strings.Join(argv, " "))
}

var (
	separators = regexp.MustCompile(`&&|\|\||[;&|\n]`)
	assignment = regexp.MustCompile(`^[A-Za-z_][A-Za-z0-9_]*=`)
	// lazyboxCLI is the Lazybox CLI called by its full path (quoted when it
	// has spaces) or through $LAZYBOX_V2_CLI: the agent briefing's fallback
	// when another lazybox comes first on PATH. It reads as `lazybox`.
	lazyboxCLI = regexp.MustCompile(`"[^"]*/lazybox"|'[^']*/lazybox'|"\$(?:LAZYBOX_V2_CLI|\{LAZYBOX_V2_CLI\})"|\$(?:\{LAZYBOX_V2_CLI\}|LAZYBOX_V2_CLI\b)`)
)

func simpleCommands(script string) []string {
	script = strings.ReplaceAll(script, "\\\n", " ")
	return separators.Split(script, -1)
}

// words splits a simple command, drops quotes, environment assignments and
// wrappers (sudo, env, npx, uv run, `lazybox run --`, …), and lowercases
// nothing but the program's base name.
func words(command string) []string {
	var out []string
	for _, w := range strings.Fields(lazyboxCLI.ReplaceAllString(command, "lazybox")) {
		out = append(out, strings.Trim(w, `"'`))
	}
	for len(out) > 0 {
		w := out[0]
		switch {
		case assignment.MatchString(w):
			out = out[1:] // FOO=bar
		case wrapper[path.Base(w)]:
			out = out[1:]
			for len(out) > 0 && strings.HasPrefix(out[0], "-") { // sudo -E, env -i, npx -y
				out = out[1:]
			}
		case path.Base(w) == "lazybox" && len(out) > 1 && out[1] == "run": // also by its full path
			rest := out[2:]
			out = nil
			for i, r := range rest {
				if r == "--" {
					out = rest[i+1:]
					break
				}
				if !strings.HasPrefix(r, "-") {
					out = rest[i:]
					break
				}
			}
		case (w == "uv" || w == "poetry" || w == "pipenv" || w == "pdm" || w == "rye" || w == "hatch") && len(out) > 1 && out[1] == "run",
			w == "bundle" && len(out) > 1 && out[1] == "exec",
			w == "go" && len(out) > 2 && out[1] == "tool":
			out = out[2:]
		default:
			if len(out) > 0 {
				out[0] = strings.ToLower(path.Base(out[0]))
			}
			return out
		}
	}
	return out
}

var wrapper = set("sudo", "env", "exec", "time", "nohup", "npx", "pnpx", "bunx", "uvx", "cross-env", "dotenv", "caffeinate", "command", "nice", "stdbuf", "unbuffer")

// tasks are task names that start something that keeps running: `npm run
// dev`, `make up`, `just watch`, `pnpm start`.
var tasks = set("dev", "develop", "start", "serve", "server", "up", "watch", "preview", "runserver")

func serverTask(task string) bool {
	if tasks[task] {
		return true
	}
	for _, prefix := range []string{"dev:", "serve:", "start:", "watch:"} {
		if strings.HasPrefix(task, prefix) {
			return true
		}
	}
	for _, suffix := range []string{":dev", ":serve", ":start", ":watch", "-watch", "_watch", ":up"} {
		if strings.HasSuffix(task, suffix) {
			return true
		}
	}
	return false
}

// servers are programs that serve or watch whatever their arguments.
var servers = set("nodemon", "webpack-dev-server", "http-server", "live-server", "browser-sync", "uvicorn", "gunicorn", "hypercorn", "daphne", "air", "reflex", "watchexec", "entr", "ngrok", "redis-server", "mongod", "postgres", "mailhog", "mailpit", "json-server", "serve", "lt", "localtunnel", "start-storybook")

// subcommands are a program's arguments that make it a server: `next dev`,
// `rails s`, `hugo server`. "*" stands for no argument at all.
var subcommands = map[string][]string{
	"vite":            {"*", "dev", "serve", "preview"},
	"next":            {"*", "dev", "start"},
	"nuxt":            {"dev", "start", "preview"},
	"nuxi":            {"dev", "preview"},
	"astro":           {"dev", "preview"},
	"remix":           {"dev"},
	"svelte-kit":      {"dev", "preview"},
	"gatsby":          {"develop", "serve"},
	"expo":            {"start"},
	"react-scripts":   {"start"},
	"webpack":         {"serve"},
	"ng":              {"serve"},
	"vue-cli-service": {"serve"},
	"flask":           {"run"},
	"rails":           {"s", "server"},
	"hugo":            {"server", "serve"},
	"jekyll":          {"serve", "s"},
	"mkdocs":          {"serve"},
	"docusaurus":      {"start", "serve"},
	"storybook":       {"dev"},
	"firebase":        {"serve", "emulators:start"},
	"kubectl":         {"port-forward", "proxy"},
	"minikube":        {"tunnel"},
	"tilt":            {"up"},
	"skaffold":        {"dev"},
	"wrangler":        {"dev"},
	"cargo":           {"watch"},
	"caddy":           {"run"},
	"deno":            {"serve"},
	"php":             {"-s"},
	"artisan":         {"serve"},
	"manage.py":       {"runserver"},
	"jupyter":         {"notebook", "lab", "server"},
}

// genericSubcommands make any program a server when they are its first
// argument: `bin/devbox up`, `codefly dev`, `hugo serve`. "start" is not one:
// `docker start`, `colima start` and `systemctl start` return at once.
var genericSubcommands = set("serve", "server", "dev", "watch", "up")

func matches(w []string) bool {
	if len(w) == 0 {
		return false
	}
	program, args := w[0], w[1:]
	first := ""
	if len(args) > 0 {
		first = strings.ToLower(args[0])
	}
	for _, a := range args {
		switch strings.ToLower(a) {
		case "--watch", "--watchall", "--serve", "--watch=true":
			return true
		}
		if strings.HasPrefix(a, "--watch=") && a != "--watch=false" {
			return true
		}
	}
	switch program {
	case "npm", "pnpm", "yarn", "bun", "turbo":
		task := first
		if (task == "run" || task == "run-script") && len(args) > 1 {
			task = strings.ToLower(args[1])
		}
		return serverTask(task) || (program == "npm" && task == "start")
	case "make", "just", "task", "mise", "nx", "gradle", "gradlew":
		for _, a := range args {
			if a == "run" && program == "mise" {
				continue
			}
			if !strings.HasPrefix(a, "-") && (serverTask(strings.ToLower(a)) || a == "bootRun") {
				return true
			}
		}
		return false
	case "docker", "podman":
		if first == "compose" {
			return composeUp(args[1:])
		}
		return (first == "logs" || first == "events") && follows(args)
	case "docker-compose", "podman-compose":
		return composeUp(args)
	case "tail":
		return follows(args)
	case "journalctl":
		return follows(args)
	case "python", "python3", "python2":
		for i, a := range args {
			if a == "-m" && i+1 < len(args) {
				switch args[i+1] {
				case "http.server", "uvicorn", "gunicorn", "flask", "streamlit", "jupyter":
					return args[i+1] != "flask" || (i+2 < len(args) && args[i+2] == "run")
				}
			}
			if path.Base(a) == "manage.py" && i+1 < len(args) && args[i+1] == "runserver" {
				return true
			}
		}
		return false
	case "streamlit":
		return first == "run"
	case "ssh":
		for _, a := range args {
			if a == "-N" || strings.HasPrefix(a, "-N") && !strings.Contains(a, "=") {
				return true
			}
		}
		return false
	}
	if program == "kubectl" && first == "logs" || program == "kubectl" && first == "get" {
		for _, a := range args {
			if a == "-w" || a == "--watch" || a == "-f" || a == "--follow" {
				return true
			}
		}
	}
	if servers[program] {
		return true
	}
	if want, ok := subcommands[program]; ok {
		for _, s := range want {
			if s == "*" && (len(args) == 0 || strings.HasPrefix(first, "-")) || s == first {
				return true
			}
		}
		return false
	}
	return genericSubcommands[first]
}

// composeUp: `docker compose up` stays attached unless told to detach.
func composeUp(args []string) bool {
	up := false
	for _, a := range args {
		switch a {
		case "up", "watch":
			up = true
		case "-d", "--detach", "--wait":
			return false
		}
	}
	return up
}

func follows(args []string) bool {
	for _, a := range args {
		if a == "-f" || a == "-F" || a == "--follow" || strings.HasPrefix(a, "--follow=") || strings.HasPrefix(a, "-") && !strings.HasPrefix(a, "--") && strings.ContainsAny(a, "fF") && len(a) <= 4 {
			return true
		}
	}
	return false
}

func set(values ...string) map[string]bool {
	m := make(map[string]bool, len(values))
	for _, v := range values {
		m[v] = true
	}
	return m
}
