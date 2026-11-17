package power

import (
	"crypto/rand"
	"crypto/sha256"
	"embed"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"html"
	"io/fs"
	"net/url"
	"os"
	"path"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
	"sync"
	"time"

	"gopkg.in/yaml.v3"
)

// Extensions (EXTENSIONS.md): sandboxed panels a person adds as a folder in
// the config folder, extensions/<id>/ with an extension.yaml and static HTML,
// JavaScript and CSS. The manifest declares its panels, the grants it wants
// and the network hosts it would reach. Nothing runs until the person
// approves exactly those grants and hosts; an update that asks for more runs
// with what was approved until they look again. An extension runs in a
// sandboxed frame on an origin of its own (extension_content.go) and reaches
// the Runner only through the app's bridge, with a credential that admits
// its approved grants alone (transport/extension_auth.go).
//
// This file is the manifest parser, the folder scan, the approvals (owner
// only, in the Runner's state), and the panel sessions: a content address and
// a credential, revoked when the grants they carry change.

// ExtensionManifestName is the manifest's file name in an extension's folder.
const ExtensionManifestName = "extension.yaml"

// ExtensionProtocol is the bridge protocol this Runner serves.
const ExtensionProtocol = 1

// Limits.
const (
	maxExtensions           = 50
	maxExtensionFiles       = 400
	maxExtensionBytes       = 8 << 20
	maxExtensionFileBytes   = 4 << 20
	maxExtensionPanels      = 8
	maxExtensionHosts       = 16
	maxExtensionGrants      = 40
	maxSessionsPerExtension = 16
)

// ExtensionIDPattern is a folder name that may be an extension's id.
var ExtensionIDPattern = regexp.MustCompile(`^[a-z0-9][a-z0-9-]{0,47}$`)

// Grants an extension may ask for. Each is one thing the approval sheet says
// in a sentence; do:<action> names one action of the app's registry marked safe.
const (
	GrantReadWorkspaces    = "read:workspaces"
	GrantReadSessions      = "read:sessions"
	GrantReadAttention     = "read:attention"
	GrantReadCommands      = "read:commands"
	GrantReadMissions      = "read:missions"
	GrantReadUsage         = "read:usage"
	GrantShowConversations = "show:conversations"
	GrantSendMessages      = "send:messages"
	GrantDoPrefix          = "do:"
)

// Account policy grants (EXTENSIONS.md, "Account policy"): switch:<provider>
// lets the extension's account policy move a conversation of that provider to
// another of the person's signed-in accounts at a limit. The Runner interprets
// the policy itself; these grants admit no RPC to a panel.
const SwitchAccountsPrefix = "switch:"

// AccountPolicyGrants are the switch grants, one per provider.
var AccountPolicyGrants = []string{SwitchAccountsPrefix + "claude", SwitchAccountsPrefix + "codex"}

// ExtensionGrants are the fixed grants, in the order the sheet lists them.
var ExtensionGrants = []string{GrantReadWorkspaces, GrantReadSessions, GrantReadAttention, GrantReadCommands, GrantReadMissions, GrantReadUsage, GrantShowConversations, GrantSendMessages}

// ExtensionIcons are the icons a panel may name (the app's own set).
var ExtensionIcons = []string{"puzzle", "chart", "columns", "activity", "target", "sliders", "globe", "bell", "inbox", "checklist", "report", "notes", "card", "tag", "sparkle", "shield", "agent", "eye", "history", "file", "folder", "chat", "terminal", "branch", "cloud", "review", "layout"}

// Extension states.
const (
	ExtensionNew      = "new"      // never approved
	ExtensionEnabled  = "enabled"  // approved, on
	ExtensionDisabled = "disabled" // approved, turned off
	ExtensionReview   = "review"   // on, but asks for more than was approved: runs with what was
	ExtensionBroken   = "broken"   // the folder or its manifest does not load
)

// ExtensionPanel is one panel an extension declares.
type ExtensionPanel struct {
	ID          string `json:"id"`
	Title       string `json:"title"`
	Icon        string `json:"icon"`
	Entry       string `json:"entry"`
	Description string `json:"description,omitempty"`
}

// ExtensionAccountPolicy is a manifest's accountPolicy: what the Runner does
// when a conversation of a provider it holds a switch grant for reaches a
// limit. Declarative on purpose: an extension's code runs in a sandboxed
// panel and never decides which account a conversation runs on.
type ExtensionAccountPolicy struct {
	// Providers are the providers of its switch:<provider> grants.
	Providers []string `json:"providers"`
	// SwitchOnLimit moves a conversation to another account at a limit.
	SwitchOnLimit bool `json:"switchOnLimit"`
	// Accounts are the accounts to try, in order (profile ids); empty is every
	// other signed-in account of the provider.
	Accounts []string `json:"accounts"`
}

// ExtensionManifest is what extension.yaml declares.
type ExtensionManifest struct {
	Name        string           `json:"name"`
	Version     string           `json:"version"`
	Description string           `json:"description"`
	Author      string           `json:"author"`
	Homepage    string           `json:"homepage"`
	Protocol    int              `json:"protocol"`
	Panels      []ExtensionPanel `json:"panels"`
	Grants      []string         `json:"grants"`
	Hosts       []string         `json:"hosts"`
	// AccountPolicy is nil when the manifest declares none.
	AccountPolicy *ExtensionAccountPolicy `json:"accountPolicy,omitempty"`
}

var versionPattern = regexp.MustCompile(`^[0-9A-Za-z][0-9A-Za-z.+-]{0,31}$`)
var hostPattern = regexp.MustCompile(`^(?:\*\.)?[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)*$`)

