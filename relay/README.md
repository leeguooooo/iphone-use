# iphone-use relay

Control a phone from anywhere without a Mac or an open port. The phone's
on-phone daemon keeps one outgoing WebSocket to this relay. Another phone, or a
browser, sends its requests to the relay, and the relay passes them along that
WebSocket.

```
controller ──https──▶ relay /p/<id>/…  ──WebSocket──▶ daemon on the phone
```

The relay only routes. Every request still has to pass the daemon's own
sign-in: a pairing code, a session or a device token. A phone's `<id>` is
random. The first key a phone presents for that id is pinned, so nobody else
can take the id over.

The relay runs on Cloudflare Workers, with one Durable Object per phone.

## Deploy your own

You need a Cloudflare account. The free plan is enough to try the relay;
Durable Objects on SQLite are available there.

```bash
cd relay
npm install
npx wrangler login
# Use your own domain, or delete the `routes` line to get <name>.<you>.workers.dev
$EDITOR wrangler.toml
npx wrangler deploy
curl https://<your relay>/      # → iphone-use relay
```

Then connect a phone through it. The `--relay` value is the controlled phone's
address on the relay; `--connect-to` names the phone that does the
controlling:

```bash
iphone-use setup-app --instance <controlled phone> --connect-to <controlling phone> \
  --connect-only --relay https://<your relay>
```

For a daemon on a Mac, set `IPHONE_USE_RELAY_URL=https://<your relay>` in its
environment. It then logs `relay tunnel open: https://<your relay>/p/0123…`;
the id is cut short there because whoever has it can reach the phone's sign-in
page. The full address is in `relay-identity.json` in the daemon's state
directory (`"id"`), or in the log at `RUST_LOG=server::relay_client=debug`.

## One origin per phone (optional)

The path form `https://<relay>/p/<id>/…` puts every phone on one origin, so
browsers share cookies and storage between phones. The relay also answers the
host form `https://<id>.<relay>/…`, which gives each phone its own origin. The
daemon learns which form was used from `x-forwarded-prefix` (`/p/<id>` or `/`)
and scopes its cookie `Path` to it.

To turn the host form on for your deployment:

1. Add a wildcard DNS record for the relay's host, proxied through Cloudflare:
   `*.iphone-use-relay.example.com` (CNAME to the relay's host, or AAAA `100::`).
2. Add a zone route for it in `wrangler.toml` next to the existing one:
   `{ pattern = "*.iphone-use-relay.example.com/*", zone_name = "example.com" }`
   (a wildcard is a route, not a `custom_domain`).
3. Certificates: Cloudflare's Universal SSL only covers one wildcard level at
   the zone apex (`*.example.com`). A relay at `relay.example.com` needs a
   certificate for `*.relay.example.com`: Advanced Certificate Manager or
   Total TLS. A relay at the zone apex, or on `<name>.workers.dev`, does not
   get a second-level wildcard either, so give it its own hostname.
4. `npx wrangler deploy`, then `curl https://<id>.<relay>/agent/status`.

Only an id of 32 lowercase hex characters fits in a hostname. Daemons make
such ids since this form exists; a phone with an older base64url id keeps
working on the path form and keeps its id.

## Cost

A Worker's outbound traffic is free. The relay uses WebSocket hibernation, so
a phone that is connected but idle bills no duration. The relay answers the
phone's pings without waking its object. While someone is watching:

- one hour of live picture at about 10 frames a second is roughly 36,000
  WebSocket messages, billed at 20:1, so about 1,800 requests;
- it also runs about 450 GB·s of Durable Object duration;
- the paid plan includes 1M requests and 400k GB·s a month, which is about
  890 hours of watching.

Check the current prices on Cloudflare's pricing page.

## Limits

- Request bodies are capped at 1 MB (checked against `Content-Length` before
  the body is read). Each phone can have 32 requests waiting for its first
  byte and 96 in flight in all, long-lived answers (a live picture) included.
- A phone has 30 s to start answering a request. When that passes, or the
  client goes away, the relay tells the phone to stop (`cancel`) and drops
  anything the phone still sends for that request.
- Request ids are random; one never names a request that is in flight or was
  cancelled a moment ago, even after the phone's object was evicted and
  restarted.
- The relay sets `x-forwarded-proto`, `x-forwarded-host`, `x-forwarded-prefix`
  and `x-forwarded-for` itself and drops any the client sent.
- TLS ends at Cloudflare, so the relay can see the traffic it passes along.
  Run your own relay if that matters to you.
- The daemon's web pages assume they are served from `/`, so use the native
  app through a relay, not the browser page.
