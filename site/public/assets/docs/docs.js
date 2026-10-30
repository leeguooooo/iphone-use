/* ==========================================================================
   Shared analytics for every iphone-use.leeguoo.com page.
   - blog.leeguoo.com central traffic beacon (self-locating -> posts to the blog)
   - Google Analytics 4: the unified leeguoo.com root-domain stream
     (G-RCV0Z432Y8, property 542876134; segment by hostname), the same stream
     chrome-use.leeguoo.com and the blog use.
   No ads, no account chip, no PostHog on this site.
   ========================================================================== */
(function () {
  // Preview builds must not count as production visits.
  if (location.hostname.toLowerCase() !== 'iphone-use.leeguoo.com') return;
  var head = document.head || document.getElementsByTagName('head')[0];
  if (!head) return;

  if (!document.querySelector('script[src*="visitor-beacon.js"]')) {
    var beacon = document.createElement('script');
    beacon.defer = true;
    beacon.src = 'https://blog.leeguoo.com/scripts/visitor-beacon.js?v=20260930-clicks';
    head.appendChild(beacon);
  }

  var GA_IDS = ['G-RCV0Z432Y8'];
  if (!window.__cuGtag && !window.gtag) {
    window.__cuGtag = true;
    var ga = document.createElement('script');
    ga.async = true;
    ga.src = 'https://www.googletagmanager.com/gtag/js?id=' + GA_IDS[0];
    head.appendChild(ga);
    window.dataLayer = window.dataLayer || [];
    window.gtag = function () { window.dataLayer.push(arguments); };
    window.gtag('js', new Date());
    GA_IDS.forEach(function (id) { window.gtag('config', id); });
  }
})();

/* ==========================================================================
   iphone-use site — docs.js (adapted from the chrome-use docs design system)  (vanilla JS, zero deps)
   --------------------------------------------------------------------------
   On DOMContentLoaded it:
     (a) holds the FULL information architecture in NAV (bilingual)
     (b) injects the sticky topbar (#du-topbar) + grouped sidebar (#du-sidebar),
         picking zh/en labels + hrefs from <html lang> and marking the active
         item from <body data-page> with the red squiggle underline
     (c) builds the right "on this page" TOC (#du-toc) from <main> h2/h3
         with smooth-scroll + scroll-spy
     (d) ⌘K / Ctrl-K command palette that filters NAV by title + keywords
     (e) a language toggle that swaps zh <-> /en/ preserving the current slug
     (f) code-block copy buttons + a top "Copy page" button
     (g) mobile hamburger that opens the sidebar as a drawer
   Every step guards for missing elements so a page can omit any region.
   ========================================================================== */
