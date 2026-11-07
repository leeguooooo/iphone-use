//go:build integration

// End-to-end check of iphone-use-tailnet on a private tailnet: Tailscale's
// in-process test control server and DERP relay, the real helper binary as a
// subprocess in front of a fake daemon, and a second tsnet node as the
// remote client. Nothing reaches login.tailscale.com.
//
//	go test -tags integration -run TestTailnetEndToEnd -v ./...
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"tailscale.com/net/netns"
	"tailscale.com/tailcfg"
	"tailscale.com/tsnet"
	"tailscale.com/tstest/integration"
	"tailscale.com/tstest/integration/testcontrol"
	"tailscale.com/types/logger"
)

func startTestControl(t *testing.T, sameUser bool) string {
	netns.SetEnabled(false)
	t.Cleanup(func() { netns.SetEnabled(true) })
	derpMap := integration.RunDERPAndSTUN(t, logger.Discard, "127.0.0.1")
	control := &testcontrol.Server{
		DERPMap:          derpMap,
		DNSConfig:        &tailcfg.DNSConfig{Proxied: true},
		MagicDNSDomain:   "tail-scale.ts.net",
		AllNodesSameUser: sameUser,
		Logf:             logger.Discard,
	}
	control.HTTPTestServer = httptest.NewUnstartedServer(control)
	control.HTTPTestServer.Start()
	t.Cleanup(control.HTTPTestServer.Close)
	return control.HTTPTestServer.URL
}

// fakeDaemon echoes the forwarding headers it got, and streams on /stream.
func fakeDaemon(t *testing.T) *httptest.Server {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/stream" {
			w.Header().Set("Content-Type", "application/octet-stream")
			for i := 0; i < 5; i++ {
				fmt.Fprintf(w, "chunk-%d\n", i)
				w.(http.Flusher).Flush()
				time.Sleep(300 * time.Millisecond)
			}
			return
		}
		json.NewEncoder(w).Encode(map[string]string{
			"path":  r.URL.RequestURI(),
			"proto": r.Header.Get("X-Forwarded-Proto"),
			"for":   r.Header.Get("X-Forwarded-For"),
			"host":  r.Header.Get("X-Forwarded-Host"),
			"via":   r.Header.Get("X-Iphone-Use-Via"),
			"user":  r.Header.Get("X-Iphone-Use-Tailnet-User"),
			"real":  r.Header.Get("X-Real-Ip"),
			"rhost": r.Host,
		})
	}))
	t.Cleanup(srv.Close)
	return srv
}

func buildHelper(t *testing.T) string {
	bin := filepath.Join(t.TempDir(), "iphone-use-tailnet")
	out, err := exec.Command("go", "build", "-o", bin, ".").CombinedOutput()
	if err != nil {
		t.Fatalf("build: %v\n%s", err, out)
	}
	return bin
}

// runHelper starts the helper and waits until its status says running.
func runHelper(t *testing.T, bin, controlURL, upstream string, extra ...string) statusDoc {
	dir := t.TempDir()
	statusFile := filepath.Join(dir, "status.json")
	args := append([]string{"serve", "--state-dir", filepath.Join(dir, "ts"), "--hostname", "iphone-use-e2e",
		"--upstream", upstream, "--status-file", statusFile, "--control-url", controlURL, "--plain-port", "44321"}, extra...)
	cmd := exec.Command(bin, args...)
	cmd.Env = append(os.Environ(), "TS_NO_LOGS_NO_SUPPORT=true")
	cmd.Stderr = io.Discard
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		cmd.Process.Signal(os.Interrupt)
		cmd.Wait()
	})
	deadline := time.Now().Add(60 * time.Second)
	for time.Now().Before(deadline) {
		var d statusDoc
		if b, err := os.ReadFile(statusFile); err == nil && json.Unmarshal(b, &d) == nil &&
			d.State == "running" && len(d.TailnetIPs) > 0 {
			return d
		}
		time.Sleep(200 * time.Millisecond)
	}
	b, _ := os.ReadFile(statusFile)
	t.Fatalf("helper never reached running: %s", b)
	return statusDoc{}
}