// ParseExtension reads an extension.yaml. Any error-severity issue makes the
// extension broken: nothing of a manifest with a mistake runs, so a grant
// never loads differently from how the person read it.
func ParseExtension(data []byte) (ExtensionManifest, []Issue, error) {
	out := ExtensionManifest{Protocol: ExtensionProtocol}
	root, err := parseRoot(data, "the file must be a mapping with name, panels and grants")
	if err != nil {
		return out, nil, err
	}
	if root == nil {
		return out, nil, errors.New("the file is empty: it needs at least a name and one panel")
	}
	p := &parser{}
	known := []string{"version", "name", "description", "author", "homepage", "lazybox", "panels", "grants", "network", "accountPolicy"}
	seen := map[string]*yaml.Node{}
	for key, value := range pairs(root) {
		seen[key.Value] = key
		switch key.Value {
		case "name":
			out.Name = strings.TrimSpace(p.str(value, "name", 48))
		case "version":
			if v := p.str(value, "version", 32); v != "" && !versionPattern.MatchString(v) {
				p.fail(value, "version", "%q is not a version such as 1.0.0", v)
			} else {
				out.Version = v
			}
		case "description":
			out.Description = strings.TrimSpace(p.str(value, "description", 240))
		case "author":
			out.Author = strings.TrimSpace(p.str(value, "author", 80))
		case "homepage":
			if v := p.str(value, "homepage", 200); v != "" {
				if u, err := url.Parse(v); err != nil || u.Scheme != "https" || u.Host == "" {
					p.fail(value, "homepage", "must be an https:// address")
				} else {
					out.Homepage = v
				}
			}
		case "lazybox":
			if value.Value != "1" {
				p.fail(value, "lazybox", "this Lazybox speaks extension protocol 1; %q is not one it knows", value.Value)
			}
		case "panels":
			out.Panels = p.extensionPanels(value)
		case "grants":
			out.Grants = p.extensionGrants(value)
		case "network":
			out.Hosts = p.extensionHosts(value)
		case "accountPolicy":
			out.AccountPolicy = p.accountPolicy(value)
		default:
			p.warn(key, key.Value, "unknown key %q is ignored%s", key.Value, didYouMean(key.Value, known))
		}
	}
	if out.Name == "" && seen["name"] == nil {
		p.issues = append(p.issues, Issue{Path: "name", Line: 1, Message: "an extension needs a name, as Settings and its panel show it", Severity: "error"})
	}
	if len(out.Panels) == 0 && seen["panels"] == nil && seen["accountPolicy"] == nil {
		p.issues = append(p.issues, Issue{Path: "panels", Line: 1, Message: "an extension needs at least one panel, or an account policy", Severity: "error"})
	}
	// Approving the grants is approving the policy, so they must agree.
	var switching []string
	for _, g := range out.Grants {
		if provider, ok := strings.CutPrefix(g, SwitchAccountsPrefix); ok {
			switching = append(switching, provider)
		}
	}
	switch {
	case out.AccountPolicy != nil && len(switching) == 0:
		p.fail(seen["accountPolicy"], "accountPolicy", "an account policy needs a grant naming the provider it switches: %s", strings.Join(AccountPolicyGrants, " or "))
	case out.AccountPolicy == nil && len(switching) > 0 && seen["accountPolicy"] == nil:
		p.fail(seen["grants"], "grants", "%s%s is for an account policy; add accountPolicy, or remove the grant", SwitchAccountsPrefix, switching[0])
	case out.AccountPolicy != nil:
		out.AccountPolicy.Providers = switching
	}
	return out, p.issues, nil
}

// accountPolicy reads a manifest's accountPolicy mapping.
func (p *parser) accountPolicy(n *yaml.Node) *ExtensionAccountPolicy {
	if n.Kind != yaml.MappingNode {
		p.fail(n, "accountPolicy", "must be a mapping with switchOnLimit and, optionally, accounts")
		return nil
	}
	policy := &ExtensionAccountPolicy{Providers: []string{}, Accounts: []string{}}
	for key, value := range pairs(n) {
		at := "accountPolicy." + key.Value
		switch key.Value {
		case "switchOnLimit":
			policy.SwitchOnLimit = p.boolean(value, at)
		case "accounts":
			if value.Tag == "!!null" {
				continue
			}
			ids := p.list(value, at, 64)
			if len(ids) > 16 {
				p.fail(value, at, "at most 16 accounts")
				continue
			}
			for _, id := range ids {
				switch {
				case !profileIDPattern.MatchString(id):
					p.fail(value, at, "%q is not an account id (Settings → Subscriptions shows them)", id)
				case slices.Contains(policy.Accounts, id):
					p.warn(value, at, "%s is listed twice", id)
				default:
					policy.Accounts = append(policy.Accounts, id)
				}
			}
		default:
			p.warn(key, at, "unknown key %q is ignored%s", key.Value, didYouMean(key.Value, []string{"switchOnLimit", "accounts"}))
		}
	}
	return policy
}

