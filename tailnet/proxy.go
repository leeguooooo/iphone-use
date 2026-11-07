package main

import (
	"context"
	"encoding/json"
	"log"
	"net"
	"net/http"
	"net/http/httputil"
	"net/url"
	"strings"
	"sync/atomic"
	"time"

	"tailscale.com/client/tailscale/apitype"
	"tailscale.com/tailcfg"
)

const forbiddenMessage = "only the Tailscale account that owns this iPhone node can connect; turn on allow-tailnet-members to let others in"

// strippedHeaders are client-supplied forwarding headers that must never
// reach the daemon; it trusts them to describe the real caller.
var strippedHeaders = []string{
	"X-Forwarded-For", "X-Forwarded-Proto", "X-Forwarded-Host",
	"X-Real-Ip", "Cf-Connecting-Ip", "Forwarded",
}

// sanitizeHeaders removes spoofable forwarding headers and every
// X-Iphone-Use-* header from h.
func sanitizeHeaders(h http.Header) {
	for _, k := range strippedHeaders {
		h.Del(k)
	}
	for k := range h {
		if strings.HasPrefix(http.CanonicalHeaderKey(k), "X-Iphone-Use-") {
			delete(h, k)
		}
	}
}

// caller is the tailnet identity of a request, resolved via WhoIs.
type caller struct {
	IP        string
	UserID    tailcfg.UserID
	LoginName string
}

// setForwardHeaders sets the headers the daemon relies on. Call after
// sanitizeHeaders.
func setForwardHeaders(h http.Header, proto, host string, c caller) {
	h.Set("X-Forwarded-Proto", proto)
	h.Set("X-Forwarded-Host", host)
	h.Set("X-Forwarded-For", c.IP)
	h.Set("X-Iphone-Use-Via", "tailnet")
	h.Set("X-Iphone-Use-Tailnet-User", c.LoginName)
}

// allowed decides whether a WhoIs-resolved caller may reach the daemon.
// Owner-only means the caller's user is the user that owns this node; a
// tagged node has no owning user, so only allowMembers can let anyone in.
func allowed(n nodeInfo, callerID tailcfg.UserID, allowMembers bool) bool {
	if allowMembers {
		return true
	}
	if n.Tagged || n.OwnerID == 0 {
		return false
	}
	return callerID == n.OwnerID
}

// redirectURL is where the :80 listener sends a request.
func redirectURL(dnsName string, r *http.Request) string {
	host := dnsName
	if host == "" {
		host, _, _ = net.SplitHostPort(r.Host)
		if host == "" {
			host = r.Host
		}
	}
	return (&url.URL{Scheme: "https", Host: host, Path: r.URL.Path, RawPath: r.URL.RawPath, RawQuery: r.URL.RawQuery}).String()
}

func writeJSONError(w http.ResponseWriter, code int, errCode, msg string) {
	w.Header().Set("Content-Type", "application/json")
	w.Header().Set("Cache-Control", "no-store")
	w.WriteHeader(code)
	json.NewEncoder(w).Encode(map[string]string{"error": errCode, "message": msg})
}

type whoIser interface {
	WhoIs(ctx context.Context, remoteAddr string) (*apitype.WhoIsResponse, error)
}

// gateway authorizes tailnet requests and proxies them to the daemon.
type gateway struct {
	who          whoIser
	node         *atomic.Pointer[nodeInfo]
	allowMembers bool
	proxy        *httputil.ReverseProxy
}

type ctxKey struct{}

type fwd struct {
	proto string
	c     caller
}

func newGateway(upstream *url.URL, who whoIser, node *atomic.Pointer[nodeInfo], allowMembers bool, errLog *log.Logger) *gateway {
	rp := &httputil.ReverseProxy{
		Rewrite: func(pr *httputil.ProxyRequest) {
			f := pr.In.Context().Value(ctxKey{}).(fwd)
			pr.SetURL(upstream) // path/query kept; Host set to upstream
			pr.Out.URL.Path, pr.Out.URL.RawPath = pr.In.URL.Path, pr.In.URL.RawPath
			sanitizeHeaders(pr.Out.Header)
			setForwardHeaders(pr.Out.Header, f.proto, pr.In.Host, f.c)
		},
		FlushInterval: -1, // h264 / MJPEG streams: never buffer
		ErrorLog:      errLog,
		ErrorHandler: func(w http.ResponseWriter, r *http.Request, err error) {
			errLog.Printf("upstream: %v", err)
			writeJSONError(w, http.StatusBadGateway, "daemon_unreachable", "the iphone-use daemon is not reachable")
		},
	}
	return &gateway{who: who, node: node, allowMembers: allowMembers, proxy: rp}
}

// handler returns the handler for a listener speaking proto ("https"/"http").
func (g *gateway) handler(proto string) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		ctx, cancel := context.WithTimeout(r.Context(), 10*time.Second)
		res, err := g.who.WhoIs(ctx, r.RemoteAddr)
		cancel()
		n := g.node.Load()
		if err != nil || res == nil || res.UserProfile == nil || n == nil ||
			!allowed(*n, res.UserProfile.ID, g.allowMembers) {
			writeJSONError(w, http.StatusForbidden, "tailnet_forbidden", forbiddenMessage)
			return
		}
		ip, _, _ := net.SplitHostPort(r.RemoteAddr)
		c := caller{IP: ip, UserID: res.UserProfile.ID, LoginName: res.UserProfile.LoginName}
		g.proxy.ServeHTTP(w, r.WithContext(context.WithValue(r.Context(), ctxKey{}, fwd{proto, c})))
	})
}

// redirectHandler sends plain :80 traffic to the HTTPS name.
func redirectHandler(node *atomic.Pointer[nodeInfo]) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var dns string
		if n := node.Load(); n != nil {
			dns = n.DNSName
		}
		http.Redirect(w, r, redirectURL(dns, r), http.StatusPermanentRedirect)
	})
}
