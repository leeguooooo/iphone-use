package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"time"

	"tailscale.com/ipn/ipnstate"
	"tailscale.com/tailcfg"
)

// Status-file states. The daemon reads these; keep them stable.
const (
	stateStarting         = "starting"
	stateNeedsLogin       = "needs_login"
	stateNeedsMachineAuth = "needs_machine_auth"
	stateRunning          = "running"
	stateError            = "error"
	stateStopped          = "stopped"
)

const (
	hintLogin       = "open login_url and sign in to add this iPhone to your tailnet"
	hintMachineAuth = "approve this device in the Tailscale admin console (Machines)"
	hintHTTPS       = "enable HTTPS certificates in the Tailscale admin console: DNS → HTTPS Certificates"
	hintMagicDNS    = "enable MagicDNS in the Tailscale admin console: DNS → MagicDNS"
	hintTagged      = "this node is tagged, so it has no owning user; owner-only access cannot work — turn on allow-tailnet-members to let tailnet members in"
)

// statusDoc is the JSON written to --status-file.
type statusDoc struct {
	Version             int      `json:"version"`
	PID                 int      `json:"pid"`
	State               string   `json:"state"`
	BackendState        string   `json:"backend_state"`
	LoginURL            *string  `json:"login_url"`
	DNSName             *string  `json:"dns_name"`
	TailnetIPs          []string `json:"tailnet_ips"`
	HTTPS               bool     `json:"https"`
	MagicDNS            bool     `json:"magic_dns"`
	Owner               *string  `json:"owner"`
	AllowTailnetMembers bool     `json:"allow_tailnet_members"`
	Hint                *string  `json:"hint"`
	Error               *string  `json:"error"`
	UpdatedAt           string   `json:"updated_at"`
}

// nodeInfo is what the request path needs to know about this node.
type nodeInfo struct {
	Running bool
	DNSName string // without trailing dot
	OwnerID tailcfg.UserID
	Tagged  bool
}

func strPtr(s string) *string {
	if s == "" {
		return nil
	}
	return &s
}

func mapBackendState(s string) string {
	switch s {
	case "NeedsLogin":
		return stateNeedsLogin
	case "NeedsMachineAuth":
		return stateNeedsMachineAuth
	case "Running":
		return stateRunning
	default: // NoState, Starting, Stopped, InUseOtherUser
		return stateStarting
	}
}

// nodeFromStatus extracts the access-control view of the node.
func nodeFromStatus(st *ipnstate.Status) nodeInfo {
	if st == nil || st.Self == nil {
		return nodeInfo{}
	}
	return nodeInfo{
		Running: st.BackendState == "Running",
		DNSName: strings.TrimSuffix(st.Self.DNSName, "."),
		OwnerID: st.Self.UserID,
		Tagged:  st.Self.IsTagged(),
	}
}

// buildStatus turns a LocalClient status into the status-file document.
// st may be nil (not started yet); errMsg forces state "error".
func buildStatus(st *ipnstate.Status, allowMembers bool, errMsg string, now time.Time) statusDoc {
	d := statusDoc{
		Version:             1,
		PID:                 os.Getpid(),
		State:               stateStarting,
		TailnetIPs:          []string{},
		AllowTailnetMembers: allowMembers,
		UpdatedAt:           now.UTC().Format(time.RFC3339),
	}
	var hints []string
	if st != nil {
		d.BackendState = st.BackendState
		d.State = mapBackendState(st.BackendState)
		d.LoginURL = strPtr(st.AuthURL)
		for _, ip := range st.TailscaleIPs {
			d.TailnetIPs = append(d.TailnetIPs, ip.String())
		}
		d.HTTPS = len(st.CertDomains) > 0
		if st.CurrentTailnet != nil {
			d.MagicDNS = st.CurrentTailnet.MagicDNSEnabled
		}
		n := nodeFromStatus(st)
		d.DNSName = strPtr(n.DNSName)
		if !n.Tagged && n.OwnerID != 0 {
			if u, ok := st.User[n.OwnerID]; ok {
				d.Owner = strPtr(u.LoginName)
			}
		}
		switch d.State {
		case stateNeedsLogin:
			hints = append(hints, hintLogin)
		case stateNeedsMachineAuth:
			hints = append(hints, hintMachineAuth)
		case stateRunning:
			if n.Tagged && !allowMembers {
				hints = append(hints, hintTagged)
			}
			if !d.MagicDNS {
				hints = append(hints, hintMagicDNS)
			}
			if !d.HTTPS {
				hints = append(hints, hintHTTPS)
			}
		}
	}
	if errMsg != "" {
		d.State = stateError
		d.Error = &errMsg
	}
	if len(hints) > 0 {
		d.Hint = strPtr(strings.Join(hints, "; "))
	}
	return d
}

// writeStatusFile writes the doc atomically (tmp + rename, mode 0600).
func writeStatusFile(path string, d statusDoc) error {
	b, err := json.MarshalIndent(d, "", "  ")
	if err != nil {
		return err
	}
	tmp, err := os.CreateTemp(filepath.Dir(path), "."+filepath.Base(path)+".tmp*")
	if err != nil {
		return err
	}
	defer os.Remove(tmp.Name()) // no-op after a successful rename
	if err := tmp.Chmod(0o600); err != nil {
		tmp.Close()
		return err
	}
	if _, err := tmp.Write(append(b, '\n')); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Close(); err != nil {
		return err
	}
	return os.Rename(tmp.Name(), path)
}