// profileIDPattern is an account id, as accounts enrolls them.
var profileIDPattern = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`)

func (p *parser) extensionPanels(n *yaml.Node) []ExtensionPanel {
	if n.Kind != yaml.SequenceNode || len(n.Content) == 0 {
		p.fail(n, "panels", "must be a list of panels, each with an id, a title and an entry page")
		return nil
	}
	if len(n.Content) > maxExtensionPanels {
		p.fail(n, "panels", "at most %d panels", maxExtensionPanels)
		return nil
	}
	var out []ExtensionPanel
	ids := map[string]bool{}
	for i, item := range n.Content {
		at := fmt.Sprintf("panels[%d]", i)
		if item.Kind != yaml.MappingNode {
			p.fail(item, at, "a panel is a mapping with id, title and entry")
			continue
		}
		panel := ExtensionPanel{Icon: "puzzle", Entry: "index.html"}
		for key, value := range pairs(item) {
			switch key.Value {
			case "id":
				panel.ID = p.str(value, at+".id", 48)
				if panel.ID != "" && !idPattern.MatchString(panel.ID) {
					p.fail(value, at+".id", "%q: use lowercase letters, digits and dashes", panel.ID)
				}
			case "title":
				panel.Title = strings.TrimSpace(p.str(value, at+".title", 48))
			case "description":
				panel.Description = strings.TrimSpace(p.str(value, at+".description", 160))
			case "icon":
				icon := p.str(value, at+".icon", 32)
				if icon != "" && !slices.Contains(ExtensionIcons, icon) {
					p.fail(value, at+".icon", "there is no icon %q%s (one of %s)", icon, didYouMean(icon, ExtensionIcons), strings.Join(ExtensionIcons, ", "))
				} else if icon != "" {
					panel.Icon = icon
				}
			case "entry":
				entry := p.str(value, at+".entry", 200)
				if problem := checkEntry(entry); problem != "" {
					p.fail(value, at+".entry", "%s", problem)
				} else {
					panel.Entry = entry
				}
			default:
				p.warn(key, at+"."+key.Value, "unknown key %q is ignored%s", key.Value, didYouMean(key.Value, []string{"id", "title", "description", "icon", "entry"}))
			}
		}
		switch {
		case panel.ID == "":
			p.fail(item, at+".id", "a panel needs an id")
		case ids[panel.ID]:
			p.fail(item, at+".id", "another panel has the id %q", panel.ID)
		}
		if panel.Title == "" {
			p.fail(item, at+".title", "a panel needs a title, as its header shows it")
		}
		ids[panel.ID] = true
		out = append(out, panel)
	}
	return out
}

// checkEntry is "" for a page of the extension's own folder.
func checkEntry(entry string) string {
	if entry == "" {
		return "an entry is the panel's page, such as index.html"
	}
	if strings.Contains(entry, "\\") || strings.Contains(entry, ":") || strings.HasPrefix(entry, "/") || strings.Contains(entry, "?") || strings.Contains(entry, "#") {
		return fmt.Sprintf("%q: name a page in the extension's folder, such as index.html", entry)
	}
	clean := path.Clean(entry)
	if clean != entry || clean == "." || strings.HasPrefix(clean, "..") || hiddenPath(clean) {
		return fmt.Sprintf("%q: name a page in the extension's folder, such as index.html (no .. or hidden files)", entry)
	}
	if ext := strings.ToLower(path.Ext(clean)); ext != ".html" && ext != ".htm" {
		return fmt.Sprintf("%q: a panel's entry is an .html page", entry)
	}
	return ""
}

func hiddenPath(p string) bool {
	for _, part := range strings.Split(p, "/") {
		if strings.HasPrefix(part, ".") || strings.HasPrefix(part, "_lazybox") {
			return true
		}
	}
	return false
}

func (p *parser) extensionGrants(n *yaml.Node) []string {
	if n.Tag == "!!null" {
		return nil
	}
	if n.Kind != yaml.SequenceNode {
		p.fail(n, "grants", "must be a list such as [read:attention, show:conversations]")
		return nil
	}
	if len(n.Content) > maxExtensionGrants {
		p.fail(n, "grants", "at most %d grants", maxExtensionGrants)
		return nil
	}
	var out []string
	for i, item := range n.Content {
		at := fmt.Sprintf("grants[%d]", i)
		grant := p.str(item, at, 80)
		if grant == "" {
			continue
		}
		if problem := checkGrant(grant); problem != "" {
			p.fail(item, at, "%s", problem)
			continue
		}
		if slices.Contains(out, grant) {
			p.warn(item, at, "%s is listed twice", grant)
			continue
		}
		out = append(out, grant)
	}
	return out
}

// checkGrant is "" for a grant an extension may ask for.
func checkGrant(grant string) string {
	if slices.Contains(ExtensionGrants, grant) || slices.Contains(AccountPolicyGrants, grant) {
		return ""
	}
	if action, ok := strings.CutPrefix(grant, GrantDoPrefix); ok {
		a, found := registry.Action(action)
		if !found {
			return fmt.Sprintf("there is no action %q%s (lazybox do --list lists them)", action, didYouMean(action, registry.ActionIDs()))
		}
		if !a.Safe {
			return fmt.Sprintf("%s is not an action an extension may run: only those that show, focus or arrange (lazybox do --list marks them safe)", action)
		}
		return ""
	}
	known := append(slices.Clone(ExtensionGrants), AccountPolicyGrants...)
	return fmt.Sprintf("there is no grant %q%s; grants are %s, or do:<action>", grant, didYouMean(grant, known), strings.Join(known, ", "))
}

func (p *parser) extensionHosts(n *yaml.Node) []string {
	if n.Tag == "!!null" {
		return nil
	}
	if n.Kind != yaml.SequenceNode {
		p.fail(n, "network", "must be a list of https:// origins, such as [https://api.example.com]")
		return nil
	}
	if len(n.Content) > maxExtensionHosts {
		p.fail(n, "network", "at most %d hosts", maxExtensionHosts)
		return nil
	}
	var out []string
	for i, item := range n.Content {
		at := fmt.Sprintf("network[%d]", i)
		raw := p.str(item, at, 200)
		if raw == "" {
			continue
		}
		host, problem := CheckExtensionHost(raw)
		if problem != "" {
			p.fail(item, at, "%s", problem)
			continue
		}
		if !slices.Contains(out, host) {
			out = append(out, host)
		}
	}
	return out
}

// CheckExtensionHost normalizes one network origin: https://host[:port], a
// *.domain wildcard, or http://localhost / 127.0.0.1 for a local service.
// Anything with a path, credentials or another scheme is refused, so the
// Content-Security-Policy built from it cannot be widened by its text.
func CheckExtensionHost(raw string) (string, string) {
	u, err := url.Parse(raw)
	if err != nil || u.Host == "" {
		return "", fmt.Sprintf("%q is not an origin such as https://api.example.com", raw)
	}
	if u.User != nil || (u.Path != "" && u.Path != "/") || u.RawQuery != "" || u.Fragment != "" {
		return "", fmt.Sprintf("%q: name an origin only (scheme, host and port), with no path", raw)
	}
	host, port := strings.ToLower(u.Hostname()), u.Port()
	local := host == "localhost" || host == "127.0.0.1"
	switch {
	case u.Scheme == "http" && !local:
		return "", fmt.Sprintf("%q: an extension reaches other machines over https only", raw)
	case u.Scheme != "https" && u.Scheme != "http":
		return "", fmt.Sprintf("%q: use https://", raw)
	case !local && !hostPattern.MatchString(host):
		return "", fmt.Sprintf("%q is not a host name", raw)
	case strings.Count(host, "*") > 0 && strings.Count(host, ".") < 2:
		return "", fmt.Sprintf("%q: a wildcard covers the subdomains of one domain, such as *.example.com", raw)
	}
	if port != "" {
		for _, c := range port {
			if c < '0' || c > '9' {
				return "", fmt.Sprintf("%q: the port is a number", raw)
			}
		}
		return u.Scheme + "://" + host + ":" + port, ""
	}
	return u.Scheme + "://" + host, ""
}

// ExtensionInfo is one extension folder as last scanned.
type ExtensionInfo struct {
	ID             string            `json:"id"`
	Path           string            `json:"path"`
	State          string            `json:"state"`
	Manifest       ExtensionManifest `json:"manifest"`
	ApprovedGrants []string          `json:"approvedGrants"`
	ApprovedHosts  []string          `json:"approvedHosts"`
	ApprovedAt     time.Time         `json:"approvedAt"`
	Err            string            `json:"error,omitempty"`
	Issues         []Issue           `json:"issues,omitempty"`
	// Digest changes whenever any of its files changes: open panels reload.
	Digest string `json:"digest"`
}

// Effective are the grants and hosts a running panel holds: what the
// manifest asks for and the person approved.
func (e ExtensionInfo) Effective() (grants, hosts []string) {
	return intersect(e.Manifest.Grants, e.ApprovedGrants), intersect(e.Manifest.Hosts, e.ApprovedHosts)
}

// Runs reports whether its panels may open.
func (e ExtensionInfo) Runs() bool { return e.State == ExtensionEnabled || e.State == ExtensionReview }

// SwitchesFor reports whether this extension's account policy switches a
// conversation of provider at a limit now: it runs, its manifest asks for
// switch:<provider> and the person approved that.
func (e ExtensionInfo) SwitchesFor(provider string) bool {
	if !e.Runs() || e.Manifest.AccountPolicy == nil || !e.Manifest.AccountPolicy.SwitchOnLimit {
		return false
	}
	grants, _ := e.Effective()
	return slices.Contains(grants, SwitchAccountsPrefix+provider)
}

func intersect(asked, approved []string) []string {
	out := []string{}
	for _, v := range asked {
		if slices.Contains(approved, v) {
			out = append(out, v)
		}
	}
	return out
}

func subset(asked, approved []string) bool {
	for _, v := range asked {
		if !slices.Contains(approved, v) {
			return false
		}
	}
	return true
}

func sameSet(a, b []string) bool { return subset(a, b) && subset(b, a) }

type extensionApproval struct {
	Grants     []string `json:"grants"`
	Hosts      []string `json:"hosts"`
	Enabled    bool     `json:"enabled"`
	ApprovedAt int64    `json:"approvedAt"`
	Version    string   `json:"version,omitempty"`
}

// ExtensionsSnapshot is every extension folder as last scanned.
type ExtensionsSnapshot struct {
	Dir        string
	Extensions []ExtensionInfo
	Revision   uint64
}

// Find returns the extension of this id.
func (s ExtensionsSnapshot) Find(id string) (ExtensionInfo, bool) {
	i := slices.IndexFunc(s.Extensions, func(e ExtensionInfo) bool { return e.ID == id })
	if i < 0 {
		return ExtensionInfo{}, false
	}
	return s.Extensions[i], true
}

// ExtensionSession is one open panel: where its sandboxed page is served and
// the credential its bridge calls the Runner with.
type ExtensionSession struct {
	Extension string
	Panel     string
	Entry     string
	// URL is the shell page (Lazybox's own) that frames the extension's entry.
	URL string
	// Base is the address every file of this session is served under, on
	// ContentOrigin.
	Base          string
	ContentOrigin string
	Origin        string
	Credential    string
	Grants        []string
	Hosts         []string
	Digest        string
	created       time.Time
	token         string
	signature     string
}

// ErrExtensionCredential is a credential that is forged, revoked or expired.
var ErrExtensionCredential = errors.New("not a live extension credential")

// ExtensionCredentialPrefix marks an extension credential; no other has it.
const ExtensionCredentialPrefix = "lbx-ext."

//go:embed extensions
var extensionAssets embed.FS

// Extensions keeps the extension folders scanned, the person's approvals,
// and the open panels' sessions.
type Extensions struct {
	dir           string
	approvalsPath string

	mu        sync.Mutex
	approvals map[string]extensionApproval
	snap      ExtensionsSnapshot
	stamp     string
	sessions  map[string]*ExtensionSession // content token → session
	creds     map[string]*ExtensionSession // credential → session
	server    *contentServer
	closed    bool
}

// NewExtensions scans dir (the config folder's extensions/, which need not
// exist) with the approvals kept at approvalsPath (the Runner's state).
func NewExtensions(dir, approvalsPath string) *Extensions {
	e := &Extensions{dir: dir, approvalsPath: approvalsPath, approvals: map[string]extensionApproval{}, sessions: map[string]*ExtensionSession{}, creds: map[string]*ExtensionSession{}}
	if data, err := os.ReadFile(approvalsPath); err == nil {
		_ = json.Unmarshal(data, &e.approvals)
	}
	e.Reload()
	return e
}

// Dir is the extensions folder.
func (e *Extensions) Dir() string { return e.dir }

// Snapshot returns the folders as last scanned.
func (e *Extensions) Snapshot() ExtensionsSnapshot {
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.snap
}

// Reload scans the folder again and reports whether anything changed. Open
// sessions whose grants or hosts no longer hold are revoked.
func (e *Extensions) Reload() (ExtensionsSnapshot, bool) {
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.reloadLocked()
}

func (e *Extensions) reloadLocked() (ExtensionsSnapshot, bool) {
	var list []ExtensionInfo
	entries, _ := os.ReadDir(e.dir)
	for _, entry := range entries {
		if !entry.IsDir() || strings.HasPrefix(entry.Name(), ".") {
			continue
		}
		if len(list) >= maxExtensions {
			break
		}
		list = append(list, e.scan(entry.Name()))
	}
	stamp := extensionsStamp(list)
	if stamp == e.stamp && e.snap.Revision > 0 {
		return e.snap, false
	}
	e.stamp = stamp
	e.snap = ExtensionsSnapshot{Dir: e.dir, Extensions: list, Revision: e.snap.Revision + 1}
	e.revokeStaleLocked()
	return e.snap, true
}

func extensionsStamp(list []ExtensionInfo) string {
	h := sha256.New()
	data, _ := json.Marshal(list)
	h.Write(data)
	return hex.EncodeToString(h.Sum(nil))
}

// InspectExtension reads one extension folder as the Runner would, without
// any approval: its state is new or broken (`lazybox extension validate`).
func InspectExtension(folder string) ExtensionInfo {
	e := &Extensions{dir: filepath.Dir(folder), approvals: map[string]extensionApproval{}}
	return e.scan(filepath.Base(folder))
}

// scan reads one folder: its manifest, its files, and the approval.
func (e *Extensions) scan(id string) ExtensionInfo {
	folder := filepath.Join(e.dir, id)
	info := ExtensionInfo{ID: id, Path: folder, ApprovedGrants: []string{}, ApprovedHosts: []string{}}
	info.Manifest.Protocol = ExtensionProtocol
	if a, ok := e.approvals[id]; ok {
		info.ApprovedGrants, info.ApprovedHosts = nonNil(a.Grants), nonNil(a.Hosts)
		info.ApprovedAt = time.UnixMilli(a.ApprovedAt)
	}
	digest, problem := digestFolder(folder)
	info.Digest = digest
	data, err := readBounded(filepath.Join(folder, ExtensionManifestName))
	switch {
	case !ExtensionIDPattern.MatchString(id):
		info.Err = fmt.Sprintf("the folder name %q is the extension's id: use lowercase letters, digits and dashes", id)
	case errors.Is(err, fs.ErrNotExist):
		info.Err = "the folder has no " + ExtensionManifestName + " (lazybox extension new writes a starting one)"
	case err != nil:
		info.Err = "the manifest cannot be read: " + err.Error()
	case problem != "":
		info.Err = problem
	default:
		manifest, issues, parseErr := ParseExtension(data)
		info.Manifest, info.Issues = manifest, issues
		if parseErr != nil {
			info.Err = parseErr.Error()
		}
		for _, panel := range manifest.Panels {
			if _, statErr := os.Lstat(filepath.Join(folder, filepath.FromSlash(panel.Entry))); statErr != nil {
				info.Issues = append(info.Issues, Issue{Path: "panels." + panel.ID + ".entry", Line: 1, Message: fmt.Sprintf("%s is not in the extension's folder", panel.Entry), Severity: "error"})
			}
		}
	}
	info.State = e.stateOf(info)
	return info
}

func nonNil(v []string) []string {
	if v == nil {
		return []string{}
	}
	return slices.Clone(v)
}

func (e *Extensions) stateOf(info ExtensionInfo) string {
	if info.Err != "" || slices.ContainsFunc(info.Issues, func(i Issue) bool { return i.Severity == "error" }) {
		return ExtensionBroken
	}
	a, ok := e.approvals[info.ID]
	switch {
	case !ok:
		return ExtensionNew
	case !a.Enabled:
		return ExtensionDisabled
	case !subset(info.Manifest.Grants, a.Grants) || !subset(info.Manifest.Hosts, a.Hosts):
		return ExtensionReview
	}
	return ExtensionEnabled
}

// servedTypes are the files an extension's folder may serve, by extension.
var servedTypes = map[string]string{
	".html": "text/html; charset=utf-8", ".htm": "text/html; charset=utf-8",
	".js": "text/javascript; charset=utf-8", ".mjs": "text/javascript; charset=utf-8",
	".css": "text/css; charset=utf-8", ".json": "application/json", ".map": "application/json",
	".svg": "image/svg+xml", ".png": "image/png", ".jpg": "image/jpeg", ".jpeg": "image/jpeg", ".gif": "image/gif", ".webp": "image/webp", ".ico": "image/x-icon",
	".woff": "font/woff", ".woff2": "font/woff2", ".ttf": "font/ttf", ".otf": "font/otf",
	".txt": "text/plain; charset=utf-8", ".md": "text/markdown; charset=utf-8",
}

// digestFolder stamps a folder's files (names, sizes, times) and checks its
// limits; problem says what is wrong with it.
func digestFolder(folder string) (digest, problem string) {
	h := sha256.New()
	files, total := 0, int64(0)
	err := filepath.WalkDir(folder, func(p string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		rel, _ := filepath.Rel(folder, p)
		if rel != "." && strings.HasPrefix(d.Name(), ".") {
			if d.IsDir() {
				return filepath.SkipDir
			}
			return nil
		}
		if d.IsDir() {
			return nil
		}
		info, err := d.Info()
		if err != nil {
			return err
		}
		if d.Type()&fs.ModeSymlink != 0 {
			return fmt.Errorf("%s is a symbolic link; an extension's files are its own, copy them in", filepath.ToSlash(rel))
		}
		files++
		total += info.Size()
		switch {
		case files > maxExtensionFiles:
			return fmt.Errorf("the folder has more than %d files", maxExtensionFiles)
		case info.Size() > maxExtensionFileBytes:
			return fmt.Errorf("%s is larger than %d MiB", filepath.ToSlash(rel), maxExtensionFileBytes>>20)
		case total > maxExtensionBytes:
			return fmt.Errorf("the folder holds more than %d MiB", maxExtensionBytes>>20)
		}
		fmt.Fprintf(h, "%s\x00%d\x00%d\n", filepath.ToSlash(rel), info.Size(), info.ModTime().UnixNano())
		return nil
	})
	sum := hex.EncodeToString(h.Sum(nil))[:16]
	if err != nil {
		return sum, err.Error()
	}
	return sum, ""
}

func (e *Extensions) saveLocked() error {
	data, err := json.MarshalIndent(e.approvals, "", "  ")
	if err != nil {
		return err
	}
	return writeAtomic(e.approvalsPath, data)
}

// ExtensionError is a refusal the app shows as it is.
type ExtensionError struct{ Reason string }

func (e *ExtensionError) Error() string { return e.Reason }

func refuse(format string, args ...any) error {
	return &ExtensionError{Reason: fmt.Sprintf(format, args...)}
}

// ErrNoExtension is an id with no extension folder.
var ErrNoExtension = errors.New("no such extension")

// Approve records the person's approval of exactly these grants and hosts,
// and turns the extension on. It is refused when the manifest asks for
// anything else now (it changed while the sheet was open) or does not load.
func (e *Extensions) Approve(id string, grants, hosts []string) (ExtensionsSnapshot, error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	e.reloadLocked()
	info, ok := e.snap.Find(id)
	if !ok {
		return e.snap, ErrNoExtension
	}
	if info.State == ExtensionBroken {
		return e.snap, refuse("%s does not load; fix its manifest first", id)
	}
	if !sameSet(info.Manifest.Grants, grants) || !sameSet(info.Manifest.Hosts, hosts) {
		return e.snap, refuse("%s asks for something else now (its manifest changed); review it again", info.Manifest.Name)
	}
	e.approvals[id] = extensionApproval{Grants: nonNil(info.Manifest.Grants), Hosts: nonNil(info.Manifest.Hosts), Enabled: true, ApprovedAt: time.Now().UnixMilli(), Version: info.Manifest.Version}
	if err := e.saveLocked(); err != nil {
		return e.snap, err
	}
	e.stamp = ""
	snap, _ := e.reloadLocked()
	return snap, nil
}

// SetEnabled turns an approved extension off or back on. Turning on one that
// asks for more than was approved is refused: it needs review first.
func (e *Extensions) SetEnabled(id string, enabled bool) (ExtensionsSnapshot, error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	e.reloadLocked()
	info, ok := e.snap.Find(id)
	a, approved := e.approvals[id]
	switch {
	case !ok:
		return e.snap, ErrNoExtension
	case !approved:
		return e.snap, refuse("%s was never approved; review what it asks for first", id)
	case enabled && (!subset(info.Manifest.Grants, a.Grants) || !subset(info.Manifest.Hosts, a.Hosts)):
		return e.snap, refuse("%s asks for more than you approved; review it first", info.Manifest.Name)
	}
	a.Enabled = enabled
	e.approvals[id] = a
	if err := e.saveLocked(); err != nil {
		return e.snap, err
	}
	e.stamp = ""
	snap, _ := e.reloadLocked()
	return snap, nil
}

// Remove deletes an extension's folder and its approval.
func (e *Extensions) Remove(id string) (ExtensionsSnapshot, error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	if !ExtensionIDPattern.MatchString(id) {
		return e.snap, ErrNoExtension
	}
	folder := filepath.Join(e.dir, id)
	if _, err := os.Lstat(folder); err != nil {
		return e.snap, ErrNoExtension
	}
	if err := os.RemoveAll(folder); err != nil {
		return e.snap, err
	}
	delete(e.approvals, id)
	if err := e.saveLocked(); err != nil {
		return e.snap, err
	}
	e.stamp = ""
	snap, _ := e.reloadLocked()
	return snap, nil
}

// ExtensionExample is an extension bundled with Lazybox.
type ExtensionExample struct {
	ID, Name, Description string
}

// ExtensionExamples are the bundled examples.
func ExtensionExamples() []ExtensionExample {
	var out []ExtensionExample
	entries, _ := extensionAssets.ReadDir("extensions/examples")
	for _, entry := range entries {
		data, err := extensionAssets.ReadFile("extensions/examples/" + entry.Name() + "/" + ExtensionManifestName)
		if err != nil {
			continue
		}
		m, _, _ := ParseExtension(data)
		out = append(out, ExtensionExample{ID: entry.Name(), Name: m.Name, Description: m.Description})
	}
	return out
}

// WriteExtension copies a bundled example (or, with example "", the starter
// with this name) into dir/id. An existing folder is never replaced.
func WriteExtension(dir, id, name, example string) (string, error) {
	if !ExtensionIDPattern.MatchString(id) {
		return "", refuse("%q is not an extension id: lowercase letters, digits and dashes", id)
	}
	source := "extensions/starter"
	if example != "" {
		if !slices.ContainsFunc(ExtensionExamples(), func(x ExtensionExample) bool { return x.ID == example }) {
			ids := []string{}
			for _, x := range ExtensionExamples() {
				ids = append(ids, x.ID)
			}
			return "", refuse("there is no example %q%s", example, didYouMean(example, ids))
		}
		source = "extensions/examples/" + example
	}
	target := filepath.Join(dir, id)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return "", err
	}
	if err := os.Mkdir(target, 0o700); err != nil {
		if errors.Is(err, fs.ErrExist) {
			return target, refuse("%s already exists; an existing extension is never replaced", target)
		}
		return "", err
	}
	if name == "" {
		name = humanize(id)
	}
	err := fs.WalkDir(extensionAssets, source, func(p string, d fs.DirEntry, err error) error {
		if err != nil || d.IsDir() {
			return err
		}
		data, err := extensionAssets.ReadFile(p)
		if err != nil {
			return err
		}
		if example == "" {
			// The name is the person's text: quoted for YAML, escaped for HTML.
			value := html.EscapeString(name)
			if strings.HasSuffix(p, ".yaml") {
				quoted, _ := json.Marshal(name)
				value = string(quoted)
			}
			data = []byte(strings.ReplaceAll(string(data), "{{name}}", value))
		}
		rel := strings.TrimPrefix(p, source+"/")
		out := filepath.Join(target, filepath.FromSlash(rel))
		if err := os.MkdirAll(filepath.Dir(out), 0o700); err != nil {
			return err
		}
		return os.WriteFile(out, data, 0o600)
	})
	if err == nil && example == "" {
		// The bridge's types, for an editor's completion.
		var types []byte
		if types, err = RuntimeAsset("lazybox.d.ts"); err == nil {
			err = os.WriteFile(filepath.Join(target, "lazybox.d.ts"), types, 0o600)
		}
	}
	if err == nil {
		err = WriteExtensionSchema(dir)
	}
	if err != nil {
		_ = os.RemoveAll(target)
		return "", err
	}
	return target, nil
}

// ExtensionSchemaName is the manifest's JSON Schema, in the extensions folder.
const ExtensionSchemaName = "extension.schema.json"

// WriteExtensionSchema writes extension.schema.json into the extensions
// folder (each manifest's header names ../extension.schema.json).
func WriteExtensionSchema(dir string) error {
	path := filepath.Join(dir, ExtensionSchemaName)
	data := []byte(ExtensionSchema())
	if current, err := os.ReadFile(path); err == nil && string(current) == string(data) {
		return nil
	}
	return writeAtomic(path, data)
}

// InstallExample copies a bundled example into the folder, not yet approved.
func (e *Extensions) InstallExample(example string) (ExtensionsSnapshot, error) {
	if _, err := WriteExtension(e.dir, example, "", example); err != nil {
		return e.Snapshot(), err
	}
	snap, _ := e.Reload()
	return snap, nil
}

// Open starts a session for one panel of a running extension. origin is the
// app's own (it may frame the page); it is checked, never trusted for more.
func (e *Extensions) Open(id, panel, origin string) (ExtensionSession, error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	if e.closed {
		return ExtensionSession{}, errors.New("the Runner is stopping")
	}
	e.reloadLocked()
	info, ok := e.snap.Find(id)
	if !ok {
		return ExtensionSession{}, ErrNoExtension
	}
	if !info.Runs() {
		switch info.State {
		case ExtensionNew:
			return ExtensionSession{}, refuse("%s is not approved yet: review what it asks for in Settings → Extensions", nameOf(info))
		case ExtensionDisabled:
			return ExtensionSession{}, refuse("%s is turned off; turn it on in Settings → Extensions", nameOf(info))
		}
		return ExtensionSession{}, refuse("%s does not load: %s", nameOf(info), firstProblem(info))
	}
	if panel == "" && len(info.Manifest.Panels) > 0 {
		panel = info.Manifest.Panels[0].ID
	}
	i := slices.IndexFunc(info.Manifest.Panels, func(p ExtensionPanel) bool { return p.ID == panel })
	if i < 0 {
		return ExtensionSession{}, refuse("%s has no panel %q", nameOf(info), panel)
	}
	if origin != "" && !validOrigin(origin) {
		return ExtensionSession{}, refuse("%q is not the app's origin", origin)
	}
	if e.server == nil {
		server, err := startContentServer(e)
		if err != nil {
			return ExtensionSession{}, fmt.Errorf("the extension content server cannot start: %w", err)
		}
		e.server = server
	}
	grants, hosts := info.Effective()
	token, credential := randomToken(24), ExtensionCredentialPrefix+randomToken(32)
	base := e.server.origin + "/x/" + token + "/"
	s := &ExtensionSession{Extension: id, Panel: panel, Entry: info.Manifest.Panels[i].Entry, Base: base, ContentOrigin: e.server.origin, Origin: origin, Credential: credential, Grants: grants, Hosts: hosts, Digest: info.Digest, created: time.Now(), token: token, signature: signature(grants, hosts)}
	s.URL = base + "_lazybox/shell.html?entry=" + url.QueryEscape(s.Entry)
	e.evictLocked(id)
	e.sessions[token], e.creds[credential] = s, s
	return *s, nil
}

func nameOf(info ExtensionInfo) string {
	if info.Manifest.Name != "" {
		return info.Manifest.Name
	}
	return info.ID
}

func firstProblem(info ExtensionInfo) string {
	if info.Err != "" {
		return info.Err
	}
	for _, issue := range info.Issues {
		if issue.Severity == "error" {
			return issue.String()
		}
	}
	return "unknown problem"
}

// validOrigin admits an origin the app can have: http(s)://host[:port], or a
// desktop shell's scheme://host.
func validOrigin(origin string) bool {
	u, err := url.Parse(origin)
	return err == nil && u.Host != "" && u.Path == "" && u.RawQuery == "" && u.User == nil && schemePattern.MatchString(u.Scheme) && !strings.ContainsAny(origin, " ;,'\"*")
}

var schemePattern = regexp.MustCompile(`^[a-z][a-z0-9+.-]*$`)

func signature(grants, hosts []string) string {
	return strings.Join(grants, ",") + "|" + strings.Join(hosts, ",")
}

func randomToken(n int) string {
	b := make([]byte, n)
	_, _ = rand.Read(b)
	return base64.RawURLEncoding.EncodeToString(b)
}

// evictLocked keeps at most maxSessionsPerExtension sessions of one
// extension, dropping the oldest.
func (e *Extensions) evictLocked(id string) {
	var mine []*ExtensionSession
	for _, s := range e.sessions {
		if s.Extension == id {
			mine = append(mine, s)
		}
	}
	slices.SortFunc(mine, func(a, b *ExtensionSession) int { return a.created.Compare(b.created) })
	for len(mine) >= maxSessionsPerExtension {
		e.dropLocked(mine[0])
		mine = mine[1:]
	}
}

func (e *Extensions) dropLocked(s *ExtensionSession) {
	delete(e.sessions, s.token)
	delete(e.creds, s.Credential)
}

// revokeStaleLocked drops every session whose extension is gone, no longer
// runs, or would now hold other grants or hosts.
func (e *Extensions) revokeStaleLocked() {
	for _, s := range e.sessions {
		info, ok := e.snap.Find(s.Extension)
		if !ok || !info.Runs() {
			e.dropLocked(s)
			continue
		}
		if grants, hosts := info.Effective(); signature(grants, hosts) != s.signature {
			e.dropLocked(s)
		}
	}
}

// Close revokes every session and stops the content server.
func (e *Extensions) Close() {
	e.mu.Lock()
	server := e.server
	e.server, e.closed = nil, true
	e.sessions, e.creds = map[string]*ExtensionSession{}, map[string]*ExtensionSession{}
	e.mu.Unlock()
	if server != nil {
		server.close()
	}
}

// Scope returns the extension and grants of a live extension credential.
func (e *Extensions) Scope(credential string) (extension string, grants []string, err error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	s, ok := e.creds[credential]
	if !ok || !strings.HasPrefix(credential, ExtensionCredentialPrefix) {
		return "", nil, ErrExtensionCredential
	}
	return s.Extension, slices.Clone(s.Grants), nil
}

// session returns the live session of a content token.
func (e *Extensions) session(token string) (ExtensionSession, bool) {
	e.mu.Lock()
	defer e.mu.Unlock()
	s, ok := e.sessions[token]
	if !ok {
		return ExtensionSession{}, false
	}
	return *s, true
}

// HasGrant reports whether grants hold this one.
func HasGrant(grants []string, grant string) bool { return slices.Contains(grants, grant) }

// ExtensionSchema is extension.yaml's JSON Schema, naming every grant and icon.
func ExtensionSchema() string {
	var doGrants []string
	for _, a := range registry.Actions {
		if a.Safe {
			doGrants = append(doGrants, GrantDoPrefix+a.ID)
		}
	}
	schema := map[string]any{
		"$schema": "https://json-schema.org/draft/2020-12/schema", "$id": "urn:lazybox:schema:extension:v1", "title": "A Lazybox extension (extensions/<id>/extension.yaml)",
		"type": "object", "additionalProperties": false, "required": []string{"name"},
		"properties": map[string]any{
			"lazybox":     map[string]any{"const": 1, "description": "The extension protocol"},
			"name":        map[string]any{"type": "string", "maxLength": 48},
			"version":     map[string]any{"type": "string", "pattern": versionPattern.String()},
			"description": map[string]any{"type": "string", "maxLength": 240},
			"author":      map[string]any{"type": "string", "maxLength": 80},
			"homepage":    map[string]any{"type": "string", "pattern": "^https://"},
			"panels": map[string]any{"type": "array", "minItems": 1, "maxItems": maxExtensionPanels, "items": map[string]any{
				"type": "object", "additionalProperties": false, "required": []string{"id", "title"},
				"properties": map[string]any{
					"id":          map[string]any{"type": "string", "pattern": idPattern.String()},
					"title":       map[string]any{"type": "string", "maxLength": 48},
					"description": map[string]any{"type": "string", "maxLength": 160},
					"icon":        map[string]any{"enum": ExtensionIcons},
					"entry":       map[string]any{"type": "string", "pattern": `^[^/:?#\\][^:?#\\]*\.html?$`, "default": "index.html"},
				}}},
			"grants": map[string]any{"type": "array", "maxItems": maxExtensionGrants, "uniqueItems": true, "items": map[string]any{"enum": append(append(slices.Clone(ExtensionGrants), AccountPolicyGrants...), doGrants...)}},
			"accountPolicy": map[string]any{"type": "object", "additionalProperties": false, "required": []string{"switchOnLimit"},
				"description": "What the Runner does when a conversation reaches a limit (EXTENSIONS.md, Account policy); needs a switch:<provider> grant",
				"properties": map[string]any{
					"switchOnLimit": map[string]any{"type": "boolean", "description": "Move the conversation to another of your signed-in accounts"},
					"accounts":      map[string]any{"type": "array", "maxItems": 16, "uniqueItems": true, "items": map[string]any{"type": "string"}, "description": "Accounts to try, in order; empty is every other one"},
				}},
			"network": map[string]any{"type": "array", "maxItems": maxExtensionHosts, "items": map[string]any{"type": "string", "pattern": `^(https://(\*\.)?[a-z0-9.-]+|http://(localhost|127\.0\.0\.1))(:[0-9]+)?/?$`}},
		},
	}
	data, _ := json.MarshalIndent(schema, "", "  ")
	return string(data) + "\n"
}
