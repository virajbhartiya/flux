// flux web UI. No framework: server-rendered pages, swapped in place.
(() => {
  "use strict";
  const $ = (s, r = document) => r.querySelector(s);
  const $$ = (s, r = document) => Array.from(r.querySelectorAll(s));

  // ── theme ──────────────────────────────────────────────────────────
  const root = document.documentElement;
  try {
    const t = localStorage.getItem("flux-theme");
    if (t) root.dataset.theme = t;
  } catch (_) {}
  function toggleTheme() {
    const dark = root.dataset.theme
      ? root.dataset.theme === "dark"
      : matchMedia("(prefers-color-scheme: dark)").matches;
    root.dataset.theme = dark ? "light" : "dark";
    try { localStorage.setItem("flux-theme", root.dataset.theme); } catch (_) {}
  }

  // ── instant navigation ─────────────────────────────────────────────
  // Pages are fetched (and prefetched on hover), then <main>, the tab bar
  // and the title are swapped in. No full reloads, no layout flash.
  const cache = new Map(); // url -> {t, p: Promise<string>}
  const TTL = 15000;
  let progress;

  function eligible(a, ev) {
    if (!a || !a.href || a.target || a.hasAttribute("download") || a.dataset.noSwap !== undefined) return false;
    if (ev && (ev.metaKey || ev.ctrlKey || ev.shiftKey || ev.altKey || ev.button !== 0)) return false;
    const u = new URL(a.href, location.href);
    if (u.origin !== location.origin) return false;
    if (u.pathname.includes("/-/raw/") || u.pathname.includes("/-/events") || u.pathname.startsWith("/-/static/")) return false;
    if (u.pathname === location.pathname && u.search === location.search && u.hash) return false;
    return true;
  }

  function fetchPage(url) {
    const hit = cache.get(url);
    if (hit && performance.now() - hit.t < TTL) return hit.p;
    const p = fetch(url, { headers: { "X-Flux-Partial": "1" }, credentials: "same-origin" }).then((r) => {
      if (!r.ok && r.status !== 404 && r.status !== 409) throw new Error(String(r.status));
      if (!(r.headers.get("content-type") || "").includes("text/html")) throw new Error("not html");
      return r.text();
    });
    cache.set(url, { t: performance.now(), p });
    p.catch(() => cache.delete(url));
    return p;
  }

  function startProgress() {
    if (!progress) {
      progress = document.createElement("div");
      progress.id = "progress";
      document.body.appendChild(progress);
    }
    progress.style.opacity = "1";
    progress.style.width = "0";
    requestAnimationFrame(() => (progress.style.width = "60%"));
  }
  function endProgress() {
    if (!progress) return;
    progress.style.width = "100%";
    setTimeout(() => (progress.style.opacity = "0"), 150);
  }

  async function navigate(url, push) {
    const slow = setTimeout(startProgress, 80);
    try {
      const html = await fetchPage(url);
      clearTimeout(slow);
      const doc = new DOMParser().parseFromString(html, "text/html");
      for (const sel of ["main", "nav.repo-tabs", "header.top", "footer.foot"]) {
        const cur = $(sel), next = $(sel, doc);
        if (cur && next) cur.replaceWith(next);
        else if (cur && !next) cur.remove();
        else if (!cur && next && sel === "nav.repo-tabs") $("header.top").after(next);
      }
      document.title = doc.title;
      if (push) history.pushState({}, "", url);
      const hash = new URL(url, location.href).hash;
      if (hash && $(hash)) $(hash).scrollIntoView();
      else window.scrollTo(0, 0);
      endProgress();
      onPage();
    } catch (e) {
      clearTimeout(slow);
      location.href = url; // fall back to a normal load
    }
  }

  let hoverTimer;
  document.addEventListener("mouseover", (ev) => {
    const a = ev.target.closest && ev.target.closest("a");
    if (!eligible(a)) return;
    clearTimeout(hoverTimer);
    hoverTimer = setTimeout(() => fetchPage(a.href), 60);
  });
  document.addEventListener("mouseout", () => clearTimeout(hoverTimer));
  document.addEventListener("touchstart", (ev) => {
    const a = ev.target.closest && ev.target.closest("a");
    if (eligible(a)) fetchPage(a.href);
  }, { passive: true });
  document.addEventListener("mousedown", (ev) => {
    const a = ev.target.closest && ev.target.closest("a");
    if (eligible(a, ev)) fetchPage(a.href);
  });
  document.addEventListener("click", (ev) => {
    const a = ev.target.closest && ev.target.closest("a");
    if (!eligible(a, ev)) return;
    ev.preventDefault();
    closePalette();
    navigate(a.href, true);
  });
  window.addEventListener("popstate", () => navigate(location.href, false));

  // Destructive forms ask first (data-confirm="…").
  document.addEventListener("submit", (ev) => {
    const f = ev.target;
    const msg = f.dataset && f.dataset.confirm;
    if (msg && !confirm(msg)) { ev.preventDefault(); ev.stopImmediatePropagation(); }
  }, true);

  // GET forms (search) also navigate in place.
  document.addEventListener("submit", (ev) => {
    const f = ev.target;
    if ((f.method || "get").toLowerCase() !== "get") return;
    ev.preventDefault();
    const u = new URL(f.action, location.href);
    u.search = new URLSearchParams(new FormData(f)).toString();
    navigate(u.toString(), true);
  });

  // ── copy buttons ───────────────────────────────────────────────────
  async function copy(text, btn) {
    try {
      await navigator.clipboard.writeText(text);
      if (btn) {
        btn.classList.add("copied");
        setTimeout(() => btn.classList.remove("copied"), 1200);
      }
    } catch (_) {}
  }
  document.addEventListener("click", (ev) => {
    const b = ev.target.closest && ev.target.closest(".copy-btn");
    if (b) {
      ev.preventDefault();
      if (b.dataset.copyUrl) copy(location.origin + b.dataset.copyUrl + location.hash, b);
      else copy(b.dataset.copy || "", b);
      return;
    }
    if (ev.target.closest && ev.target.closest(".theme-toggle")) toggleTheme();
    const fold = ev.target.closest && ev.target.closest(".fold");
    if (fold) fold.closest(".diff-file").classList.toggle("folded");
    const live = ev.target.closest && ev.target.closest("#live-banner button");
    if (live) {
      cache.clear();
      $("#live-banner").hidden = true;
      navigate(location.href, false);
    }
  });

  // ── line selection (#L10 or #L10-L20) ──────────────────────────────
  let anchorLine = null;
  function applyLineHash() {
    $$("table.code tr.sel").forEach((r) => r.classList.remove("sel"));
    const m = /^#L(\d+)(?:-L(\d+))?$/.exec(location.hash);
    if (!m) return;
    const a = +m[1], b = +(m[2] || m[1]);
    for (let i = Math.min(a, b); i <= Math.max(a, b); i++) {
      const r = document.getElementById("L" + i);
      if (r) r.classList.add("sel");
    }
    const first = document.getElementById("L" + Math.min(a, b));
    if (first && !first.dataset.seen) {
      first.dataset.seen = "1";
      first.scrollIntoView({ block: "center" });
    }
  }
  document.addEventListener("click", (ev) => {
    const td = ev.target.closest && ev.target.closest("table.code td.ln");
    if (!td || ev.target.closest("a")) return;
    const n = +td.dataset.line;
    if (!n) return;
    let hash = "#L" + n;
    if (ev.shiftKey && anchorLine) hash = "#L" + Math.min(anchorLine, n) + "-L" + Math.max(anchorLine, n);
    else anchorLine = n;
    history.replaceState({}, "", location.pathname + location.search + hash);
    applyLineHash();
  });
  window.addEventListener("hashchange", applyLineHash);

  // ── branch picker filter ───────────────────────────────────────────
  document.addEventListener("input", (ev) => {
    if (!ev.target.classList.contains("picker-filter")) return;
    const q = ev.target.value.toLowerCase();
    $$("li", ev.target.closest(".picker-menu")).forEach((li) => {
      li.hidden = !li.textContent.toLowerCase().includes(q);
    });
  });
  document.addEventListener("toggle", (ev) => {
    if (ev.target.classList && ev.target.classList.contains("picker") && ev.target.open) {
      const f = $(".picker-filter", ev.target);
      if (f) f.focus();
    }
  }, true);
  document.addEventListener("click", (ev) => {
    $$("details.picker[open], details.clone[open], details.menu[open], details.side-edit[open], details.edit-toggle[open]").forEach((d) => { if (!d.contains(ev.target)) d.open = false; });
  });

  // ── command palette (⌘K) and file finder (t) ─────────────────────
  // One overlay, several sources: commands for the current page, every
  // repository, the repository's branches and its files. Everything is
  // fetched once and filtered locally, so typing never waits on the network.
  const pal = { mode: "all", items: [], sel: 0, data: {} };
  function ctx() {
    const m = $("main");
    return m ? { repo: m.dataset.repo, rev: m.dataset.rev, files: m.dataset.files } : {};
  }
  async function getJSON(url) {
    if (pal.data[url]) return pal.data[url];
    const p = fetch(url, { credentials: "same-origin" }).then((r) => (r.ok ? r.json() : null)).catch(() => null);
    pal.data[url] = p;
    return p;
  }
  function esc(s) { return String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]); }
  function score(text, q) {
    let s = 0, qi = 0, prev = -2;
    const lower = text.toLowerCase(), base = text.lastIndexOf("/") + 1, hits = [];
    for (let i = 0; i < lower.length && qi < q.length; i++) {
      if (lower[i] === q[qi]) {
        let v = 1;
        if (prev === i - 1) v += 5;
        if (i === 0 || "/_-. ".includes(text[i - 1])) v += 4;
        if (i >= base) v += 2;
        s += v; prev = i; qi++; hits.push(i);
      }
    }
    return qi === q.length ? [s * 100 - text.length, hits] : null;
  }
  function marked(text, hits) {
    const set = new Set(hits);
    let out = "";
    for (let k = 0; k < text.length; k++) out += set.has(k) ? "<b>" + esc(text[k]) + "</b>" : esc(text[k]);
    return out;
  }
  const ICONS = {
    file: '<svg class="icon" viewBox="0 0 16 16" width="16" height="16"><path d="M3.75 1.5h5.3L13 5.45v8.8c0 .4-.35.75-.75.75h-8.5A.75.75 0 0 1 3 14.25V2.25c0-.4.35-.75.75-.75Zm5 .75v3.5h3.5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"/></svg>',
    branch: '<svg class="icon" viewBox="0 0 16 16" width="16" height="16"><path d="M4.5 4.25v7.5M11.5 4.25v1c0 2.75-7 2.25-7 5.5M6 2.75a1.5 1.5 0 1 1-3 0 1.5 1.5 0 1 1 3 0ZM6 13.25a1.5 1.5 0 1 1-3 0 1.5 1.5 0 1 1 3 0ZM13 2.75a1.5 1.5 0 1 1-3 0 1.5 1.5 0 1 1 3 0Z" fill="none" stroke="currentColor" stroke-width="1.5"/></svg>',
    repo: '<svg class="icon" viewBox="0 0 16 16" width="16" height="16"><path d="M3 2.5h8.5a1 1 0 0 1 1 1v10H4a1 1 0 0 1-1-1Zm0 9.25a1 1 0 0 1 1-1h8.5M6 5h4" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"/></svg>',
    go: '<svg class="icon" viewBox="0 0 16 16" width="16" height="16"><path d="M3 8h10M9 4l4 4-4 4" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg>',
    search: '<svg class="icon" viewBox="0 0 16 16" width="16" height="16"><path d="M7 2a5 5 0 1 0 0 10A5 5 0 0 0 7 2Zm3.6 8.6L14 14" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/></svg>',
    commit: '<svg class="icon" viewBox="0 0 16 16" width="16" height="16"><path d="M8 5.25a2.75 2.75 0 1 0 0 5.5 2.75 2.75 0 0 0 0-5.5ZM1 8h4.25m5.5 0H15" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/></svg>',
  };
  function commands(c) {
    const r = c.repo ? "/" + encodeURI(c.repo) : null;
    const rev = c.rev ? encodeURIComponent(c.rev) : "main";
    const list = [];
    if (r) {
      list.push(["Go to code", r + "/-/tree/" + rev, "g c"], ["Go to changes", r + "/-/commits/" + rev, "g h"],
        ["Go to issues", r + "/-/issues", "g i"], ["Go to reviews", r + "/-/reviews", "g r"],
        ["Go to CI runs", r + "/-/runs", "g x"], ["Go to releases", r + "/-/releases", "g l"],
        ["Go to branches", r + "/-/branches", "g b"], ["Go to activity", r + "/-/activity", "g a"],
        ["Search code", r + "/-/search?rev=" + rev, "/"], ["New issue", r + "/-/issues/new", ""],
        ["New review", r + "/-/reviews/new", ""], ["Repository settings", r + "/-/settings", ""]);
    }
    list.push(["All repositories", "/", "g g"], ["Notifications", "/-/notifications", "g n"], ["Toggle light / dark theme", "#theme", ""]);
    return list;
  }
  async function openPalette(mode) {
    pal.mode = mode;
    const o = $("#palette");
    if (!o) return;
    o.hidden = false;
    const input = $("#palette-input");
    input.value = "";
    input.placeholder = mode === "files" ? "Go to file…" : "Search files, branches, repositories, commands…";
    input.focus();
    renderPalette();
    const c = ctx();
    const loads = [];
    if (c.files) loads.push(getJSON(c.files));
    if (mode === "all") {
      loads.push(getJSON("/-/repos.json"));
      if (c.repo) loads.push(getJSON("/" + encodeURI(c.repo) + "/-/refs.json"));
    }
    await Promise.all(loads);
    if (!o.hidden) renderPalette();
  }
  function closePalette() { const o = $("#palette"); if (o) o.hidden = true; }
  async function renderPalette() {
    const c = ctx();
    const raw = $("#palette-input").value.trim();
    const q = raw.replace(/\s+/g, "").toLowerCase();
    const groups = [];
    const files = c.files ? await pal.data[c.files] : null;
    const blobUrl = (p) => "/" + encodeURI(c.repo) + "/-/blob/" + encodeURIComponent(c.rev) + "/" + p.split("/").map(encodeURIComponent).join("/");
    const rank = (arr, text, n) => {
      if (!q) return arr.slice(0, n).map((x) => [x, []]);
      const scored = [];
      for (const x of arr) { const r = score(text(x), q); if (r) scored.push([r[0], x, r[1]]); }
      scored.sort((a, b) => b[0] - a[0]);
      return scored.slice(0, n).map((s) => [s[1], s[2]]);
    };
    if (pal.mode === "files") {
      if (files) groups.push(["Files", rank(files.paths, (p) => p, 60).map(([p, h]) => ({ html: marked(p, h), href: blobUrl(p), icon: "file", mono: true }))]);
    } else {
      if (c.repo && /^(cs_)?[0-9a-f]{6,64}$/i.test(raw)) {
        groups.push(["Changeset", [{ html: "Go to changeset <b>" + esc(raw) + "</b>", href: "/" + encodeURI(c.repo) + "/-/commit/" + raw.replace(/^cs_/, ""), icon: "commit" }]]);
      }
      groups.push(["Commands", rank(commands(c), (x) => x[0], q ? 5 : 8).map(([x, h]) => ({ html: marked(x[0], h), href: x[1], icon: "go", key: x[2] }))]);
      if (files) groups.push(["Files", rank(files.paths, (p) => p, q ? 8 : 0).map(([p, h]) => ({ html: marked(p, h), href: blobUrl(p), icon: "file", mono: true }))]);
      const refs = c.repo ? await pal.data["/" + encodeURI(c.repo) + "/-/refs.json"] : null;
      if (refs) groups.push(["Branches", rank(refs.refs, (r) => r, q ? 5 : 0).map(([r, h]) => ({ html: marked(r, h), href: "/" + encodeURI(c.repo) + "/-/tree/" + encodeURIComponent(r), icon: "branch", mono: true }))]);
      const repos = await pal.data["/-/repos.json"];
      if (repos) groups.push(["Repositories", rank(repos.repos, (r) => r, q ? 5 : 4).map(([r, h]) => ({ html: marked(r, h), href: "/" + encodeURI(r), icon: "repo" }))]);
      if (c.repo && raw) groups.push(["Search", [{ html: "Search code for <b>" + esc(raw) + "</b>", href: "/" + encodeURI(c.repo) + "/-/search?q=" + encodeURIComponent(raw) + "&rev=" + encodeURIComponent(c.rev || ""), icon: "search" }]]);
    }
    pal.items = [];
    let html = "";
    for (const [name, items] of groups) {
      if (!items.length) continue;
      html += `<li class="group">${name}</li>`;
      for (const it of items) {
        const i = pal.items.length;
        pal.items.push(it);
        html += `<li data-i="${i}" class="${i === 0 ? "on" : ""}"><a href="${it.href}" class="${it.mono ? "mono" : ""}">${ICONS[it.icon] || ""}<span>${it.html}</span>${it.key ? `<span class="k">${esc(it.key)}</span>` : ""}</a></li>`;
      }
    }
    pal.sel = 0;
    $("#palette-list").innerHTML = html || "<li class='muted pad'>Nothing matches</li>";
  }
  function palMove(d) {
    const lis = $$("#palette-list li[data-i]");
    if (!lis.length) return;
    lis[pal.sel] && lis[pal.sel].classList.remove("on");
    pal.sel = (pal.sel + d + lis.length) % lis.length;
    lis[pal.sel].classList.add("on");
    lis[pal.sel].scrollIntoView({ block: "nearest" });
    const a = $("a", lis[pal.sel]);
    if (a && a.getAttribute("href") !== "#theme") fetchPage(a.href);
  }
  function palGo() {
    const it = pal.items[pal.sel];
    if (!it) return;
    closePalette();
    if (it.href === "#theme") return toggleTheme();
    navigate(new URL(it.href, location.href).toString(), true);
  }
  document.addEventListener("input", (ev) => { if (ev.target.id === "palette-input") renderPalette(); });
  document.addEventListener("click", (ev) => {
    if (ev.target.id === "palette") return closePalette();
    if (ev.target.closest && ev.target.closest("[data-palette]")) { ev.preventDefault(); openPalette("all"); return; }
    if (ev.target.closest && ev.target.closest("[data-palette-files]")) { ev.preventDefault(); openPalette("files"); return; }
    const a = ev.target.closest && ev.target.closest("#palette-list a");
    if (a) {
      ev.preventDefault();
      ev.stopPropagation();
      pal.sel = +a.parentElement.dataset.i;
      palGo();
    }
  }, true);

  // ── keyboard ───────────────────────────────────────────────────────
  let gPending = 0;
  document.addEventListener("keydown", (ev) => {
    const palOpen = $("#palette") && !$("#palette").hidden;
    if ((ev.metaKey || ev.ctrlKey) && ev.key.toLowerCase() === "k") {
      ev.preventDefault();
      palOpen ? closePalette() : openPalette("all");
      return;
    }
    if (palOpen) {
      if (ev.key === "Escape") { ev.preventDefault(); closePalette(); }
      else if (ev.key === "ArrowDown" || (ev.ctrlKey && ev.key === "n")) { ev.preventDefault(); palMove(1); }
      else if (ev.key === "ArrowUp" || (ev.ctrlKey && ev.key === "p")) { ev.preventDefault(); palMove(-1); }
      else if (ev.key === "Enter") { ev.preventDefault(); palGo(); }
      return;
    }
    const t = ev.target;
    if (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName) || ev.metaKey || ev.ctrlKey || ev.altKey) return;
    const c = ctx();
    if (gPending && Date.now() - gPending < 900) {
      gPending = 0;
      const r = c.repo ? "/" + encodeURI(c.repo) : null;
      const rev = encodeURIComponent(c.rev || "main");
      const dest = { g: "/", n: "/-/notifications", c: r && r + "/-/tree/" + rev, h: r && r + "/-/commits/" + rev, i: r && r + "/-/issues", r: r && r + "/-/reviews", x: r && r + "/-/runs", l: r && r + "/-/releases", b: r && r + "/-/branches", a: r && r + "/-/activity" }[ev.key];
      if (dest) { ev.preventDefault(); navigate(new URL(dest, location.href).toString(), true); }
      return;
    }
    if (ev.key === "g") { gPending = Date.now(); return; }
    if (ev.key === "t" && c.files) { ev.preventDefault(); openPalette("files"); }
    else if (ev.key === "/") {
      const s = $(".search-row input");
      if (s) { ev.preventDefault(); s.focus(); s.select(); }
      else if (c.repo) { ev.preventDefault(); navigate(new URL("/" + encodeURI(c.repo) + "/-/search?rev=" + encodeURIComponent(c.rev || ""), location.href).toString(), true); }
    } else if (ev.key === "y") {
      const btn = $("[data-copy-url]");
      if (btn) { history.replaceState({}, "", btn.dataset.copyUrl + location.hash); copy(location.href, btn); }
    } else if (ev.key === "Escape") {
      $$("details[open]").forEach((d) => (d.open = false));
    }
  });

  // ── live updates (server-sent events) ─────────────────────────────
  let es = null, esRepo = null;
  function connectLive() {
    const repo = $("main") && $("main").dataset.repo;
    if (repo === esRepo) return;
    if (es) { es.close(); es = null; }
    esRepo = repo;
    if (!repo || !window.EventSource) return;
    es = new EventSource("/" + encodeURI(repo) + "/-/events");
    es.addEventListener("flux", (msg) => {
      let e; try { e = JSON.parse(msg.data); } catch (_) { return; }
      cache.clear();
      const banner = $("#live-banner");
      const path = location.pathname;
      // Activity and review pages refresh themselves.
      if (path.includes("/-/activity") || (e.kind === "review" && path.includes("/-/reviews"))) {
        navigate(location.href, false);
        return;
      }
      if (e.kind === "ci" && /\/-\/runs\/?$/.test(path)) { navigate(location.href, false); return; }
      if (e.kind === "ci" && $("pre.ci-log")) { refreshRun(); return; }
      let text = null;
      if (e.kind === "ref") text = `${e.actor} moved ${e.ref}`;
      else if (e.kind === "review") text = `Review #${e.review} ${e.detail}`;
      else if ((e.kind === "issue" || e.kind === "comment") && path.includes("/-/issues")) text = `${e.actor} updated #${e.review}`;
      if (!text || !banner) return;
      banner.innerHTML = "";
      const span = document.createElement("span");
      span.textContent = text;
      const b = document.createElement("button");
      b.type = "button";
      b.textContent = "Refresh";
      banner.append(span, b);
      banner.hidden = false;
    });
  }

  // ── inline review comments ─────────────────────────────────────────
  document.addEventListener("click", (ev) => {
    const b = ev.target.closest && ev.target.closest(".add-comment");
    if (!b) return;
    ev.preventDefault();
    const row = b.closest("tr");
    const table = b.closest("table");
    if (row.nextElementSibling && row.nextElementSibling.classList.contains("draft")) {
      $("textarea", row.nextElementSibling).focus();
      return;
    }
    const tr = document.createElement("tr");
    tr.className = "thread draft";
    const td = document.createElement("td");
    td.colSpan = 3;
    const box = document.createElement("div");
    box.className = "thread-box";
    const form = document.createElement("form");
    form.method = "post";
    form.action = table.dataset.commentUrl;
    for (const [k, v] of [["path", table.dataset.path], ["line", b.dataset.line]]) {
      const i = document.createElement("input");
      i.type = "hidden"; i.name = k; i.value = v;
      form.append(i);
    }
    const ta = document.createElement("textarea");
    ta.name = "body"; ta.rows = 3; ta.required = true;
    ta.placeholder = "Comment on line " + Math.abs(+b.dataset.line) + " (Markdown)";
    const actions = document.createElement("div");
    actions.className = "form-actions";
    const cancel = document.createElement("button");
    cancel.type = "button"; cancel.className = "btn small"; cancel.textContent = "Cancel";
    cancel.onclick = () => tr.remove();
    const submit = document.createElement("button");
    submit.type = "submit"; submit.className = "btn small primary"; submit.textContent = "Comment";
    actions.append(cancel, submit);
    form.append(ta, actions);
    box.append(form);
    td.append(box);
    tr.append(td);
    row.after(tr);
    ta.focus();
  });
  document.addEventListener("keydown", (ev) => {
    if ((ev.metaKey || ev.ctrlKey) && ev.key === "Enter" && ev.target.tagName === "TEXTAREA" && ev.target.form) {
      ev.preventDefault();
      ev.target.form.requestSubmit();
    }
  });

  // ── diff view mode (unified / split) ──────────────────────────────
  // Split tables are built client-side from the unified rows: context lines
  // go on both sides, runs of deletions pair up with the following additions.
  // Drop the +/- marker and the comment button from a unified cell.
  const clean = (html) => html.replace(/<button class="add-comment"[^>]*>\+<\/button>/, "").replace(/^\s*<span class="mark">.<\/span>/, "");
  function buildSplit(table) {
    const out = document.createElement("table");
    out.className = "diff split-table";
    const cg = document.createElement("colgroup");
    for (const c of ["n", "c", "n", "c"]) { const col = document.createElement("col"); col.className = c; cg.append(col); }
    out.append(cg);
    const cell = (cls, line, html) => {
      const ln = document.createElement("td");
      ln.className = "ln " + cls;
      if (line) ln.dataset.line = line;
      const lc = document.createElement("td");
      lc.className = "lc " + cls;
      if (html !== null) lc.innerHTML = html;
      return [ln, lc];
    };
    for (const body of table.tBodies) {
      const tb = document.createElement("tbody");
      const rows = Array.from(body.rows);
      let i = 0;
      while (i < rows.length) {
        const r = rows[i];
        const tr = document.createElement("tr");
        if (r.classList.contains("hunk")) {
          const td = document.createElement("td");
          td.colSpan = 4;
          td.className = "lc hunk-cell";
          td.textContent = r.cells[2].textContent;
          tr.className = "hunk";
          tr.append(td);
          tb.append(tr);
          i++;
          continue;
        }
        if (r.classList.contains("thread")) {
          const t = r.cloneNode(true);
          t.cells[0].colSpan = 4;
          tb.append(t);
          i++;
          continue;
        }
        if (r.classList.contains("l-ctx")) {
          const html = clean(r.cells[2].innerHTML);
          tr.append(...cell("", r.cells[0].dataset.line, html), ...cell("", r.cells[1].dataset.line, html));
          tb.append(tr);
          i++;
          continue;
        }
        const dels = [], adds = [];
        while (i < rows.length && rows[i].classList.contains("l-del")) dels.push(rows[i++]);
        while (i < rows.length && rows[i].classList.contains("l-add")) adds.push(rows[i++]);
        if (!dels.length && !adds.length) { i++; continue; }
        for (let k = 0; k < Math.max(dels.length, adds.length); k++) {
          const t = document.createElement("tr");
          const d = dels[k], a = adds[k];
          const strip = (row) => clean(row.cells[2].innerHTML);
          t.append(...(d ? cell("s-del", d.cells[0].dataset.line, strip(d)) : cell("s-empty", null, null)));
          t.append(...(a ? cell("s-add", a.cells[1].dataset.line, strip(a)) : cell("s-empty", null, null)));
          tb.append(t);
        }
      }
      out.append(tb);
    }
    return out;
  }
  function applyDiffMode() {
    let mode = "unified";
    try { mode = localStorage.getItem("flux-diff") || "unified"; } catch (_) {}
    $$(".diff-mode button").forEach((x) => x.classList.toggle("active", x.dataset.mode === mode));
    $$("table.diff:not(.split-table)").forEach((t) => {
      let split = t.nextElementSibling && t.nextElementSibling.classList.contains("split-table") ? t.nextElementSibling : null;
      if (mode === "split" && !split) { split = buildSplit(t); t.after(split); }
      t.hidden = mode === "split";
      if (split) split.hidden = mode !== "split";
    });
  }
  document.addEventListener("click", (ev) => {
    const b = ev.target.closest && ev.target.closest(".diff-mode button");
    if (!b) return;
    try { localStorage.setItem("flux-diff", b.dataset.mode); } catch (_) {}
    applyDiffMode();
  });

  // ── editor: Tab inserts indentation ───────────────────────────────
  document.addEventListener("keydown", (ev) => {
    const t = ev.target;
    if (ev.key !== "Tab" || !t.classList || !t.classList.contains("code-editor") || ev.shiftKey) return;
    ev.preventDefault();
    const s = t.selectionStart, e = t.selectionEnd;
    t.setRangeText("    ", s, e, "end");
  });

  // ── per-page setup ────────────────────────────────────────────────
  // Highlighting for a big file was still running when the page rendered:
  // fetch the page again shortly and swap in the highlighted code.
  let hlTimer;
  function pollHighlight(attempt) {
    clearTimeout(hlTimer);
    if (!$(".hl-pending") || attempt > 8) return;
    const url = location.href;
    hlTimer = setTimeout(async () => {
      if (location.href !== url) return;
      cache.delete(url);
      try {
        const html = await fetchPage(url);
        const doc = new DOMParser().parseFromString(html, "text/html");
        const next = $("table.code", doc);
        if (!$(".hl-pending", doc) && next && $("table.code") && location.href === url) {
          $("table.code").replaceWith(next);
          const p = $(".hl-pending"); if (p) p.remove();
          applyLineHash();
        } else pollHighlight(attempt + 1);
      } catch (_) {}
    }, 150 + attempt * 150);
  }

  // ── deferred diffs: load as they near the viewport (or on click) ──
  async function loadDiff(el) {
    if (el.dataset.loading) return;
    el.dataset.loading = "1";
    const b = $("button", el); if (b) b.textContent = "Loading…";
    try {
      const r = await fetch(el.dataset.src, { credentials: "same-origin" });
      if (!r.ok) throw new Error(r.status);
      const html = await r.text();
      const tpl = document.createElement("template");
      tpl.innerHTML = html;
      el.replaceWith(tpl.content);
      applyDiffMode();
    } catch (_) {
      delete el.dataset.loading;
      if (b) b.textContent = "Retry";
    }
  }
  let diffObserver;
  function watchDiffs() {
    if (diffObserver) diffObserver.disconnect();
    const els = $$(".lazy-diff");
    if (!els.length) return;
    if (!("IntersectionObserver" in window)) { els.forEach(loadDiff); return; }
    diffObserver = new IntersectionObserver((entries) => {
      for (const e of entries) if (e.isIntersecting) { diffObserver.unobserve(e.target); loadDiff(e.target); }
    }, { rootMargin: "800px 0px" });
    els.forEach((el) => diffObserver.observe(el));
  }
  document.addEventListener("click", (ev) => {
    const el = ev.target.closest && ev.target.closest(".lazy-diff");
    if (el) loadDiff(el);
  });

  // ── CI: live job logs ────────────────────────────────────────────
  // Each running job's <pre> polls its log endpoint once a second for new
  // output (?after=<seq>); state changes refresh the headers in place.
  const DONE = new Set(["success", "failure", "cancelled", "skipped"]);
  let logTimer;
  async function refreshRun() {
    const url = location.href;
    cache.delete(url);
    try {
      const doc = new DOMParser().parseFromString(await fetchPage(url), "text/html");
      if (location.href !== url) return;
      for (const sel of [".review-head", ".job-nav"]) { const a = $(sel), b = $(sel, doc); if (a && b) a.replaceWith(b); }
      $$(".card.job", doc).forEach((j) => {
        const cur = document.getElementById(j.id);
        const h = cur && $(".job-head", cur), nh = $(".job-head", j);
        if (h && nh) h.replaceWith(nh);
      });
    } catch (_) {}
  }
  function pollLogs() {
    clearTimeout(logTimer);
    if (!$$("pre.ci-log[data-log-src]").some((p) => !DONE.has(p.dataset.state))) return;
    const url = location.href;
    logTimer = setTimeout(async () => {
      if (location.href !== url) return;
      const live = $$("pre.ci-log[data-log-src]").filter((p) => !DONE.has(p.dataset.state));
      let changed = false;
      await Promise.all(live.map(async (p) => {
        try {
          const r = await fetch(p.dataset.logSrc + "?after=" + (p.dataset.seq || 0), { credentials: "same-origin", cache: "no-store" });
          if (!r.ok) return;
          const text = await r.text();
          if (text) {
            const empty = $(".log-empty", p);
            if (empty) empty.remove();
            const stick = p.scrollTop + p.clientHeight >= p.scrollHeight - 24;
            p.append(document.createTextNode(text));
            if (stick) p.scrollTop = p.scrollHeight;
          }
          const seq = r.headers.get("x-flux-log-seq"), state = r.headers.get("x-flux-job-state");
          if (seq) p.dataset.seq = seq;
          if (state && state !== p.dataset.state) { p.dataset.state = state; changed = true; }
        } catch (_) {}
      }));
      if (location.href !== url) return;
      if (changed) await refreshRun();
      pollLogs();
    }, 1000);
  }

  function onPage() {
    pollLogs();
    watchDiffs();
    pollHighlight(0);
    applyLineHash();
    applyDiffMode();
    connectLive();
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", onPage);
  else onPage();
})();
