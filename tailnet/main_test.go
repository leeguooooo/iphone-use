package main

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"log"
	"net/http"
	"net/http/httptest"
	"net/netip"
	"net/url"
	"os"
	"path/filepath"
	"sync/atomic"
	"testing"
	"time"

	"tailscale.com/client/tailscale/apitype"
	"tailscale.com/ipn/ipnstate"
	"tailscale.com/tailcfg"
	"tailscale.com/types/views"
)

func TestSanitizeAndForwardHeaders(t *testing.T) {
	h := http.Header{}
	for _, k := range []string{"X-Forwarded-For", "X-Forwarded-Proto", "X-Forwarded-Host", "X-Real-IP",
		"CF-Connecting-IP", "Forwarded", "X-Iphone-Use-Owner", "x-iphone-use-via"} {
		h.Add(k, "spoofed")
	}
	h.Set("Authorization", "Bearer keep")
	sanitizeHeaders(h)
	for k, v := range h {
		if k != "Authorization" {
			t.Errorf("header %s=%v survived", k, v)
		}
	}
	setForwardHeaders(h, "https", "iphone.tail1.ts.net", caller{IP: "100.64.0.7", LoginName: "me@example.com"})
	want := map[string]string{
		"X-Forwarded-Proto": "https", "X-Forwarded-Host": "iphone.tail1.ts.net",
		"X-Forwarded-For": "100.64.0.7", "X-Iphone-Use-Via": "tailnet",
		"X-Iphone-Use-Tailnet-User": "me@example.com", "Authorization": "Bearer keep",
	}
	for k, v := range want {
		if got := h.Values(k); len(got) != 1 || got[0] != v {
			t.Errorf("%s = %v, want [%s]", k, got, v)
		}
	}
}

func TestAllowed(t *testing.T) {
	owner := nodeInfo{OwnerID: 42}
	tagged := nodeInfo{OwnerID: 7, Tagged: true}
	cases := []struct {
		name   string
		n      nodeInfo
		caller tailcfg.UserID
		allow  bool
		want   bool
	}{
		{"owner", owner, 42, false, true},
		{"other member", owner, 43, false, false},
		{"other member allowed", owner, 43, true, true},
		{"tagged owner-only", tagged, 7, false, false},
		{"tagged allowed", tagged, 99, true, true},
		{"unknown owner", nodeInfo{}, 0, false, false},
	}
	for _, c := range cases {
		if got := allowed(c.n, c.caller, c.allow); got != c.want {
			t.Errorf("%s: got %v want %v", c.name, got, c.want)
		}
	}
}

func TestRedirectURL(t *testing.T) {
	r := httptest.NewRequest("GET", "http://100.64.0.1:80/agent/status?x=1&y=%2F", nil)
	if got, want := redirectURL("iphone.tail1.ts.net", r), "https://iphone.tail1.ts.net/agent/status?x=1&y=%2F"; got != want {
		t.Errorf("got %s want %s", got, want)
	}
	if got, want := redirectURL("", r), "https://100.64.0.1/agent/status?x=1&y=%2F"; got != want {
		t.Errorf("fallback: got %s want %s", got, want)
	}
}

func runningStatus(tagged bool) *ipnstate.Status {
	self := &ipnstate.PeerStatus{DNSName: "iphone-use-1.tail1eab92.ts.net.", UserID: 42}
	if tagged {
		v := views.SliceOf([]string{"tag:phone"})
		self.Tags = &v
	}
	return &ipnstate.Status{
		BackendState:   "Running",
		TailscaleIPs:   []netip.Addr{netip.MustParseAddr("100.101.102.103"), netip.MustParseAddr("fd7a:115c:a1e0::1")},
		Self:           self,
		CertDomains:    []string{"iphone-use-1.tail1eab92.ts.net"},
		CurrentTailnet: &ipnstate.TailnetStatus{MagicDNSEnabled: true},
		User:           map[tailcfg.UserID]tailcfg.UserProfile{42: {ID: 42, LoginName: "me@example.com"}},
	}
}

