// The cloud relay. A phone's on-phone daemon keeps one WebSocket open to
// `/t/<id>` (its "tunnel"); anyone may send HTTP requests to `/p/<id>/<path>`,
// or to `https://<id>.<relay host>/<path>`, which this relay forwards over that
// tunnel and streams the answer back. The daemon's own authentication (pairing
// code, session, device token) still decides who may do what: the relay only
// routes.
//
// One Durable Object per phone id. The tunnel uses the hibernation API, so an
// idle phone costs no duration; pings are answered by the runtime without
// waking the object.
//
// Frames on the tunnel:
//   relay → phone  text  {"t":"req","id":N,"m":"GET","p":"/agent/status","h":[[k,v]],"b":"<base64>"}
//                  text  {"t":"cancel","id":N}                 stop answering N (client gone, or timed out)
//                  text  {"t":"ack","id":N,"n":BYTES}         the client took BYTES of the body
//   phone → relay  text  {"t":"res","id":N,"s":200,"h":[[k,v]]}
//                  binary  4-byte big-endian id, then body bytes
//                  text  {"t":"end","id":N}
//
// N is a random non-zero 32-bit integer, never one that is in flight or was
// cancelled recently, so an object that hibernated and forgot its counter can
// not hand a new request the id of one the phone is still answering.
//
// Headers the relay sets on every forwarded request (any the client sent with
// these names are dropped first, so the client cannot pick them):
//   x-forwarded-proto   https
//   x-forwarded-host    the host the client used (`<relay>` or `<id>.<relay>`)
//   x-forwarded-prefix  `/p/<id>` for the path form, `/` for the host form:
//                       the daemon scopes its cookie Path and links to it
//   x-forwarded-for     the client's address

export interface Env {
  PHONES: DurableObjectNamespace;
}

/// A phone id in a path: `/p/<id>`, `/t/<id>`.
const ID = /^[A-Za-z0-9_-]{22,64}$/;
/// A phone id as the first label of the host: `<id>.<relay host>`. Hostnames
/// are case-folded and may not contain `_`, so only the daemon's hex ids (32
/// lowercase hex characters) can be reached this way; older base64url ids use
/// the path form.
const HOST_ID = /^[0-9a-f]{32}$/;
const MAX_REQUEST_BODY = 1 << 20;
/// Requests the phone has not started answering yet (the 30 s window).
const MAX_PENDING = 32;
/// Everything in flight, long-lived answers (a live picture) included.
const MAX_OPEN = 96;
const RESPONSE_TIMEOUT_MS = 30_000;
/// Ids cancelled or timed out that the phone may still send frames for.
const MAX_DEAD = 1024;
const HOP_BY_HOP = new Set([
  "connection", "keep-alive", "transfer-encoding", "upgrade", "host",
  "proxy-authorization", "te", "trailer", "content-length",
]);

type Route = { id: string; kind: "t" | "p"; path: string; prefix: string };

/// Where a request goes: the path form `/t/<id>…` and `/p/<id>…` first, then
/// the host form `<id>.<relay host>` (always a request to the phone).
export function route(url: URL): Route | { error: Response } | null {
  const match = url.pathname.match(/^\/(t|p)\/([^/]+)(\/.*)?$/);
  if (match) {
    const id = match[2];
    if (!ID.test(id)) return { error: new Response("bad phone id\n", { status: 400 }) };
    return { id, kind: match[1] as "t" | "p", path: match[3] ?? "/", prefix: `/p/${id}` };
  }
  const labels = url.hostname.split(".");
  if (labels.length >= 3 && HOST_ID.test(labels[0])) {
    return { id: labels[0], kind: "p", path: url.pathname, prefix: "/" };
  }
  return null;
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const target = route(url);
    if (!target) {
      return url.pathname === "/" ? new Response("iphone-use relay\n") : new Response("not found\n", { status: 404 });
    }
    if ("error" in target) return target.error;
    return env.PHONES.get(env.PHONES.idFromName(target.id)).fetch(request);
  },
};

