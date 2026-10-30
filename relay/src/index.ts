// The cloud relay. A phone's on-phone daemon keeps one WebSocket open to
// `/t/<id>` (its "tunnel"); anyone may send HTTP requests to `/p/<id>/<path>`,
// which this relay forwards over that tunnel and streams the answer back.
// The daemon's own authentication (pairing code, session, device token) still
// decides who may do what: the relay only routes.
//
// One Durable Object per phone id. The tunnel uses the hibernation API, so an
// idle phone costs no duration; pings are answered by the runtime without
// waking the object.
//
// Frames on the tunnel:
//   relay → phone  text  {"t":"req","id":N,"m":"GET","p":"/agent/status","h":[[k,v]],"b":"<base64>"}
//                  text  {"t":"cancel","id":N}
//   phone → relay  text  {"t":"res","id":N,"s":200,"h":[[k,v]]}
//                  binary  4-byte big-endian id, then body bytes
//                  text  {"t":"end","id":N}

export interface Env {
  PHONES: DurableObjectNamespace;
}

const ID = /^[A-Za-z0-9_-]{22,64}$/;
const MAX_REQUEST_BODY = 1 << 20;
const MAX_PENDING = 32;
const RESPONSE_TIMEOUT_MS = 30_000;
const HOP_BY_HOP = new Set([
  "connection", "keep-alive", "transfer-encoding", "upgrade", "host",
  "proxy-authorization", "te", "trailer", "content-length",
]);

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const match = url.pathname.match(/^\/(t|p)\/([^/]+)(\/.*)?$/);
    if (!match) {
      return url.pathname === "/" ? new Response("iphone-use relay\n") : new Response("not found\n", { status: 404 });
    }
    const id = match[2];
    if (!ID.test(id)) return new Response("bad phone id\n", { status: 400 });
    return env.PHONES.get(env.PHONES.idFromName(id)).fetch(request);
  },
};

type Pending = {
  writer: WritableStreamDefaultWriter<Uint8Array> | null;
  resolve: (response: Response) => void;
  timer: ReturnType<typeof setTimeout>;
};

export class Phone implements DurableObject {
  private pending = new Map<number, Pending>();
  private nextId = 1;

  constructor(private state: DurableObjectState, _env: Env) {
    state.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const [, kind, , rest] = url.pathname.match(/^\/(t|p)\/([^/]+)(\/.*)?$/)!;
    return kind === "t" ? this.openTunnel(request) : this.forward(request, url, rest ?? "/");
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

  private async forward(request: Request, url: URL, path: string): Promise<Response> {
    const tunnel = this.state.getWebSockets()[0];
    if (!tunnel) return new Response("phone offline\n", { status: 502, headers: { "x-relay-error": "phone_offline" } });
    if (this.pending.size >= MAX_PENDING) return new Response("too many requests\n", { status: 503 });
    const body = request.body ? new Uint8Array(await request.arrayBuffer()) : new Uint8Array();
    if (body.length > MAX_REQUEST_BODY) return new Response("request too large\n", { status: 413 });
    const headers: [string, string][] = [];
    request.headers.forEach((value, name) => {
      if (!HOP_BY_HOP.has(name) && !name.startsWith("cf-")) headers.push([name, value]);
    });
    headers.push(["x-forwarded-proto", "https"], ["x-forwarded-host", url.host]);
    const id = this.nextId++;
    const response = new Promise<Response>((resolve) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        resolve(new Response("phone did not answer\n", { status: 504 }));
      }, RESPONSE_TIMEOUT_MS);
      this.pending.set(id, { writer: null, resolve, timer });
    });
    request.signal?.addEventListener("abort", () => this.cancel(id));
    tunnel.send(JSON.stringify({ t: "req", id, m: request.method, p: path + url.search, h: headers, b: base64(body) }));
    return response;
  }

  private cancel(id: number) {
    const entry = this.pending.get(id);
    if (!entry) return;
    clearTimeout(entry.timer);
    this.pending.delete(id);
    entry.writer?.close().catch(() => {});
    this.state.getWebSockets()[0]?.send(JSON.stringify({ t: "cancel", id }));
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer) {
    if (typeof message !== "string") {
      const bytes = new Uint8Array(message);
      if (bytes.length < 4) return;
      const id = new DataView(message).getUint32(0);
      const entry = this.pending.get(id);
      if (!entry?.writer) return;
      entry.writer.write(bytes.subarray(4)).catch(() => this.cancel(id));
      return;
    }
    let frame: { t?: string; id?: number; s?: number; h?: [string, string][] };
    try { frame = JSON.parse(message); } catch { return; }
    const entry = frame.id === undefined ? undefined : this.pending.get(frame.id);
    if (!entry) return;
    if (frame.t === "res") {
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
      this.pending.delete(frame.id!);
      clearTimeout(entry.timer);
      entry.writer?.close().catch(() => {});
    }
  }

  async webSocketClose(ws: WebSocket) {
    this.failAll();
    ws.close();
  }

  async webSocketError() {
    this.failAll();
  }

  private failAll() {
    for (const [id, entry] of this.pending) {
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
