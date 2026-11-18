package link

import (
	"bufio"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

// SSHHost is one Host entry of the person's ssh configuration, as Add a
// computer lists it.
type SSHHost struct {
	Name     string `json:"name"`
	HostName string `json:"hostName,omitempty"`
	User     string `json:"user,omitempty"`
	Port     string `json:"port,omitempty"`
	// Source is the file it was found in.
	Source string `json:"source"`
}

// SSHHosts lists the concrete Host aliases of ~/.ssh/config and the files it
// Includes. Wildcard patterns (`Host *`, `Host *.corp`) and negations are not
// hosts one can pick, so they are left out; Match blocks are skipped.
func SSHHosts(home string) []SSHHost {
	dir := filepath.Join(home, ".ssh")
	seen := map[string]bool{}
	visited := map[string]bool{}
	var out []SSHHost
	var read func(path string, depth int)
	read = func(path string, depth int) {
		if depth > 8 || visited[path] {
			return
		}
		visited[path] = true
		f, err := os.Open(path)
		if err != nil {
			return
		}
		defer f.Close()
		var current []int // indexes into out of the Host block being read
		scanner := bufio.NewScanner(f)
		for scanner.Scan() {
			key, value := sshLine(scanner.Text())
			switch key {
			case "":
				continue
			case "include":
				for _, pattern := range strings.Fields(value) {
					pattern = expandHome(pattern, home)
					if !filepath.IsAbs(pattern) {
						pattern = filepath.Join(dir, pattern)
					}
					matches, _ := filepath.Glob(pattern)
					sort.Strings(matches)
					for _, m := range matches {
						read(m, depth+1)
					}
				}
			case "match":
				current = nil
			case "host":
				current = nil
				for _, name := range strings.Fields(value) {
					if strings.ContainsAny(name, "*?!") || !ValidHost(name) || seen[name] {
						continue
					}
					seen[name] = true
					out = append(out, SSHHost{Name: name, Source: path})
					current = append(current, len(out)-1)
				}
			case "hostname", "user", "port":
				for _, i := range current {
					switch key {
					case "hostname":
						out[i].HostName = firstNonEmpty(out[i].HostName, value)
					case "user":
						out[i].User = firstNonEmpty(out[i].User, value)
					case "port":
						out[i].Port = firstNonEmpty(out[i].Port, value)
					}
				}
			}
		}
	}
	read(filepath.Join(dir, "config"), 0)
	return out
}

// sshLine splits `Key value` or `Key=value`, lowercasing the key; comments
// and blank lines give "".
func sshLine(line string) (string, string) {
	line = strings.TrimSpace(line)
	if line == "" || strings.HasPrefix(line, "#") {
		return "", ""
	}
	i := strings.IndexAny(line, " \t=")
	if i < 0 {
		return strings.ToLower(line), ""
	}
	key := strings.ToLower(line[:i])
	value := strings.TrimLeft(line[i:], " \t=")
	value = strings.Trim(value, `"`)
	return key, value
}

func expandHome(path, home string) string {
	if path == "~" {
		return home
	}
	if rest, ok := strings.CutPrefix(path, "~/"); ok {
		return filepath.Join(home, rest)
	}
	return path
}