(function () {
  "use strict";

  /* -------------------------------------------------- information architecture
     slug 'overview' maps to / (and /en/).
     Every other slug maps to /<slug>  (en mirror /en/<slug>); files are <slug>.html.
     kw = extra search keywords (space separated, any language). ico = doodle. */
  var NAV = [
    { id: "start", label: { zh: "开始", en: "Getting Started" }, items: [
      { slug: "overview", ico: "🧭", zh: "概览", en: "Overview", kw: "intro start home iphone ai agent 首页 介绍 真机" },
      { slug: "install", ico: "📦", zh: "安装", en: "Install", kw: "install setup curl xcode developer mode doctor 安装 配置 开发者模式" },
      { slug: "compare", ico: "⚖️", zh: "和其他方案对比", en: "vs Other Tools", kw: "compare mobile-mcp appium webdriveragent agent-device ios-agent iphone-mcp 对比 选型" },
    ]},
    { id: "tasks", label: { zh: "让 agent 做事", en: "What Agents Can Do" }, items: [
      { slug: "tasks", ico: "📱", zh: "让 agent 用手机", en: "Ask Your Agent", kw: "tasks examples health wechat bank notes qa test 任务 例子 健康 微信 银行 测试" },
      { slug: "flows", ico: "🔁", zh: "Flow 录制与重放", en: "Flows & Replay", kw: "flow replay registry record schedule test suite ci 录制 重放 定时 测试套件" },
    ]},
    { id: "connect", label: { zh: "接入方式", en: "Connecting Agents" }, items: [
      { slug: "phone-agent", ico: "🤖", zh: "手机自己操作自己", en: "Phone Drives Itself", kw: "on-phone assistant agent self control no computer local control model openrouter 助手 自己操作自己 本机控制 不需要电脑" },
      { slug: "remote", ico: "📡", zh: "远程控制", en: "Remote Control", kw: "remote control phone to phone relay cloudflare local control no mac self host 远程 手机控制手机 中继 本机控制 不需要电脑" },
      { slug: "connect", ico: "🔌", zh: "MCP、HTTP API 与 skill", en: "MCP, HTTP API & Skill", kw: "mcp claude code codex cursor vscode gemini opencode windsurf http api skill plugin 接入 插件" },
    ]},
    { id: "under", label: { zh: "底层", en: "Under the Hood" }, items: [
      { slug: "how-it-works", ico: "⚙️", zh: "它怎么工作", en: "How It Works", kw: "architecture daemon xctest runner usb wi-fi usbmux coredevice tunnel ios 15 16 架构 原理 守护进程" },
    ]},
    { id: "ref", label: { zh: "参考", en: "Reference" }, items: [
      { slug: "faq", ico: "❓", zh: "常见问题", en: "FAQ", kw: "faq jailbreak windows linux simulator license 常见问题 越狱 模拟器 许可证" },
      { slug: "privacy", ico: "🔒", zh: "隐私与安全", en: "Privacy & Security", kw: "privacy security telemetry token password 隐私 安全 遥测" },
    ]},
  ];

  /* doc tabs (topbar center) */
  var TABS = [
    { slug: "overview", zh: "文档", en: "Docs" },
    { slug: "connect", zh: "接入", en: "Connect" },
    { slug: "releases", zh: "下载", en: "Releases", href: "https://github.com/leeguooooo/iphone-use/releases" },
  ];

  var GH = "https://github.com/leeguooooo/iphone-use";

  /* --------------------------------------------------------------- helpers */
  function el(tag, cls, html) {
    var n = document.createElement(tag);
    if (cls) n.className = cls;
    if (html != null) n.innerHTML = html;
    return n;
  }
  function lang() {
    return (document.documentElement.lang || "zh").toLowerCase().indexOf("en") === 0 ? "en" : "zh";
  }
  // Build an href for a slug in a given language. overview -> / or /en/.
  // Extensionless: the Worker serves /install.html at /install.
  function hrefFor(slug, lng) {
    var base = lng === "en" ? "/en/" : "/";
    return slug === "overview" ? base : base + slug;
  }
  function currentSlug() {
    return (document.body && document.body.dataset.page) || "overview";
  }
  var SVG = {
    gh: '<svg viewBox="0 0 16 16" fill="currentColor"><path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0016 8c0-4.42-3.58-8-8-8z"/></svg>',
    search: '<svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="2"><circle cx="9" cy="9" r="6"/><path d="M14 14l4 4" stroke-linecap="round"/></svg>',
    menu: '<svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M3 5h14M3 10h14M3 15h14"/></svg>',
    copy: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6"><rect x="5" y="5" width="9" height="9" rx="1.5"/><path d="M11 5V3.5A1.5 1.5 0 009.5 2h-6A1.5 1.5 0 002 3.5v6A1.5 1.5 0 003.5 11H5"/></svg>',
  };

  /* ============================================================= TOP BAR == */
  function buildTopbar() {
    var host = document.getElementById("du-topbar");
    if (!host) return;
    var lng = lang();
    host.classList.add("du-topbar");

    // hamburger (mobile only, shown via CSS)
    var burger = el("button", "du-hamburger", SVG.menu);
    burger.setAttribute("aria-label", "菜单 / Menu");
    burger.addEventListener("click", toggleDrawer);
    host.appendChild(burger);

    // logo
    var logo = el("a", "du-logo");
    logo.href = hrefFor("overview", lng);
    logo.innerHTML =
      '<img class="du-logo-ico" src="/assets/icon-180.png" alt="" width="26" height="26" />' +
      '<span class="du-logo-word">iphone<span class="accent">-use</span></span>' +
      '<span class="du-repo-tag">leeguooooo/iphone-use</span>';
    host.appendChild(logo);

    // center doc tabs
    var tabs = el("nav", "du-tabs");
    var slug = currentSlug();
    TABS.forEach(function (t) {
      var a = el("a", null, t[lng]);
      a.href = t.href || hrefFor(t.slug, lng);
      if (t.href) { a.target = "_blank"; a.rel = "noopener"; }
      if (t.slug === slug) a.classList.add("active");
      tabs.appendChild(a);
    });
    host.appendChild(tabs);

    // right cluster
    var right = el("div", "du-topbar-right");

    var searchBtn = el("button", "du-search-btn",
      SVG.search + '<span class="du-search-label">' + (lng === "en" ? "Search…" : "搜索…") + '</span>' +
      '<span class="du-kbd">⌘K</span>');
    searchBtn.addEventListener("click", openSearch);
    right.appendChild(searchBtn);

    var langBox = el("div", "du-lang");
    var zhA = el("a", lng === "zh" ? "active" : null, "中");
    var enA = el("a", lng === "en" ? "active" : null, "EN");
    zhA.href = hrefFor(currentSlug(), "zh");
    enA.href = hrefFor(currentSlug(), "en");
    langBox.appendChild(zhA); langBox.appendChild(enA);
    right.appendChild(langBox);

    var gh = el("a", "du-gh", SVG.gh + "<span>GitHub</span>");
    gh.href = GH; gh.target = "_blank"; gh.rel = "noopener";
    right.appendChild(gh);

    host.appendChild(right);
  }

  /* ============================================================= SIDEBAR == */
  function buildSidebar() {
    var host = document.getElementById("du-sidebar");
    if (!host) return;
    var lng = lang();
    var slug = currentSlug();
    host.classList.add("du-sidebar");
    host.setAttribute("role", "navigation");
    // docs/_tools/seo.py pre-renders the same links for crawlers and first
    // paint; rebuild from NAV so handlers and the active state stay in one place.
    host.textContent = "";

    NAV.forEach(function (group) {
      var g = el("div", "du-nav-group");
      g.appendChild(el("div", "du-nav-label", group.label[lng]));
      group.items.forEach(function (it) {
        var a = el("a", "du-nav-item");
        a.href = hrefFor(it.slug, lng);
        a.innerHTML =
          '<span class="du-nav-ico">' + it.ico + '</span>' +
          '<span class="du-nav-text">' + it[lng] + '</span>';
        if (it.slug === slug) {
          a.classList.add("active");
          a.setAttribute("aria-current", "page");
        }
        a.addEventListener("click", closeDrawer);
        g.appendChild(a);
      });
      host.appendChild(g);
    });

    // drawer scrim (mobile)
    if (!document.querySelector(".du-drawer-scrim")) {
      var scrim = el("div", "du-drawer-scrim");
      scrim.addEventListener("click", closeDrawer);
      document.body.appendChild(scrim);
    }
  }

  function toggleDrawer() { document.body.classList.toggle("du-drawer-open"); }
  function closeDrawer() { document.body.classList.remove("du-drawer-open"); }

  /* =============================================== ON-THIS-PAGE TOC ======= */
  function buildTOC() {
    var host = document.getElementById("du-toc");
    var main = document.querySelector("main.du-main") || document.querySelector("main");
    if (!host || !main) return;
    var heads = main.querySelectorAll("h2, h3");
    if (!heads.length) { host.style.display = "none"; return; }

    var lng = lang();
    host.classList.add("du-toc");
    host.appendChild(el("div", "du-toc-label", lng === "en" ? "On this page" : "本页目录"));
    var ul = el("ul");
    var links = [];

    var usedIds = {};
    heads.forEach(function (h, i) {
      if (!h.id) {
        var base = "sec-" + ((h.textContent || "").trim().toLowerCase().replace(/[^\w一-龥]+/g, "-").replace(/^-+|-+$/g, "") || i);
        var uid = base, n = 2;
        while (usedIds[uid] || document.getElementById(uid)) uid = base + "-" + (n++);
        h.id = uid;
      }
      usedIds[h.id] = true;
      // add hover anchor: click copies a shareable deep-link + smooth-scrolls
      if (!h.querySelector(".du-anchor")) {
        var anc = el("a", "du-anchor", "#");
        anc.href = "#" + h.id;
        anc.setAttribute("aria-label", lng === "en" ? "Copy link to this section" : "复制本节链接");
        anc.title = anc.getAttribute("aria-label");
        (function (hid, a) {
          a.addEventListener("click", function (e) {
            e.preventDefault();
            var t = document.getElementById(hid);
            if (t) t.scrollIntoView({ behavior: "smooth", block: "start" });
            // replaceState puts the deep-link in the address bar synchronously,
            // so the URL is always shareable even if the async clipboard write
            // hangs or is blocked. Fire the copy, confirm optimistically.
            try { history.replaceState(null, "", "#" + hid); } catch (_) {}
            var url = location.origin + location.pathname + "#" + hid;
            copyText(url, a, "", true);
            toast(lng === "en" ? "Section link copied" : "已复制章节链接");
          });
        })(h.id, anc);
        h.appendChild(anc);
      }
      var li = el("li");
      var a = el("a", h.tagName === "H3" ? "lvl-3" : "lvl-2", (h.textContent || "").replace(/#$/, "").trim());
      a.href = "#" + h.id;
      a.addEventListener("click", function (e) {
        e.preventDefault();
        var t = document.getElementById(h.id);
        if (t) t.scrollIntoView({ behavior: "smooth", block: "start" });
        history.replaceState(null, "", "#" + h.id);
      });
      li.appendChild(a); ul.appendChild(li);
      links.push({ id: h.id, a: a, el: h });
    });
    host.appendChild(ul);

    // scroll-spy
    if ("IntersectionObserver" in window) {
      var visible = {};
      var io = new IntersectionObserver(function (entries) {
        entries.forEach(function (en) { visible[en.target.id] = en.isIntersecting ? en.intersectionRatio : 0; });
        var best = null, bestR = 0;
        links.forEach(function (l) { var r = visible[l.id] || 0; if (r >= bestR && r > 0) { bestR = r; best = l; } });
        if (!best) return;
        links.forEach(function (l) { l.a.classList.toggle("active", l === best); });
      }, { rootMargin: "-" + (parseInt(getComputedStyle(document.documentElement).getPropertyValue("--topbar-h")) || 58) + "px 0px -70% 0px", threshold: [0, .25, .5, 1] });
      links.forEach(function (l) { io.observe(l.el); });
    }
  }

  /* ===================================================== ⌘K SEARCH ======= */
  var searchState = { overlay: null, input: null, results: null, flat: [], filtered: [], idx: 0 };

  function flattenNav() {
    var lng = lang(), out = [];
    NAV.forEach(function (g) {
      g.items.forEach(function (it) {
        out.push({
          title: it[lng], ico: it.ico, group: g.label[lng],
          href: hrefFor(it.slug, lng),
          hay: (it.zh + " " + it.en + " " + it.slug + " " + (it.kw || "")).toLowerCase(),
        });
      });
    });
    return out;
  }

  function buildSearch() {
    var overlay = el("div", "du-search-overlay");
    overlay.innerHTML =
      '<div class="du-search-box" role="dialog" aria-modal="true" aria-label="搜索文档">' +
        '<div class="du-search-input-row">' + SVG.search +
          '<input class="du-search-input" type="text" placeholder="' + (lang() === "en" ? "Search docs…" : "搜索文档…") + '" autocomplete="off" spellcheck="false" />' +
        '</div>' +
        '<div class="du-search-results"></div>' +
        '<div class="du-search-foot"><span><kbd>↑</kbd><kbd>↓</kbd> ' + (lang() === "en" ? "navigate" : "选择") + '</span>' +
          '<span><kbd>↵</kbd> ' + (lang() === "en" ? "open" : "打开") + '</span>' +
          '<span><kbd>esc</kbd> ' + (lang() === "en" ? "close" : "关闭") + '</span></div>' +
      '</div>';
    document.body.appendChild(overlay);
    searchState.overlay = overlay;
    searchState.input = overlay.querySelector(".du-search-input");
    searchState.results = overlay.querySelector(".du-search-results");
    searchState.flat = flattenNav();

    overlay.addEventListener("click", function (e) { if (e.target === overlay) closeSearch(); });
    searchState.input.addEventListener("input", function () { renderResults(this.value); });
    searchState.input.addEventListener("keydown", function (e) {
      if (e.key === "ArrowDown") { e.preventDefault(); moveSel(1); }
      else if (e.key === "ArrowUp") { e.preventDefault(); moveSel(-1); }
      else if (e.key === "Enter") { e.preventDefault(); go(); }
      else if (e.key === "Escape") { e.preventDefault(); closeSearch(); }
    });
  }
  function renderResults(q) {
    q = (q || "").trim().toLowerCase();
    var list = q ? searchState.flat.filter(function (r) { return r.hay.indexOf(q) >= 0; }) : searchState.flat;
    searchState.filtered = list; searchState.idx = 0;
    var box = searchState.results; box.innerHTML = "";
    if (!list.length) { box.appendChild(el("div", "du-search-empty", lang() === "en" ? "No matches" : "没有匹配结果")); return; }
    list.forEach(function (r, i) {
      var a = el("a", "du-search-result" + (i === 0 ? " active" : ""),
        '<span class="ico">' + r.ico + '</span><span>' + r.title + '</span><span class="grp">' + r.group + '</span>');
      a.href = r.href;
      a.addEventListener("mousemove", function () { setSel(i); });
      a.addEventListener("click", function (e) { e.preventDefault(); location.href = r.href; });
      box.appendChild(a);
    });
  }
  function setSel(i) {
    searchState.idx = i;
    var kids = searchState.results.children;
    for (var k = 0; k < kids.length; k++) kids[k].classList.toggle("active", k === i);
  }
  function moveSel(d) {
    var n = searchState.filtered.length; if (!n) return;
    setSel((searchState.idx + d + n) % n);
    var act = searchState.results.children[searchState.idx];
    if (act) act.scrollIntoView({ block: "nearest" });
  }
  function go() { var r = searchState.filtered[searchState.idx]; if (r) location.href = r.href; }
  function openSearch() {
    if (!searchState.overlay) buildSearch();
    searchState.overlay.classList.add("open");
    renderResults("");
    searchState.input.value = "";
    setTimeout(function () { searchState.input.focus(); }, 20);
  }
  function closeSearch() { if (searchState.overlay) searchState.overlay.classList.remove("open"); }

  /* ============================================ DEEP-LINK / TOAST ======= */
  // Headings get their ids at runtime (buildTOC), so a cold load on
  // page.html#sec-xxx can't be scrolled by the browser's native :target jump —
  // the id doesn't exist yet at parse time. Re-run the jump once ids exist.
  var _userScrolled = false;
  ["wheel", "touchstart", "keydown"].forEach(function (ev) {
    window.addEventListener(ev, function () { _userScrolled = true; }, { passive: true });
  });
  function currentHashId() {
    var raw = (location.hash || "").replace(/^#/, "");
    if (!raw) return "";
    try { return decodeURIComponent(raw); } catch (_) { return raw; }
  }
  function focusHash(smooth) {
    var id = currentHashId();
    if (!id) return false;
    var t = document.getElementById(id);
    if (!t) return false;
    t.scrollIntoView({ behavior: smooth ? "smooth" : "auto", block: "start" });
    return true;
  }
  function wireDeepLinks() {
    // cold-load jump (ids now assigned)
    focusHash(false);
    // correct for late layout shift (images/fonts) unless the user already moved
    window.addEventListener("load", function () { if (!_userScrolled) focusHash(false); });
    // manual hash edits / back-forward across sections
    window.addEventListener("hashchange", function () { focusHash(true); });
  }

  var _toastEl = null, _toastTimer = null;
  function toast(msg) {
    if (!_toastEl) { _toastEl = el("div", "du-toast"); document.body.appendChild(_toastEl); }
    _toastEl.textContent = msg;
    _toastEl.classList.add("show");
    clearTimeout(_toastTimer);
    _toastTimer = setTimeout(function () { _toastEl.classList.remove("show"); }, 1600);
  }

  /* ================================================== COPY BUTTONS ======= */
  function copyText(text, btn, doneLabel, isIcon) {
    var write = navigator.clipboard && navigator.clipboard.writeText
      ? navigator.clipboard.writeText(text)
      : new Promise(function (res, rej) {
          var ta = document.createElement("textarea"); ta.value = text;
          ta.style.position = "fixed"; ta.style.opacity = "0"; document.body.appendChild(ta);
          ta.select(); try { document.execCommand("copy"); res(); } catch (e) { rej(e); } document.body.removeChild(ta);
        });
    write.then(function () {
      btn.classList.add("copied");
      var prev = btn.innerHTML;
      if (!isIcon) btn.textContent = doneLabel;
      setTimeout(function () { btn.classList.remove("copied"); if (!isIcon) btn.innerHTML = prev; }, 1400);
    }).catch(function () {});
    return write;
  }

  function wireCodeCopy() {
    var lng = lang();
    document.querySelectorAll(".du-code").forEach(function (block) {
      if (block.querySelector(".du-code-copy")) return;
      var head = block.querySelector(".du-code-head");
      if (!head) { // synthesize a header with mac dots if the author omitted one
        head = el("div", "du-code-head", '<span class="dot r"></span><span class="dot y"></span><span class="dot g"></span>');
        block.insertBefore(head, block.firstChild);
      }
      var btn = el("button", "du-code-copy", lng === "en" ? "Copy" : "复制");
      btn.addEventListener("click", function () {
        var pre = block.querySelector("pre");
        var code = pre ? pre.cloneNode(true) : null;
        if (code) code.querySelectorAll(".du-prompt").forEach(function (prompt) { prompt.remove(); });
        copyText(code ? code.textContent : "", btn, lng === "en" ? "Copied" : "已复制", false);
      });
      head.appendChild(btn);
    });
  }

  function wireCopyPage() {
    var lng = lang();
    document.querySelectorAll(".du-copy-page").forEach(function (btn) {
      if (btn.dataset.wired) return; btn.dataset.wired = "1";
      btn.addEventListener("click", function () {
        var main = document.querySelector("main.du-main") || document.querySelector("main");
        copyText(main ? main.innerText : "", btn, "", true);
        var lbl = btn.querySelector(".du-copy-page-label");
        if (lbl) { var p = lbl.textContent; lbl.textContent = lng === "en" ? "Copied!" : "已复制!"; setTimeout(function () { lbl.textContent = p; }, 1400); }
      });
    });
  }

  /* ================================================== PAGE FOOTER ========= */
  // Social / outbound links + author byline, injected once at the end of
  // <main> on every page — there's no static per-page footer to match, so
  // this is the one place that guarantees zh/en parity across all pages.
  function buildFooter() {
    var main = document.querySelector("main.du-main") || document.querySelector("main");
    if (!main || main.querySelector(".du-page-footer")) return;
    var lng = lang();

    var footer = el("footer", "du-page-footer");

    var social = el("div", "du-footer-social");
    function link(href, label, title) {
      var a = el("a", null, label);
      a.href = href; a.target = "_blank"; a.rel = "noopener";
      if (title) a.title = title;
      social.appendChild(a);
    }
    link(GH, "GitHub", "leeguooooo/iphone-use");
    link("https://github.com/leeguooooo", "@leeguooooo", lng === "en" ? "Author on GitHub" : "作者 GitHub 主页");
    link("https://blog.leeguoo.com", lng === "en" ? "Blog" : "博客");
    link("https://x.com/leeguooooo", "X", lng === "en" ? "Author on X" : "作者 X");
    link("https://www.linkedin.com/in/li-guo-372ba1365/", "LinkedIn", lng === "en" ? "Author on LinkedIn" : "作者 LinkedIn");
    footer.appendChild(social);

    // "More from the author" — plain links to the author's other apps.
    // Clicks are counted by the visitor beacon (auto-tracks these hosts).
    var utm = "utm_source=iphone-use.leeguoo.com&utm_medium=cta&utm_campaign=";
    var more = el("div", "du-footer-more");
    more.appendChild(el("p", "du-footer-more-title", lng === "en" ? "More from the author" : "作者的其他作品"));
    var list = el("ul", "du-footer-more-list");
    [
      ["chrome-use", "https://chrome-use.leeguoo.com/" + (lng === "en" ? "en/" : "") + "?" + utm + "chrome-use",
        lng === "en" ? "The sibling in the *-use family: lets AI agents use your real, signed-in Chrome."
                     : "*-use 家族的兄弟项目：让 AI agent 用你已登录的真实 Chrome。"],
      ["ChooseBrowser", "https://choosebrowser.leeguoo.com/choose-browser/?" + utm + "choosebrowser",
        lng === "en" ? "A Mac app that opens every link in the right browser or profile, by URL rules."
                     : "按网址规则，把每个链接自动用对的浏览器 / Profile 打开的 Mac 小工具。"],
      ["Pastyx", "https://paste.leeguoo.com/?" + utm + "pastyx",
        lng === "en" ? "Clipboard history for Mac and Windows, with optional sync and web access."
                     : "Mac 和 Windows 剪贴板历史，可选同步和网页版。"]
    ].forEach(function (it) {
      var li = el("li");
      var a = el("a", null, it[0]);
      a.href = it[1]; a.target = "_blank"; a.rel = "noopener";
      li.appendChild(a);
      li.appendChild(document.createTextNode(" — " + it[2]));
      list.appendChild(li);
    });
    more.appendChild(list);
    footer.appendChild(more);

    var byline = el("p", "du-footer-byline");
    var a = el("a", null, "郭立 (leeguoo)");
    a.href = "https://leeguoo.com"; a.target = "_blank"; a.rel = "noopener";
    byline.appendChild(document.createTextNode(lng === "en" ? "Built by " : "作者 "));
    byline.appendChild(a);
    footer.appendChild(byline);

    // keep seo.py's static link row (.du-static-footer) as the last thing in <main>
    main.insertBefore(footer, main.querySelector(".du-static-footer"));
  }

  /* ================================================= GLOBAL KEYBINDINGS == */
  function wireKeys() {
    document.addEventListener("keydown", function (e) {
      if ((e.metaKey || e.ctrlKey) && (e.key === "k" || e.key === "K")) { e.preventDefault(); openSearch(); }
      else if (e.key === "Escape") { closeSearch(); closeDrawer(); }
      else if (e.key === "/" && !/^(INPUT|TEXTAREA)$/.test((document.activeElement || {}).tagName || "") && !e.metaKey && !e.ctrlKey) {
        e.preventDefault(); openSearch();
      }
    });
  }

  /* ============================================================= BOOT ===== */
  function init() {
    try { buildTopbar(); } catch (e) { console.error("[docs] topbar", e); }
    try { buildSidebar(); } catch (e) { console.error("[docs] sidebar", e); }
    try { buildTOC(); } catch (e) { console.error("[docs] toc", e); }
    try { buildFooter(); } catch (e) { console.error("[docs] footer", e); }
    try { wireCodeCopy(); } catch (e) { console.error("[docs] code copy", e); }
    try { wireCopyPage(); } catch (e) { console.error("[docs] copy page", e); }
    try { wireKeys(); } catch (e) { console.error("[docs] keys", e); }
    try { wireDeepLinks(); } catch (e) { console.error("[docs] deep links", e); }
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init);
  else init();
})();
