// Command iphone-use-tailnet gives the iphone-use daemon its own Tailscale
// node: a userspace tsnet node that joins the user's tailnet as a separate
// device and reverse-proxies tailnet requests to the daemon on loopback.
package main

import (
	"context"
	"crypto/tls"
	"errors"
	"flag"
	"fmt"
	"log"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	"tailscale.com/client/local"
	"tailscale.com/ipn"
	"tailscale.com/tsnet"
)

var version = "dev"

const statusInterval = 5 * time.Second

func main() {
	if len(os.Args) < 2 {
		usage()
	}
	var err error
	switch os.Args[1] {
	case "--version", "-version", "version":
		fmt.Println("iphone-use-tailnet", version)
		return
	case "serve":
		err = cmdServe(os.Args[2:])
	case "logout":
		err = cmdLogout(os.Args[2:])
	default:
		usage()
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "iphone-use-tailnet:", err)
		os.Exit(1)
	}
}

func usage() {
	fmt.Fprint(os.Stderr, `usage:
  iphone-use-tailnet serve --state-dir DIR --hostname NAME --upstream URL --status-file PATH
                           [--allow-tailnet-members] [--auth-key-file FILE] [--control-url URL] [--plain-port 44321]
  iphone-use-tailnet logout --state-dir DIR [--control-url URL]
  iphone-use-tailnet --version
`)
	os.Exit(2)
}

// newServer builds the tsnet server; tsnet's chatty logs go to
// <state-dir>/tsnet.log (truncated each start), user-facing ones to stderr.
func newServer(stateDir, hostname, controlURL, authKey string) (*tsnet.Server, *os.File, error) {
	if err := os.MkdirAll(stateDir, 0o700); err != nil {
		return nil, nil, err
	}
	lf, err := os.OpenFile(filepath.Join(stateDir, "tsnet.log"), os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o600)
	if err != nil {
		return nil, nil, err
	}
	l := log.New(lf, "", log.LstdFlags)
	return &tsnet.Server{
		Dir:        stateDir,
		Hostname:   hostname,
		ControlURL: controlURL,
		AuthKey:    authKey,
		Ephemeral:  false,
		Logf:       l.Printf,
		UserLogf:   log.New(os.Stderr, "", log.LstdFlags).Printf,
	}, lf, nil
}

func readAuthKey(path string) (string, error) {
	if path == "" {
		return "", nil
	}
	b, err := os.ReadFile(path)
	if err != nil {
		return "", fmt.Errorf("auth key file: %w", err)
	}
	return strings.TrimSpace(string(b)), nil
}

func cmdServe(args []string) error {
	fs := flag.NewFlagSet("serve", flag.ExitOnError)
	stateDir := fs.String("state-dir", "", "tsnet state directory")
	hostname := fs.String("hostname", "iphone-use", "tailnet hostname")
	upstreamStr := fs.String("upstream", "", "daemon URL, e.g. http://127.0.0.1:44321")
	statusFile := fs.String("status-file", "", "where to write status JSON")
	allowMembers := fs.Bool("allow-tailnet-members", false, "allow any tailnet member, not just the node owner")
	authKeyFile := fs.String("auth-key-file", "", "file holding a Tailscale auth key")
	controlURL := fs.String("control-url", "", "coordination server URL (default Tailscale)")
	plainPort := fs.Int("plain-port", 44321, "plain-HTTP tailnet port")
	fs.Parse(args)
	if *stateDir == "" || *upstreamStr == "" || *statusFile == "" {
		return errors.New("serve: --state-dir, --upstream and --status-file are required")
	}
	upstream, err := url.Parse(*upstreamStr)
	if err != nil || upstream.Host == "" {
		return fmt.Errorf("serve: bad --upstream %q", *upstreamStr)
	}
	authKey, err := readAuthKey(*authKeyFile)
	if err != nil {
		return err
	}

	writeStatus := func(d statusDoc) {
		if err := writeStatusFile(*statusFile, d); err != nil {
			log.Printf("status file: %v", err)
		}
	}
	fail := func(err error) error {
		writeStatus(buildStatus(nil, *allowMembers, false, err.Error(), time.Now()))
		return err
	}
	writeStatus(buildStatus(nil, *allowMembers, false, "", time.Now()))

	srv, logFile, err := newServer(*stateDir, *hostname, *controlURL, authKey)
	if err != nil {
		return fail(err)
	}
	defer logFile.Close()
	if err := srv.Start(); err != nil {
		return fail(err)
	}
	defer srv.Close()
	lc, err := srv.LocalClient()
	if err != nil {
		return fail(err)
	}

	errLog := log.New(logFile, "http: ", log.LstdFlags)
	var node atomic.Pointer[nodeInfo]
	node.Store(&nodeInfo{})
	gw := newGateway(upstream, lc, &node, *allowMembers, errLog)

	// Listen (not ListenTLS): ListenTLS blocks until Up and refuses when
	// HTTPS certs are off. With certs off, handshakes fail; status says so.
	ln443, err := srv.Listen("tcp", ":443")
	if err != nil {
		return fail(err)
	}
	tlsLn := tls.NewListener(ln443, &tls.Config{GetCertificate: lc.GetCertificate, NextProtos: []string{"h2", "http/1.1"}})
	lnPlain, err := srv.Listen("tcp", ":"+strconv.Itoa(*plainPort))
	if err != nil {
		return fail(err)
	}
	ln80, err := srv.Listen("tcp", ":80")
	if err != nil {
		return fail(err)
	}
	serve := func(ln net.Listener, h http.Handler) {
		hs := &http.Server{Handler: h, ReadHeaderTimeout: 30 * time.Second, ErrorLog: errLog}
		if err := hs.Serve(ln); err != nil && !errors.Is(err, net.ErrClosed) {
			errLog.Printf("serve %s: %v", ln.Addr(), err)
		}
	}
	go serve(tlsLn, gw.handler("https"))
	go serve(lnPlain, gw.handler("http"))
	go serve(ln80, redirectHandler(&node))

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	kick := make(chan struct{}, 1)
	poke := func() {
		select {
		case kick <- struct{}{}:
		default:
		}
	}
	go watchBus(ctx, lc, poke)

	sigs := make(chan os.Signal, 1)
	signal.Notify(sigs, syscall.SIGTERM, syscall.SIGINT, syscall.SIGUSR1)
	tick := time.NewTicker(statusInterval)
	defer tick.Stop()
	certs := &certWarmer{}
	refresh := func() {
		sctx, scancel := context.WithTimeout(ctx, 5*time.Second)
		st, err := lc.Status(sctx)
		scancel()
		if err != nil {
			log.Printf("status: %v", err)
			return
		}
		n := nodeFromStatus(st)
		node.Store(&n)
		if n.Running && len(st.CertDomains) > 0 && n.DNSName != "" {
			certs.ensure(ctx, lc, n.DNSName, poke)
		}
		writeStatus(buildStatus(st, *allowMembers, certs.ready(n.DNSName), "", time.Now()))
	}
	refresh()
	for {
		select {
		case <-tick.C:
			refresh()
		case <-kick:
			refresh()
		case s := <-sigs:
			if s == syscall.SIGUSR1 {
				lctx, lcancel := context.WithTimeout(ctx, 15*time.Second)
				if err := lc.StartLoginInteractive(lctx); err != nil {
					log.Printf("login: %v", err)
				}
				lcancel()
				poke()
				continue
			}
			d := buildStatus(nil, *allowMembers, false, "", time.Now())
			d.State = stateStopped
			writeStatus(d)
			return nil
		}
	}
}