type Pending = {
  /// The tunnel the request went out on.
  tunnel: WebSocket;
  writer: WritableStreamDefaultWriter<Uint8Array> | null;
  resolve: (response: Response) => void;
  timer: ReturnType<typeof setTimeout>;
};

export class Phone implements DurableObject {
  private pending = new Map<number, Pending>();
  /// Ids the phone was told to stop answering; frames for them are dropped
  /// until it says `end` (or they age out).
  private dead = new Set<number>();

  constructor(private state: DurableObjectState, _env: Env) {
    state.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const target = route(url);
    if (!target || "error" in target) return new Response("bad route\n", { status: 400 });
    return target.kind === "t" ? this.openTunnel(request) : this.forward(request, url, target);
  }

  /// The phone connects. The first key it presents is kept (as a hash);
  /// later connections must present the same key.
  private async openTunnel(request: Request): Promise<Response> {
    if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
      return new Response("websocket expected\n", { status: 426 });
    }
    const key = (request.headers.get("authorization") ?? "").replace(/^Bearer\s+/i, "");
    if (key.length < 32 || key.length > 256) return new Response("phone key required\n", { status: 401 });
    const hash = await sha256(key);
    const known = await this.state.storage.get<string>("key");
    if (known && known !== hash) return new Response("wrong phone key\n", { status: 403 });
    if (!known) await this.state.storage.put("key", hash);
    for (const old of this.state.getWebSockets()) old.close(4000, "replaced by a newer tunnel");
    const [client, server] = Object.values(new WebSocketPair());
    this.state.acceptWebSocket(server);
    return new Response(null, { status: 101, webSocket: client });
  }

  private async forward(request: Request, url: URL, target: Route): Promise<Response> {
    const tunnel = this.state.getWebSockets()[0];
    if (!tunnel) return new Response("phone offline\n", { status: 502, headers: { "x-relay-error": "phone_offline" } });
    let waiting = 0;
    for (const entry of this.pending.values()) if (!entry.writer) waiting++;
    if (waiting >= MAX_PENDING || this.pending.size >= MAX_OPEN) {
      return new Response("too many requests\n", { status: 503 });
    }
    const declared = Number(request.headers.get("content-length"));
    if (declared > MAX_REQUEST_BODY) return new Response("request too large\n", { status: 413 });
    const body = request.body ? new Uint8Array(await request.arrayBuffer()) : new Uint8Array();
    if (body.length > MAX_REQUEST_BODY) return new Response("request too large\n", { status: 413 });
    const headers: [string, string][] = [];
    request.headers.forEach((value, name) => {
      if (HOP_BY_HOP.has(name) || name.startsWith("cf-") || name.startsWith("x-forwarded-")) return;
      headers.push([name, value]);
    });
    headers.push(["x-forwarded-proto", "https"], ["x-forwarded-host", url.host], ["x-forwarded-prefix", target.prefix]);
    const from = request.headers.get("cf-connecting-ip");
    if (from) headers.push(["x-forwarded-for", from]);
    const id = this.freshId();
    const response = new Promise<Response>((resolve) => {
      const timer = setTimeout(() => {
        // Tell the phone to stop before the client is told it did not answer,
        // or a late answer would be work for nobody.
        this.finish(id);
        resolve(new Response("phone did not answer\n", { status: 504 }));
      }, RESPONSE_TIMEOUT_MS);
      this.pending.set(id, { tunnel, writer: null, resolve, timer });
    });
    request.signal?.addEventListener("abort", () => this.finish(id));
    try {
      tunnel.send(JSON.stringify({ t: "req", id, m: request.method, p: target.path + url.search, h: headers, b: base64(body) }));
    } catch {
      // The socket is closing under us (the phone is reconnecting): fail now
      // rather than sit in `pending` until the timer does.
      this.finish(id);
      return new Response("phone offline\n", { status: 502, headers: { "x-relay-error": "phone_offline" } });
    }
    return response;
  }

  /// A random non-zero 32-bit id that is neither in flight nor recently dead.
  private freshId(): number {
    const word = new Uint32Array(1);
    for (;;) {
      crypto.getRandomValues(word);
      const id = word[0];
      if (id !== 0 && !this.pending.has(id) && !this.dead.has(id)) return id;
    }
  }

  /// Stop a request before the phone is done with it: forget it, close what
  /// the client was reading, and tell the phone. Frames the phone still sends
  /// for the id are dropped (see `dead`).
  private finish(id: number) {
    const entry = this.pending.get(id);
    if (!entry) return;
    clearTimeout(entry.timer);
    this.pending.delete(id);
    this.remember(id);
    entry.writer?.close().catch(() => {});
    try { entry.tunnel.send(JSON.stringify({ t: "cancel", id })); } catch {}
  }

  private remember(id: number) {
    if (this.dead.size >= MAX_DEAD) {
      const oldest = this.dead.values().next().value;
      if (oldest !== undefined) this.dead.delete(oldest);
    }
    this.dead.add(id);
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer) {
    if (typeof message !== "string") {
      const bytes = new Uint8Array(message);
      if (bytes.length < 4) return;
      const id = new DataView(message).getUint32(0);
      const entry = this.pending.get(id);
      if (!entry?.writer) return;
      const length = bytes.length - 4;
      // Ack once the client has taken the chunk: the phone keeps a window of
      // unacked bytes, so a slow viewer slows (or thins) what the phone sends
      // instead of piling it up here.
      entry.writer.write(bytes.subarray(4)).then(
        () => { try { ws.send(JSON.stringify({ t: "ack", id, n: length })); } catch {} },
        () => this.finish(id),
      );
      return;
    }
    let frame: { t?: string; id?: number; s?: number; h?: [string, string][] };
    try { frame = JSON.parse(message); } catch { return; }
    if (typeof frame.id !== "number") return;
    const entry = this.pending.get(frame.id);
    if (!entry) {
      // The phone finished (or acknowledged the cancel of) a dead request.
      if (frame.t === "end") this.dead.delete(frame.id);
      return;
    }
    if (frame.t === "res") {
      if (entry.writer) return; // a second `res` for the same request: ignore it
      clearTimeout(entry.timer);
      const { readable, writable } = new TransformStream<Uint8Array, Uint8Array>();
      entry.writer = writable.getWriter();
      const headers = new Headers();
      for (const [name, value] of frame.h ?? []) {
        if (!HOP_BY_HOP.has(name.toLowerCase())) headers.append(name, value);
      }
      const status = frame.s ?? 502;
      const nullBody = status === 101 || status === 204 || status === 304;
      entry.resolve(new Response(nullBody ? null : readable, { status, headers }));
      if (nullBody) entry.writer.close().catch(() => {});
    } else if (frame.t === "end") {
      this.pending.delete(frame.id);
      clearTimeout(entry.timer);
      entry.writer?.close().catch(() => {});
    }
  }

  async webSocketClose(ws: WebSocket) {
    this.failAll(ws);
    ws.close();
  }

  async webSocketError(ws: WebSocket) {
    this.failAll(ws);
  }

  /// The tunnel `ws` is gone: fail what was sent on it. Requests already out
  /// on a newer tunnel (the phone reconnected) are left alone.
  private failAll(ws: WebSocket) {
    for (const [id, entry] of this.pending) {
      if (entry.tunnel !== ws) continue;
      clearTimeout(entry.timer);
      if (entry.writer) entry.writer.abort("phone disconnected").catch(() => {});
      else entry.resolve(new Response("phone disconnected\n", { status: 502 }));
      this.pending.delete(id);
    }
  }
}

async function sha256(text: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

function base64(bytes: Uint8Array): string {
  let binary = "";
  for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(binary);
}
