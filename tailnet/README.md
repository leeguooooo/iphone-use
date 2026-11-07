# iphone-use-tailnet

Gives the iphone-use daemon its own Tailscale node. It embeds
[tsnet](https://pkg.go.dev/tailscale.com/tsnet), so no system Tailscale app is
needed: the node joins the user's tailnet as a separate device and
reverse-proxies tailnet requests to the daemon on `127.0.0.1`.

The Rust daemon spawns and supervises it; it is not meant to be run by hand.

## CLI

```
iphone-use-tailnet serve --state-dir DIR --hostname NAME --upstream http://127.0.0.1:44321 --status-file PATH
                         [--allow-tailnet-members] [--auth-key-file FILE] [--control-url URL] [--plain-port 44321]
iphone-use-tailnet logout --state-dir DIR [--control-url URL]
iphone-use-tailnet --version
```

`serve` listens on the tailnet (never on the Mac's own interfaces):

| Port | What |
|---|---|
| `:443` | HTTPS with the node's `*.ts.net` certificate (needs HTTPS certificates enabled for the tailnet) |
| `:<plain-port>` | plain HTTP (default 44321); works without HTTPS certificates |
| `:80` | redirects to `https://<dns-name><path>` |

Access: each request is resolved with WhoIs. By default only the Tailscale
user that owns this node (the user's own devices) gets through; with
`--allow-tailnet-members` any tailnet member does. A tagged node has no owning
user, so it refuses everyone unless `--allow-tailnet-members` is set. Refusals
are `403 {"error":"tailnet_forbidden",...}`; an unreachable daemon is
`502 {"error":"daemon_unreachable",...}`.

The daemon receives `X-Forwarded-Proto`, `X-Forwarded-Host`,
`X-Forwarded-For` (caller's tailnet IP), `X-Iphone-Use-Via: tailnet` and
`X-Iphone-Use-Tailnet-User` (caller's login name). Client-supplied copies of
these, `X-Real-IP`, `CF-Connecting-IP`, `Forwarded` and any `X-Iphone-Use-*`
header are removed first. Responses are streamed unbuffered.

Signals: `SIGUSR1` asks for a fresh login link; `SIGTERM`/`SIGINT` write state
`stopped` and exit 0.

The auth key is only read from `--auth-key-file`, never from argv. tsnet's own
logs go to `<state-dir>/tsnet.log`, truncated at each start.

## Status file

Written atomically (mode 0600) on every state change and at least every 5 s:

```json
{
  "version": 1,
  "pid": 123,
  "state": "starting | needs_login | needs_machine_auth | running | error | stopped",
  "backend_state": "NeedsLogin",
  "login_url": "https://login.tailscale.com/a/…",
  "dns_name": "iphone-use.tail1eab92.ts.net",
  "tailnet_ips": ["100.x.y.z", "fd7a:…"],
  "https": true,
  "magic_dns": true,
  "owner": "user@example.com",
  "allow_tailnet_members": false,
  "hint": null,
  "error": null,
  "updated_at": "2026-11-07T04:16:20Z"
}
```

`dns_name` is the name Tailscale actually assigned (it appends `-1`, `-2`, …
when the hostname is taken). `hint` explains what the user has to do: sign in,
approve the machine, enable HTTPS certificates or MagicDNS, or turn on
`--allow-tailnet-members` for a tagged node.

## Build

```
VERSION=0.29.0 tailnet/build.sh        # -> tailnet/dist/iphone-use-tailnet (arm64 + x86_64)
cd tailnet && go vet ./... && go test ./...
```

## Tests

`go test ./...` covers the header, access and status logic. `go test -tags integration
./...` runs the real binary on a private tailnet (Tailscale's in-process test control
server and DERP, no network beyond loopback): the owner's own device is proxied with
our forwarding headers and an unbuffered stream, another user gets 403, and
`--allow-tailnet-members` lets them in.
