// Package gitidentity separates repository identity from its local storage.
package gitidentity

import (
	"crypto/sha256"
	"encoding/hex"
	"net/url"
	"regexp"
	"strings"
)

// ID requires the canonical absolute Git common directory resolved by Git.
func ID(commonDirectory string) string {
	sum := sha256.Sum256([]byte(commonDirectory))
	return "local-" + hex.EncodeToString(sum[:])
}

var githubComponent = regexp.MustCompile(`^[a-zA-Z0-9][a-zA-Z0-9._-]{0,127}$`)

// GitHubName recognizes GitHub's HTTPS, SSH and SCP-style origin URLs. It
// returns only owner/repo, never userinfo, credentials or transport details.
// Unknown hosts and SSH aliases are not guessed to be GitHub.
func GitHubName(remote string) string {
	remote = strings.TrimSpace(remote)
	if strings.HasPrefix(strings.ToLower(remote), "git@github.com:") {
		remote = "ssh://git@github.com/" + remote[len("git@github.com:"):]
	}
	u, err := url.Parse(remote)
	if err != nil || u.RawQuery != "" || u.Fragment != "" || u.RawPath != "" {
		return ""
	}
	host := strings.ToLower(u.Hostname())
	if !(host == "github.com" && (u.Scheme == "https" && (u.Port() == "" || u.Port() == "443") || u.Scheme == "ssh" && (u.Port() == "" || u.Port() == "22"))) &&
		!(host == "ssh.github.com" && u.Scheme == "ssh" && u.Port() == "443") {
		return ""
	}
	name := strings.TrimSuffix(strings.TrimSuffix(strings.TrimPrefix(u.Path, "/"), "/"), ".git")
	parts := strings.Split(name, "/")
	if len(parts) != 2 || !githubComponent.MatchString(parts[0]) || !githubComponent.MatchString(parts[1]) {
		return ""
	}
	return strings.ToLower(name)
}

// Repository uses provider identity when known, falling back to the Git common
// directory for repositories without a recognized GitHub origin.
func Repository(commonDirectory, remote string) string {
	if name := GitHubName(remote); name != "" {
		return "github:" + name
	}
	return ID(commonDirectory)
}