// watchBus pokes on every IPN state / login-URL change, reconnecting if the
// watch drops.
func watchBus(ctx context.Context, lc *local.Client, poke func()) {
	for ctx.Err() == nil {
		w, err := lc.WatchIPNBus(ctx, ipn.NotifyInitialState|ipn.NotifyNoNetMap)
		if err == nil {
			for {
				n, err := w.Next()
				if err != nil {
					break
				}
				if n.State != nil || n.BrowseToURL != nil || n.LoginFinished != nil || n.ErrMessage != nil {
					poke()
				}
			}
			w.Close()
		}
		select {
		case <-ctx.Done():
		case <-time.After(time.Second):
		}
	}
}

func cmdLogout(args []string) error {
	fs := flag.NewFlagSet("logout", flag.ExitOnError)
	stateDir := fs.String("state-dir", "", "tsnet state directory")
	controlURL := fs.String("control-url", "", "coordination server URL")
	fs.Parse(args)
	if *stateDir == "" {
		return errors.New("logout: --state-dir is required")
	}
	srv, logFile, err := newServer(*stateDir, "", *controlURL, "")
	if err != nil {
		return err
	}
	defer logFile.Close()
	srv.UserLogf = func(string, ...any) {} // don't print a login URL while logging out
	if err := srv.Start(); err != nil {
		return err
	}
	defer srv.Close()
	lc, err := srv.LocalClient()
	if err != nil {
		return err
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	err = lc.Logout(ctx)
	if err != nil {
		if st, serr := lc.Status(ctx); serr == nil && st.BackendState == "NeedsLogin" {
			return nil // already signed out
		}
	}
	return err
}

// certWarmer fetches the node's HTTPS certificate in the background (the
// first one comes from Let's Encrypt and can take a minute), retrying a
// failure after a minute.
type certWarmer struct {
	mu       sync.Mutex
	name     string
	have     bool
	inFlight bool
	retryAt  time.Time
}

func (c *certWarmer) ready(name string) bool {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.have && c.name == name
}

func (c *certWarmer) ensure(ctx context.Context, lc *local.Client, name string, done func()) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.name != name {
		c.name, c.have, c.inFlight, c.retryAt = name, false, false, time.Time{}
	}
	if c.have || c.inFlight || time.Now().Before(c.retryAt) {
		return
	}
	c.inFlight = true
	go func() {
		cctx, cancel := context.WithTimeout(ctx, 3*time.Minute)
		_, _, err := lc.CertPair(cctx, name)
		cancel()
		c.mu.Lock()
		if c.name == name {
			c.inFlight = false
			c.have = err == nil
			if err != nil {
				log.Printf("https certificate for %s: %v", name, err)
				c.retryAt = time.Now().Add(time.Minute)
			}
		}
		c.mu.Unlock()
		done()
	}()
}