func tailnetClient(t *testing.T, controlURL string) *http.Client {
	c := &tsnet.Server{Dir: t.TempDir(), Hostname: "remote-phone", ControlURL: controlURL, Ephemeral: true, Logf: logger.Discard}
	t.Cleanup(func() { c.Close() })
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	if _, err := c.Up(ctx); err != nil {
		t.Fatal(err)
	}
	hc := c.HTTPClient()
	hc.Timeout = 30 * time.Second
	return hc
}

func ipv4(d statusDoc) string {
	for _, ip := range d.TailnetIPs {
		if !strings.Contains(ip, ":") {
			return ip
		}
	}
	return ""
}

func getJSON(t *testing.T, hc *http.Client, url string, spoof bool) (int, map[string]string) {
	req, _ := http.NewRequest("GET", url, nil)
	if spoof {
		req.Header.Set("X-Forwarded-For", "6.6.6.6")
		req.Header.Set("X-Real-IP", "6.6.6.6")
		req.Header.Set("X-Iphone-Use-Tailnet-User", "attacker@example.com")
	}
	var resp *http.Response
	var err error
	// The first packets may wait for the peer's netmap to arrive.
	for i := 0; i < 20; i++ {
		if resp, err = hc.Do(req); err == nil {
			break
		}
		time.Sleep(500 * time.Millisecond)
	}
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	var m map[string]string
	json.NewDecoder(resp.Body).Decode(&m)
	return resp.StatusCode, m
}

func TestTailnetEndToEnd(t *testing.T) {
	bin := buildHelper(t)

	t.Run("owner's own device is proxied, headers are ours, streams flush", func(t *testing.T) {
		controlURL := startTestControl(t, true)
		daemon := fakeDaemon(t)
		st := runHelper(t, bin, controlURL, daemon.URL)
		hc := tailnetClient(t, controlURL)
		base := "http://" + ipv4(st) + ":44321"
		code, m := getJSON(t, hc, base+"/agent/status?x=1", true)
		if code != 200 {
			t.Fatalf("status %d %v", code, m)
		}
		if m["path"] != "/agent/status?x=1" || m["proto"] != "http" || m["via"] != "tailnet" ||
			m["for"] == "6.6.6.6" || m["real"] != "" || m["user"] == "attacker@example.com" ||
			!strings.HasPrefix(m["for"], "100.") || m["user"] == "" ||
			// The daemon builds its pairing links from Host: it must be the
			// tailnet address the client used, not the upstream's loopback.
			m["rhost"] != ipv4(st)+":44321" {
			t.Fatalf("forwarded headers wrong: %v", m)
		}
		t.Logf("proxied with %v", m)

		// The fake daemon takes 1.5 s to send all five chunks; a buffering
		// proxy would deliver the first only at the end.
		start := time.Now()
		resp, err := hc.Get(base + "/stream")
		if err != nil {
			t.Fatal(err)
		}
		defer resp.Body.Close()
		buf := make([]byte, 64)
		n, _ := resp.Body.Read(buf)
		first := time.Since(start)
		rest, _ := io.ReadAll(resp.Body)
		if !strings.HasPrefix(string(buf[:n]), "chunk-0") || first > 800*time.Millisecond {
			t.Fatalf("stream was buffered: first chunk %q after %v", buf[:n], first)
		}
		if !strings.Contains(string(rest), "chunk-4") {
			t.Fatalf("stream incomplete: %q", rest)
		}
		t.Logf("first stream chunk after %v", first)
	})

	t.Run("another tailnet user is refused unless members are allowed", func(t *testing.T) {
		controlURL := startTestControl(t, false)
		daemon := fakeDaemon(t)
		st := runHelper(t, bin, controlURL, daemon.URL)
		hc := tailnetClient(t, controlURL)
		code, m := getJSON(t, hc, "http://"+ipv4(st)+":44321/agent/status", false)
		if code != 403 || m["error"] != "tailnet_forbidden" {
			t.Fatalf("want 403 tailnet_forbidden, got %d %v", code, m)
		}

		st2 := runHelper(t, bin, controlURL, daemon.URL, "--allow-tailnet-members")
		code, m = getJSON(t, hc, "http://"+ipv4(st2)+":44321/agent/status", false)
		if code != 200 || m["via"] != "tailnet" {
			t.Fatalf("members allowed: want 200, got %d %v", code, m)
		}
	})
}