func TestStatusJSONShape(t *testing.T) {
	now := time.Date(2026, 11, 7, 1, 2, 3, 0, time.UTC)
	b, _ := json.Marshal(buildStatus(nil, false, "", now))
	var m map[string]any
	json.Unmarshal(b, &m)
	keys := []string{"version", "pid", "state", "backend_state", "login_url", "dns_name", "tailnet_ips", "https",
		"magic_dns", "owner", "allow_tailnet_members", "hint", "error", "updated_at"}
	if len(m) != len(keys) {
		t.Errorf("got %d keys: %s", len(m), b)
	}
	for _, k := range keys {
		if _, ok := m[k]; !ok {
			t.Errorf("missing key %q", k)
		}
	}
	if m["state"] != "starting" || m["login_url"] != nil || m["updated_at"] != "2026-11-07T01:02:03Z" {
		t.Errorf("starting doc: %s", b)
	}
	if ips, ok := m["tailnet_ips"].([]any); !ok || len(ips) != 0 {
		t.Errorf("tailnet_ips must be [] not null: %s", b)
	}

	d := buildStatus(runningStatus(false), false, "", now)
	if d.State != "running" || *d.DNSName != "iphone-use-1.tail1eab92.ts.net" || !d.HTTPS || !d.MagicDNS ||
		*d.Owner != "me@example.com" || d.Hint != nil || len(d.TailnetIPs) != 2 {
		t.Errorf("running doc: %+v", d)
	}

	st := runningStatus(false)
	st.CertDomains = nil
	if d := buildStatus(st, false, "", now); d.HTTPS || d.Hint == nil || *d.Hint != hintHTTPS {
		t.Errorf("https off: %+v", d)
	}
	if d := buildStatus(runningStatus(true), false, "", now); d.Owner != nil || d.Hint == nil || *d.Hint != hintTagged {
		t.Errorf("tagged: %+v", d)
	}

	nl := &ipnstate.Status{BackendState: "NeedsLogin", AuthURL: "https://login.tailscale.com/a/abc"}
	if d := buildStatus(nl, false, "", now); d.State != "needs_login" || *d.LoginURL != nl.AuthURL {
		t.Errorf("needs login: %+v", d)
	}
	if d := buildStatus(nil, false, "boom", now); d.State != "error" || *d.Error != "boom" {
		t.Errorf("error: %+v", d)
	}
}

func TestWriteStatusFileAtomic(t *testing.T) {
	p := filepath.Join(t.TempDir(), "status.json")
	if err := writeStatusFile(p, buildStatus(nil, true, "", time.Now())); err != nil {
		t.Fatal(err)
	}
	fi, err := os.Stat(p)
	if err != nil || fi.Mode().Perm() != 0o600 {
		t.Fatalf("stat: %v %v", fi.Mode(), err)
	}
	entries, _ := os.ReadDir(filepath.Dir(p))
	if len(entries) != 1 {
		t.Errorf("leftover temp files: %v", entries)
	}
}

type fakeWho struct {
	res *apitype.WhoIsResponse
	err error
}

func (f fakeWho) WhoIs(context.Context, string) (*apitype.WhoIsResponse, error) { return f.res, f.err }

func TestGatewayProxy(t *testing.T) {
	var got http.Header
	var gotURL, gotHost string
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		got, gotURL, gotHost = r.Header.Clone(), r.URL.String(), r.Host
		io.WriteString(w, "ok")
	}))
	defer up.Close()
	upURL, _ := url.Parse(up.URL)

	var node atomic.Pointer[nodeInfo]
	node.Store(&nodeInfo{Running: true, OwnerID: 42})
	me := fakeWho{res: &apitype.WhoIsResponse{UserProfile: &tailcfg.UserProfile{ID: 42, LoginName: "me@example.com"}}}
	quiet := log.New(io.Discard, "", 0)
	gw := newGateway(upURL, me, &node, false, quiet)

	req := httptest.NewRequest("GET", "http://iphone.ts.net/agent/h264?fps=30", nil)
	req.RemoteAddr = "100.64.0.9:5555"
	req.Header.Set("X-Forwarded-For", "6.6.6.6")
	req.Header.Set("X-Iphone-Use-Tailnet-User", "evil@example.com")
	rec := httptest.NewRecorder()
	gw.handler("https").ServeHTTP(rec, req)
	if rec.Code != 200 || rec.Body.String() != "ok" {
		t.Fatalf("proxy: %d %s", rec.Code, rec.Body)
	}
	if gotURL != "/agent/h264?fps=30" || gotHost != upURL.Host {
		t.Errorf("url=%s host=%s", gotURL, gotHost)
	}
	if got.Get("X-Forwarded-For") != "100.64.0.9" || got.Get("X-Iphone-Use-Tailnet-User") != "me@example.com" ||
		got.Get("X-Forwarded-Host") != "iphone.ts.net" || got.Get("X-Forwarded-Proto") != "https" {
		t.Errorf("headers: %v", got)
	}

	// Another user is refused; WhoIs failure is refused.
	for _, who := range []fakeWho{
		{res: &apitype.WhoIsResponse{UserProfile: &tailcfg.UserProfile{ID: 43}}},
		{err: errors.New("no such peer")},
	} {
		rec := httptest.NewRecorder()
		newGateway(upURL, who, &node, false, quiet).handler("http").ServeHTTP(rec, httptest.NewRequest("GET", "/", nil))
		var body map[string]string
		json.Unmarshal(rec.Body.Bytes(), &body)
		if rec.Code != 403 || body["error"] != "tailnet_forbidden" {
			t.Errorf("forbidden: %d %s", rec.Code, rec.Body)
		}
	}

	// Upstream down -> 502 daemon_unreachable.
	up.Close()
	rec = httptest.NewRecorder()
	gw.handler("http").ServeHTTP(rec, httptest.NewRequest("GET", "/", nil))
	if rec.Code != 502 || !json.Valid(rec.Body.Bytes()) {
		t.Errorf("down: %d %s", rec.Code, rec.Body)
	}
}
