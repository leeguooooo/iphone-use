// iphone-use promo: the agent's real tool calls beside the real iPhone recordings they caused.
window.drawFrame = function (c, t, film, view, M) {
  const { width: W, height: H, vertical: V } = view,
    u = M.unit,
    p = film.copy,
    k = M.palette(),
    d = film.data,
    img = (name) => window.filmAssets[name];
  const amber = "#FF9F0A",
    red = "#FF453A";
  c.fillStyle = k.bg;
  c.fillRect(0, 0, W, H);

  // Layout: phone on the left and terminal on the right; portrait stacks them.
  // Portrait keeps everything inside view.safe (platform UI covers the top, bottom and right).
  const PH = V ? H * 0.27 : H * 0.74,
    PW = PH * 0.46,
    home = V ? [W * 0.46, H * 0.255] : [W * 0.245, H * 0.45];
  const pane = V ? { x: W * 0.06, y: H * 0.47, w: W * 0.8, h: H * 0.225 } : { x: W * 0.43, y: H * 0.25, w: W * 0.52, h: H * 0.56 };
  const headAt = V ? [pane.x, H * 0.43] : [pane.x, H * 0.165];
  const fs = (V ? 25 : 29) * u,
    lh = fs * 1.6;

  function phone(screen, dim = 0) {
    const [x, y] = home;
    c.save();
    c.shadowColor = "#000";
    c.shadowBlur = 60 * u;
    c.fillStyle = "#050607";
    c.beginPath();
    c.roundRect(x - PW / 2 - 14 * u, y - PH / 2 - 14 * u, PW + 28 * u, PH + 28 * u, 66 * u);
    c.fill();
    c.restore();
    if (screen) M.cover(c, screen, { x: x - PW / 2, y: y - PH / 2, w: PW, h: PH, radius: 54 * u });
    if (dim > 0) M.round(c, x - PW / 2, y - PH / 2, PW, PH, 54 * u, `rgba(11,13,16,${0.62 * dim})`);
  }
  // Terminal: a prompt line types itself, its response lands complete.
  function term(lines, from, until) {
    if (t < from || t > until + 0.2) return;
    c.save();
    c.globalAlpha = 1 - M.tween(t, until, 0.2);
    M.round(c, pane.x, pane.y, pane.w, pane.h, 22 * u, k.surface);
    [red, amber, k.accent2].forEach((col, i) => {
      c.fillStyle = col;
      c.beginPath();
      c.arc(pane.x + 30 * u + i * 26 * u, pane.y + 28 * u, 7 * u, 0, Math.PI * 2);
      c.fill();
    });
    let y = pane.y + 78 * u;
    for (const [kind, text, at, color] of lines) {
      if (t < at) break;
      const shown = kind === "$" ? text.slice(0, Math.ceil(M.clamp((t - at) / Math.max(0.35, text.length * 0.018)) * text.length)) : text;
      const col = color ?? (kind === "$" ? k.ink : kind === "!" ? amber : kind === "#" ? k.sub : k.accent2);
      M.text(c, (kind === "$" ? "› " : "  ") + shown, pane.x + 30 * u, y, fs, col, kind === "$" ? 700 : 500, "left", pane.w - 60 * u, true);
      y += lh;
    }
    c.restore();
  }
  const head = (text, at, until, color = k.ink) =>
    M.caption(c, t, text, at, until, { x: headAt[0] / W, align: "left", y: headAt[1] / H, size: (V ? 52 : 62) * u, color, weight: 900, maxWidth: V ? W * 0.8 : pane.w });

  // Which recording (or still frame from it) the phone shows now.
  const screen =
    M.video("notes", t) ?? M.video("read", t) ?? M.video("tap", t) ?? M.video("flow", t) ??
    (t < 5 ? img("notes") : t < 25.6 ? img("datetime") : img("general"));

  if (t < 37) {
    // Short decisive moves toward whichever side is speaking, then hold. Portrait moves less:
    // its frame is narrow, so every target keeps both phone and terminal inside it.
    const C0 = [W / 2, H / 2],
      toPhone = V ? home : [W * 0.36, H * 0.45],
      toPane = V ? [W / 2, pane.y + pane.h / 2] : [W * 0.58, H * 0.5];
    const S0 = [W * 0.46, H * 0.43],
      phoneC = home,
      paneC = [W * 0.46, pane.y + pane.h / 2];
    // Portrait: into the phone while it acts, a fast move to the terminal for the result.
    M.shoot(c, t, V ? [
      [0.3, "to", phoneC, 1.4],
      [2.5, "to", S0, 1],
      [5.1, "to", phoneC, 1.5],
      [6.9, "to", S0, 1],
      [10.3, "to", phoneC, 1.45],
      [11.5, "to", S0, 1],
      [12.1, "to", phoneC, 1.55],
      [16.3, "to", S0, 1],
      [19.4, "push", S0, 0.04],
      [20.6, "to", S0, 1],
      [23.0, "to", phoneC, 1.4],
      [24.0, "to", S0, 1],
      [24.6, "to", phoneC, 1.5],
      [28.3, "to", S0, 1],
      [29.7, "to", phoneC, 1.45],
      [30.8, "to", S0, 1],
      [32.2, "push", S0, 0.05],
      [33.8, "to", phoneC, 1.3],
      [35.6, "to", S0, 1],
    ] : [
      [0.3, "push", toPhone, 0.04],
      [5.1, "to", C0, 1],
      [6.4, "push", toPane, 0.1],
      [9.6, "push", toPhone, 0.03],
      [12.1, "to", C0, 1],
      [15.4, "push", toPhone, 0.07],
      [18.1, "to", [W * 0.54, H * 0.5], 1.06],
      [21.4, "push", toPane, 0.05],
      [24.6, "to", C0, 1],
      [25.5, "push", toPhone, 0.07],
      [28.2, "to", [W * 0.56, H * 0.5], 1.06],
      [31.1, "to", C0, 1],
      [32.6, "push", toPane, 0.05],
      [35.4, "push", toPhone, 0.04],
    ], () => {
      // Honest results: the first refusal happened on a private Notes list, so that screen is
      // hidden; the second happened on Date & Time, which is shown as it was.
      if (t > 18.3 && t < 21.2) {
        phone(null);
        M.round(c, home[0] - PW / 2, home[1] - PH / 2, PW, PH, 54 * u, "#1C1F24");
        M.text(c, p.hidden, home[0], home[1], (V ? 24 : 30) * u, k.sub, 700, "center", PW * 0.86);
      } else phone(t >= 21.2 && t < 24.5 ? img("datetime") : screen);
      // Portrait: while the camera is inside the phone, only the phone is on screen; the words
      // arrive with the move to the terminal, so nothing is pushed into the platform's UI bands.
      const phoneFocus = V && [[0.25, 2.8], [5.05, 7.2], [10.25, 11.8], [12.05, 16.6], [22.95, 24.3], [24.55, 28.6], [29.65, 31.1], [33.75, 35.9]].some(([a, b]) => t >= a && t < b);
      if (phoneFocus) return;
      // hook
      M.caption(c, t, p.title1, -1, 4.9, { x: headAt[0] / W, align: "left", y: V ? 0.385 : 0.085, size: (V ? 52 : 62) * u, color: k.ink, weight: 900 });
      M.caption(c, t, p.title2, -1, 4.9, { x: headAt[0] / W, align: "left", y: V ? 0.43 : 0.175, size: (V ? 66 : 90) * u, color: k.accent, weight: 900, maxWidth: V ? W * 0.8 : pane.w });
      // Frame 0 is the cover: the title and the command are already in place.
      if (t < 5) term([["$", p.cType, -2], ["=", p.rType, 2.6]], -1, 4.9);
      // The claim in words, once the note has filled: long text goes in with one call.
      M.stamp(c, t, p.typed, V ? [W * 0.46, pane.y + pane.h * 0.66] : [pane.x + pane.w / 2, pane.y + pane.h * 0.6], { at: 2.8, until: 4.9, size: (V ? 52 : 64) * u, color: k.accent2, rotate: 0, box: false });
      // read
      head(p.read, 5.1, 12);
      term([["$", p.cRead, 5.4], ...d.rows.slice(0, V ? 4 : 7).map((r, i) => ["=", r, 6.2 + i * 0.12, k.ink])], 5.2, 11.9);
      M.stamp(c, t, p.readTime, [pane.x + pane.w - 130 * u, pane.y + 32 * u], { at: 7.6, until: 11.8, size: 38 * u, color: k.accent2, rotate: 0 });
      // tap by name: a ring on the row just before the recording opens it
      head(p.tap, 12.1, 18);
      term([["$", p.cTap1, 12.3], ["$", p.cTap2, 13.4], ["=", p.rTap, 16.1]], 12.2, 17.9);
      if (t > 15.2 && t < 16.4) M.ripple(c, t, [home[0], home[1] - PH / 2 + PH * 0.322], { at: 15.25, count: 2, radius: PW * 0.32, color: k.accent, width: 6 * u, gap: 0.3, dur: 0.6 });
      // honest results: the phone stays dimmed, because nothing on it changed
      head(p.honest, 18.2, 24.5);
      const short = (r) => r.split(" · ").slice(0, 2).join(" · "); // portrait: the first two fields fit
      term(V ? [["$", p.cH1, 18.4], ["!", short(p.rH1), 19.4], ["$", p.cH2, 21.2], ["!", short(p.rH2), 22.0]] : [["$", p.cH1, 18.4], ["!", p.rH1, 19.4], ["#", p.nH1, 19.9], ["$", p.cH2, 21.2], ["!", p.rH2, 22.0], ["#", p.nH2, 22.5]], 18.3, 24.4);
      // flow replay
      head(p.flow, 24.6, 31);
      term([["$", p.cFlow, 24.8], ["=", p.rFlow, 27.4]], 24.7, 30.9);
      M.stamp(c, t, p.tokens, V ? [W * 0.46, pane.y + pane.h * 0.72] : [pane.x + pane.w / 2, pane.y + pane.h * 0.62], { at: 28.4, until: 30.9, size: (V ? 56 : 66) * u, color: k.accent2, rotate: 0, box: false });
      // Wi-Fi: this session's own status
      head(p.wifi, 31.1, 37);
      term([["$", p.cStatus, 31.3], ["=", p.s1, 32.2], ["=", p.s2, 32.4, k.ink], ["=", p.s3, 32.6, k.ink]], 31.2, 36.9);
      M.stamp(c, t, p.ios, V ? [W * 0.58, pane.y + pane.h * 0.8] : [pane.x + pane.w * 0.72, pane.y + pane.h * 0.78], { at: 34.0, until: 36.9, size: (V ? 70 : 84) * u, color: k.accent2, rotate: -0.05 });
      if (t > 32.2 && t < 37) M.ripple(c, t, [home[0], home[1] - PH / 2 - 30 * u], { at: 32.3, count: 3, gap: 0.4, dur: 1.3, radius: 110 * u, color: k.accent, width: 6 * u });
    });
  }

  // People take over: the real recording of the browser control page, a person clicking the picture.
  if (t >= 37 && t < 49.5) {
    const page = M.video("people", t);
    if (page) {
      const iw = page.naturalWidth || page.width,
        ih = page.naturalHeight || page.height;
      // Landscape fills the frame with the page; portrait shows the whole page first (the browser
      // is part of the claim), then pushes far in on each click.
      const base = V ? W / iw : Math.max(W / iw, H / ih),
        s = V ? base : base * (1 + 0.06 * M.tween(t, 37, 1.2, "expoOut")),
        dw = iw * s,
        dh = ih * s,
        dx = V ? 0 : Math.min(0, Math.max(W - dw, W / 2 - 0.5 * dw)),
        dy = V ? H * 0.43 - dh / 2 : Math.min(0, Math.max(H - dh, H / 2 - 0.47 * dh));
      const clicks = [[37 + 3.06, 866, 398], [37 + 9.0, 679, 183]].map(([at, px, py]) => [at, dx + (px / 1920) * dw, dy + (py / 1080) * dh]);
      let focus = null,
        zp = 1;
      for (const [at, x, y] of clicks) {
        const k2 = M.tween(t, at - 0.7, 0.28) * (1 - M.tween(t, at + 2.6, 0.28));
        if (k2 > 0) ((focus = [x, y]), (zp = 1 + (V ? 1.6 : 0.14) * k2));
      }
      c.save();
      if (focus) (c.translate(focus[0], focus[1]), c.scale(zp, zp), c.translate(-focus[0], -focus[1]));
      c.drawImage(page, dx, dy, dw, dh);
      // Mark the person's two clicks where they happened (page pixels at 1920×1080).
      for (const [at, px, py] of [[37 + 3.06, 866, 398], [37 + 9.0, 679, 183]]) {
        const pt = [dx + (px / 1920) * dw, dy + (py / 1080) * dh];
        M.cursor(c, t, [[at - 0.6, pt[0] + 120 * u, pt[1] + 90 * u], [at - 0.05, pt[0], pt[1]], [at + 1.2, pt[0], pt[1]]], { clicks: [at], size: 1.3 });
      }
      c.restore();
    }
    // Enter and leave through black instead of a hard cut.
    const veil = 1 - M.tween(t, 37, 0.45) + 0.8 * M.tween(t, 48.7, 0.45);
    if (veil > 0) ((c.fillStyle = `rgba(11,13,16,${Math.min(1, veil)})`), c.fillRect(0, 0, W, H));
    M.caption(c, t, p.people, 37.5, 49.3, { y: V ? 0.13 : 0.09, size: (V ? 46 : 54) * u, color: k.ink, plate: "rgba(11,13,16,.88)", weight: 900, maxWidth: W * 0.9 });
  }

  // Outro: icon, name, the repository large and the command readable.
  if (t >= 49.1) {
    // The end card slides over the fading page, so there is never an empty frame.
    c.save();
    c.globalAlpha = M.tween(t, 49.1, 0.3);
    c.fillStyle = k.bg;
    c.fillRect(0, 0, W, H);
    c.restore();
    const cx = V ? W * 0.46 : W / 2,
      pop = M.easings.backOut(M.clamp((t - 49.2) / 0.4), 2.2),
      C = [cx, H * (V ? 0.22 : 0.24)],
      s = (V ? 240 : 200) * u;
    c.save();
    c.translate(C[0], C[1]);
    c.scale(pop, pop);
    c.beginPath();
    c.roundRect(-s / 2, -s / 2, s, s, s * 0.22);
    c.clip();
    c.drawImage(img("icon"), -s / 2, -s / 2, s, s);
    c.restore();
    M.signature(c, t, "iphone-use", [cx, H * (V ? 0.36 : 0.47)], { at: 49.8, dur: 1.0, size: (V ? 116 : 130) * u, maxWidth: V ? W * 0.78 : W, color: k.ink, accent: k.accent, underline: 51.2, pen: false });
    M.caption(c, t, d.repo, 51.1, Infinity, { x: cx / W, y: V ? 0.45 : 0.63, size: (V ? 40 : 58) * u, color: k.accent, weight: 800, maxWidth: V ? W * 0.8 : W * 0.9 });
    const cmd = d.install,
      shown = Math.floor(M.clamp((t - 51.5) / 1.2) * cmd.length),
      half = cmd.lastIndexOf("/", cmd.indexOf("install.sh")) + 1;
    if (shown > 0) {
      // Landscape: two lines; portrait: three, so the command stays readable on a phone.
      const y0 = H * (V ? 0.52 : 0.74),
        cut = V ? [cmd.indexOf("https"), half] : [half],
        parts = [],
        size = (V ? 30 : 34) * u;
      let from = 0;
      for (const at of [...cut, cmd.length]) (parts.push([from, at]), (from = at));
      M.round(c, V ? W * 0.06 : W * 0.06, y0 - 46 * u, V ? W * 0.8 : W * 0.88, (parts.length * 58 + 34) * u, 18 * u, k.surface);
      parts.forEach(([a, b], i) => {
        if (shown > a) M.text(c, (i ? "" : "$ ") + cmd.slice(a, Math.min(b, shown)).trim(), V ? W * 0.46 : W / 2, y0 + i * 58 * u, size, k.accent2, 500, "center", V ? W * 0.76 : W * 0.84, true);
      });
    }
    M.caption(c, t, p.install, 52.3, Infinity, { x: cx / W, y: V ? 0.66 : 0.88, size: 36 * u, color: k.sub, weight: 700 });
  }
  M.caption(c, t, p.real, 0.6, 4.9, { x: V ? 0.46 : 0.5, y: V ? 0.11 : 0.968, size: (V ? 24 : 26) * u, color: k.sub, weight: 600, maxWidth: V ? W * 0.8 : W * 0.9 });
  // Landscape subtitles sit low; portrait uses the default, the bottom of view.safe.
  // Portrait subtitles: bottom of view.safe, centred on it (clear of the right action column).
  M.captions(c, t, undefined, V ? { x: (view.safe.x + view.safe.w / 2) / W } : { y: t < 49.1 ? 0.9 : 0.95 });
};
