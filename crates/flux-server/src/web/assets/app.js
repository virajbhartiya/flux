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
    closeFinder();
    navigate(a.href, true);
  });
  window.addEventListener("popstate", () => navigate(location.href, false));

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
    const finderBtn = ev.target.closest && ev.target.closest(".finder-btn");
    if (finderBtn) openFinder();
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
    $$("details.picker[open]").forEach((d) => { if (!d.contains(ev.target)) d.open = false; });
  });

  // ── file finder (t) ────────────────────────────────────────────────
  let files = null, filesUrl = null, results = [], sel = 0;
  function finderUrl() {
    const b = $(".finder-btn");
    return b ? b.dataset.finder : null;
  }
  async function openFinder() {
    const url = finderUrl();
    if (!url) return;
    $("#finder").hidden = false;
    const input = $("#finder-input");
    input.value = "";
    input.focus();
    if (filesUrl !== url) {
      files = null;
      filesUrl = url;
      $("#finder-list").innerHTML = "<li class='muted pad'>Loading…</li>";
      try {
        const r = await fetch(url, { credentials: "same-origin" });
        files = (await r.json()).paths;
      } catch (_) { files = []; }
    }
    renderFinder("");
  }
  function closeFinder() { const f = $("#finder"); if (f) f.hidden = true; }
  function score(path, q) {
    let s = 0, qi = 0, prev = -2;
    const lower = path.toLowerCase(), base = path.lastIndexOf("/") + 1;
    const hits = [];
    for (let i = 0; i < lower.length && qi < q.length; i++) {
      if (lower[i] === q[qi]) {
        let v = 1;
        if (prev === i - 1) v += 5;
        if (i === 0 || "/_-.".includes(path[i - 1])) v += 4;
        if (i >= base) v += 2;
        s += v; prev = i; qi++; hits.push(i);
      }
    }
    return qi === q.length ? [s * 100 - path.length, hits] : null;
  }
  function esc(s) { return s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]); }
  function renderFinder(q) {
    const list = $("#finder-list");
    if (!files) return;
    q = q.replace(/\s+/g, "").toLowerCase();
    if (!q) results = files.slice(0, 50).map((p) => [p, []]);
    else {
      const scored = [];
      for (const p of files) { const r = score(p, q); if (r) scored.push([r[0], p, r[1]]); }
      scored.sort((a, b) => b[0] - a[0]);
      results = scored.slice(0, 50).map((x) => [x[1], x[2]]);
    }
    sel = 0;
    const repo = $("main").dataset.repo, rev = $("main").dataset.rev;
    list.innerHTML = results.map(([p, hits], i) => {
      const set = new Set(hits);
      let out = "";
      for (let k = 0; k < p.length; k++) out += set.has(k) ? "<b>" + esc(p[k]) + "</b>" : esc(p[k]);
      const href = "/" + encodeURI(repo) + "/-/blob/" + encodeURIComponent(rev) + "/" + p.split("/").map(encodeURIComponent).join("/");
      return `<li class="${i === 0 ? "on" : ""}"><a href="${href}">${out}</a></li>`;
    }).join("") || "<li class='muted pad'>No matching files</li>";
  }
  document.addEventListener("input", (ev) => { if (ev.target.id === "finder-input") renderFinder(ev.target.value); });
  document.addEventListener("click", (ev) => { if (ev.target.id === "finder") closeFinder(); });

  // ── keyboard ───────────────────────────────────────────────────────
  document.addEventListener("keydown", (ev) => {
    const finderOpen = !$("#finder").hidden;
    if (finderOpen) {
      const items = $$("#finder-list li");
      if (ev.key === "Escape") { closeFinder(); return; }
      if (ev.key === "ArrowDown" || ev.key === "ArrowUp") {
        ev.preventDefault();
        if (!items.length) return;
        items[sel] && items[sel].classList.remove("on");
        sel = (sel + (ev.key === "ArrowDown" ? 1 : items.length - 1)) % items.length;
        items[sel].classList.add("on");
        items[sel].scrollIntoView({ block: "nearest" });
        const a = $("a", items[sel]); if (a) fetchPage(a.href);
        return;
      }
      if (ev.key === "Enter") {
        ev.preventDefault();
        const a = items[sel] && $("a", items[sel]);
        if (a) { closeFinder(); navigate(a.href, true); }
      }
      return;
    }
    const t = ev.target;
    if (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName) || ev.metaKey || ev.ctrlKey || ev.altKey) return;
    if (ev.key === "t") { ev.preventDefault(); openFinder(); }
    else if (ev.key === "/") {
      const s = $(".top-search input") || $(".search-row input");
      if (s) { ev.preventDefault(); s.focus(); s.select(); }
    } else if (ev.key === "y") {
      // Permalink: pin the current branch URL to its changeset.
      const btn = $("[data-copy-url]");
      if (btn) { history.replaceState({}, "", btn.dataset.copyUrl + location.hash); copy(location.href, btn); }
    } else if (ev.key === "Escape") {
      $$("details.picker[open]").forEach((d) => (d.open = false));
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
      let text = null;
      if (e.kind === "ref") text = `${e.actor} moved ${e.ref}`;
      else if (e.kind === "review") text = `Review #${e.review} ${e.detail}`;
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

  // ── diff view mode (unified / split) ──────────────────────────────
  // Split tables are built client-side from the unified rows: context lines
  // go on both sides, runs of deletions pair up with the following additions.
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
        if (r.classList.contains("l-ctx")) {
          const html = r.cells[2].innerHTML.replace(/^<span class="mark">.<\/span>/, "");
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
          const strip = (row) => row.cells[2].innerHTML.replace(/^<span class="mark">.<\/span>/, "");
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

  function onPage() {
    pollHighlight(0);
    applyLineHash();
    applyDiffMode();
    connectLive();
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", onPage);
  else onPage();
})();
