package power

import (
	"encoding/json"
	"errors"
	"io"
	"io/fs"
	"net"
	"net/http"
	"net/url"
	"os"
	"path"
	"path/filepath"
	"strings"
	"time"
)

// The extension content origin (EXTENSIONS.md, "Where an extension runs"): a
// loopback HTTP server of its own, so an extension's pages never share an
// origin with the Runner's API or the app. Every file is served under a
// session's unguessable base, /x/<token>/, and only while that session lives:
//
//	/x/<token>/_lazybox/shell.html?entry=index.html   Lazybox's frame (the app frames this)
//	/x/<token>/_lazybox/lazybox.js, lazybox.css        the bridge client and base style
//	/x/<token>/<file>                                  the extension's own files
//
// The shell is sandboxed (allow-scripts) and frames the entry, sandboxed
// again, under a Content-Security-Policy whose frame-src is the session's
// base: the extension cannot navigate itself anywhere else, so it cannot
// carry what it read out in an address. The extension's own policy allows
// scripts and styles from the base alone and network only to the hosts the
// person approved. Both policies come from extensions/runtime/policy.json,
// which the browser fixture serves too.

type contentServer struct {
	ext    *Extensions
	origin string
	host   string
	server *http.Server
}

type contentPolicies struct {
	Shell     string `json:"shell"`
	Extension string `json:"extension"`
}

var policies = func() contentPolicies {
	var p contentPolicies
	data, err := extensionAssets.ReadFile("extensions/runtime/policy.json")
	if err == nil {
		err = json.Unmarshal(data, &p)
	}
	if err != nil || p.Shell == "" || p.Extension == "" {
		panic("power: extensions/runtime/policy.json: " + errors.Join(err, errors.New("both policies are required")).Error())
	}
	return p
}()

// ShellPolicy is the shell page's Content-Security-Policy for a session.
func ShellPolicy(base, appOrigin string) string {
	ancestors := ""
	if u, err := url.Parse(appOrigin); err == nil && (u.Scheme == "http" || u.Scheme == "https") && validOrigin(appOrigin) {
		ancestors = "; frame-ancestors " + appOrigin
	}
	return strings.NewReplacer("{base}", base, "{frameAncestors}", ancestors).Replace(policies.Shell)
}

// ExtensionPolicy is an extension file's Content-Security-Policy: hosts are
// the approved network origins (CheckExtensionHost normalized them).
func ExtensionPolicy(base string, hosts []string) string {
	connect := "'none'"
	if len(hosts) > 0 {
		connect = strings.Join(hosts, " ")
	}
	return strings.NewReplacer("{base}", base, "{hosts}", connect).Replace(policies.Extension)
}

func startContentServer(ext *Extensions) (*contentServer, error) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return nil, err
	}
	host := ln.Addr().String()
	c := &contentServer{ext: ext, origin: "http://" + host, host: host}
	c.server = &http.Server{Handler: c, ReadHeaderTimeout: 10 * time.Second, ReadTimeout: 30 * time.Second, WriteTimeout: 60 * time.Second, IdleTimeout: 120 * time.Second, MaxHeaderBytes: 32 << 10}
	go func() { _ = c.server.Serve(ln) }()
	return c, nil
}

func (c *contentServer) close() { _ = c.server.Close() }

// runtimeFiles are Lazybox's own files under _lazybox/.
var runtimeFiles = map[string]string{
	"shell.html":  "text/html; charset=utf-8",
	"shell.js":    "text/javascript; charset=utf-8",
	"shell.css":   "text/css; charset=utf-8",
	"lazybox.js":  "text/javascript; charset=utf-8",
	"lazybox.css": "text/css; charset=utf-8",
	"workflow.js": "text/javascript; charset=utf-8",
}

func (c *contentServer) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	h := w.Header()
	h.Set("X-Content-Type-Options", "nosniff")
	h.Set("Cache-Control", "no-store")
	h.Set("Referrer-Policy", "no-referrer")
	// A page elsewhere that learned this address (DNS rebinding) is not served.
	if r.Host != c.host {
		http.Error(w, "not here", http.StatusMisdirectedRequest)
		return
	}
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		h.Set("Allow", "GET, HEAD")
		http.Error(w, "read only", http.StatusMethodNotAllowed)
		return
	}
	rest, ok := strings.CutPrefix(r.URL.Path, "/x/")
	token, file, found := strings.Cut(rest, "/")
	if !ok || !found {
		http.NotFound(w, r)
		return
	}
	s, live := c.ext.session(token)
	if !live {
		http.Error(w, "this extension page is closed; reopen its panel", http.StatusGone)
		return
	}
	// Module scripts and fonts are fetched with CORS from the sandbox's
	// opaque origin; the files are static and the base is the secret.
	h.Set("Access-Control-Allow-Origin", "*")
	if name, ok := strings.CutPrefix(file, "_lazybox/"); ok {
		kind, known := runtimeFiles[name]
		data, err := extensionAssets.ReadFile("extensions/runtime/" + name)
		if !known || err != nil {
			http.NotFound(w, r)
			return
		}
		h.Set("Content-Type", kind)
		if name == "shell.html" {
			h.Set("Content-Security-Policy", ShellPolicy(s.Base, s.Origin))
		} else {
			h.Set("Content-Security-Policy", ExtensionPolicy(s.Base, s.Hosts))
		}
		_, _ = w.Write(data)
		return
	}
	clean := path.Clean("/" + file)[1:]
	kind := servedTypes[strings.ToLower(path.Ext(clean))]
	if clean == "" || clean != file || hiddenPath(clean) || kind == "" || clean == ExtensionManifestName {
		http.NotFound(w, r)
		return
	}
	root, err := os.OpenRoot(filepath.Join(c.ext.Dir(), s.Extension))
	if err != nil {
		http.NotFound(w, r)
		return
	}
	defer root.Close()
	f, err := root.Open(filepath.FromSlash(clean))
	if err != nil {
		http.NotFound(w, r)
		return
	}
	defer f.Close()
	info, err := f.Stat()
	if err != nil || !info.Mode().IsRegular() || info.Size() > maxExtensionFileBytes {
		http.NotFound(w, r)
		return
	}
	h.Set("Content-Type", kind)
	h.Set("Content-Security-Policy", ExtensionPolicy(s.Base, s.Hosts))
	if r.Method == http.MethodHead {
		return
	}
	_, _ = io.Copy(w, io.LimitReader(f, maxExtensionFileBytes))
}

// RuntimeAsset is one of Lazybox's own extension files (the SDK's lazybox.js
// and lazybox.d.ts that `lazybox extension new` writes beside a new one).
func RuntimeAsset(name string) ([]byte, error) {
	return fs.ReadFile(extensionAssets, "extensions/runtime/"+name)
}
