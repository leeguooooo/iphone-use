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
environment. It then logs `relay tunnel open: https://<your relay>/p/<id>`.

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

- Request bodies are capped at 1 MB, and each phone can have 32 requests in
  flight.
- A phone has 30 s to start answering a request.
- TLS ends at Cloudflare, so the relay can see the traffic it passes along.
  Run your own relay if that matters to you.
- The daemon's web pages assume they are served from `/`, so use the native
  app through a relay, not the browser page.
