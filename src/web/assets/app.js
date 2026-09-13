// zerch web UI — vanilla JS, no build step. The URL query string *is* the view
// token; we edit it with URLSearchParams (a spec form-urlencoder, so it round-trips
// the server's parse_view/serialize_view format) and mirror it with replaceState so
// every view is a shareable link.
"use strict";

(function () {
  const boot = (function () {
    try {
      return JSON.parse(document.getElementById("zerch-bootstrap").textContent);
    } catch (_e) {
      return { file: "", total: 0, view: "" };
    }
  })();

  const ROW_H = 24;
  const OVERSCAN = 8;
  const SAFE_MAX = 15000000;

  const $ = (id) => document.getElementById(id);
  const scroller = $("scroller");
  const spacer = $("spacer");
  const rowsEl = $("rows");
  const logViewer = $("log-viewer");
  const searchInput = $("search");
  const searchClear = $("search-clear");
  const searchBtn = $("search-btn");
  const regexToggle = $("regex-toggle");
  const counter = $("match-counter");
  const matchN = $("match-n");
  const filterList = $("filter-list");
  const highlightList = $("highlight-list");
  const rangeList = $("range-list");
  const rangeInput = $("range-input");
  const modeToggle = $("mode-toggle");
  const levelChips = $("level-chips");
  const rightPanel = $("right-panel");

  // Level colours — must match render::level_hex / levels.ts (index = level code).
  const LEVEL_COLORS = [
    "#90a4ae", "#9e9e9e", "#78909c", "#4caf50", "#ffa726", "#ef5350", "#c2185b",
  ];

  // Highlight palette — must match view::HIGHLIGHT_PALETTE (parse_view assigns colours
  // by index, so the server and client agree on which colour each highlight gets).
  const PALETTE = [
    "#4fc3f7", "#81c784", "#ba68c8", "#ff8a65", "#f06292",
    "#4db6ac", "#9575cd", "#aed581", "#ffb74d", "#a1887f",
  ];

  $("file-name").textContent = boot.file || "(stdin)";

  const state = {
    view: boot.view || "",
    total: boot.total || 0,
    fileLines: boot.total || 0,
    maxlen: boot.maxlen || 0,
    matches: [],
    matchIdx: -1,
    bmIdx: -1,
    bookmarkPos: [],
    visiblePos: [],
    selectedPos: -1,
    hscrolledMatch: -1,
    selectedText: "",
    regexOn: false,
    counts: { filters: [], highlights: [] },
    showMinimap: true,
  };
  const minimap = $("minimap");
  $("total-lines").textContent = state.fileLines.toLocaleString() + " lines";

  const gutterW = Math.max(3, String(state.fileLines).length) * 8 + 28;
  document.documentElement.style.setProperty("--gutter-width", gutterW + "px");

  // Content width for horizontal scrolling of long lines: gutter + the longest line
  // measured at the real font. Rows stretch to this width; the gutter stays sticky.
  (function setContentWidth() {
    const cvs = document.createElement("canvas");
    const cx = cvs.getContext("2d");
    cx.font = '13px "Courier New", Courier, monospace';
    const charW = cx.measureText("0").width || 7.8;
    const w = gutterW + Math.ceil(state.maxlen * charW) + 40;
    document.documentElement.style.setProperty("--content-width", w + "px");
  })();

  // ---- view token helpers -----------------------------------------------
  const tokenParams = () => new URLSearchParams(state.view);
  function commitToken(p) {
    state.view = p.toString();
    const path = window.location.pathname;
    history.replaceState(null, "", state.view ? path + "?" + state.view : path);
  }

  // ---- virtual list ------------------------------------------------------
  let viewportH = scroller.clientHeight;
  let domH = 0;
  let scale = 1;

  function layout() {
    viewportH = scroller.clientHeight;
    const virtualH = state.total * ROW_H;
    domH = Math.min(virtualH, SAFE_MAX);
    scale =
      virtualH > domH && domH > viewportH ? (virtualH - viewportH) / (domH - viewportH) : 1;
    spacer.style.height = Math.max(domH, 1) + "px";
  }

  function makeRow(item, index, topPx, isCurrent, bmSet, selLine) {
    if (item.t === "g") {
      const g = document.createElement("div");
      g.className = "gap-row";
      g.style.top = topPx + "px";
      const inner = document.createElement("div");
      inner.className = "gap-inner";
      inner.style.width = scroller.clientWidth + "px";
      const below = document.createElement("button");
      below.className = "gap-btn";
      below.textContent = "↓5";
      below.title = "reveal 5 from the top of the gap";
      below.addEventListener("click", () =>
        revealRange(item.start, Math.min(item.end, item.start + 4))
      );
      const label = document.createElement("span");
      label.textContent =
        " ··· " + item.count.toLocaleString() + " hidden (" + item.start + "–" + item.end + ") ··· ";
      const above = document.createElement("button");
      above.className = "gap-btn";
      above.textContent = "↑5";
      above.title = "reveal 5 from the bottom of the gap";
      above.addEventListener("click", () =>
        revealRange(Math.max(item.start, item.end - 4), item.end)
      );
      inner.appendChild(below);
      inner.appendChild(label);
      inner.appendChild(above);
      g.appendChild(inner);
      return g;
    }
    const bookmarked = bmSet.has(item.n);
    const row = document.createElement("div");
    row.className =
      "line-row" +
      (index % 2 === 1 ? " zebra" : "") +
      (bookmarked ? " bookmarked" : "") +
      (item.n === selLine ? " selected" : "");
    row.style.top = topPx + "px";
    row.dataset.n = item.n;
    row.addEventListener("dblclick", () => setSelected(item.n));

    const gutter = document.createElement("div");
    gutter.className = "line-number";
    gutter.style.boxShadow = "inset 3px 0 0 " + item.lvl;
    if (bookmarked) {
      const star = document.createElement("span");
      star.className = "star";
      star.textContent = "★";
      gutter.appendChild(star);
    }
    gutter.appendChild(document.createTextNode(item.n));
    gutter.title = "click: bookmark · shift-click: ±5 context";
    gutter.addEventListener("click", (e) => {
      if (e.shiftKey) revealContext(item.n);
      else toggleBookmark(item.n);
    });

    const content = document.createElement("div");
    content.className = "line-content";
    content.innerHTML = item.html; // server-rendered + server-escaped
    if (isCurrent) content.querySelectorAll(".s").forEach((s) => s.classList.add("s-cur"));

    const actions = document.createElement("span");
    actions.className = "line-actions";
    const hideBtn = document.createElement("button");
    hideBtn.className = "act";
    hideBtn.textContent = "×";
    hideBtn.title = "Hide this line";
    hideBtn.addEventListener("click", () => hideLine(item.n));
    actions.appendChild(hideBtn);
    content.appendChild(actions);

    row.appendChild(gutter);
    row.appendChild(content);
    return row;
  }

  let painted = { from: -1, to: -1 };
  let inflight = null;

  function visibleRange() {
    const virtualTop = scroller.scrollTop * scale;
    const first = Math.max(0, Math.floor(virtualTop / ROW_H) - OVERSCAN);
    const count = Math.ceil(viewportH / ROW_H) + OVERSCAN * 2;
    const last = Math.min(state.total, first + count);
    return { first, last };
  }

  function paint(rows, from) {
    const domTop = scroller.scrollTop;
    const virtualTop = domTop * scale;
    const curPos =
      state.matchIdx >= 0 && state.matches[state.matchIdx] != null
        ? state.matches[state.matchIdx]
        : -1;
    const bmSet = new Set(parseRangeSpec(tokenParams().get("bookmarks")));
    const selLine = parseInt(tokenParams().get("selected") || "", 10);
    const frag = document.createDocumentFragment();
    let curRow = null;
    for (let k = 0; k < rows.length; k++) {
      const index = from + k;
      const topPx = domTop + (index * ROW_H - virtualTop);
      const node = makeRow(rows[k], index, topPx, index === curPos, bmSet, selLine);
      if (index === curPos) curRow = node;
      frag.appendChild(node);
    }
    rowsEl.textContent = "";
    rowsEl.appendChild(frag);
    // Bring the current search match into horizontal view (once per match change),
    // so a hit far to the right of a long line is visible.
    if (curRow && curPos !== state.hscrolledMatch) {
      state.hscrolledMatch = curPos;
      const mark = curRow.querySelector(".s-cur");
      if (mark) mark.scrollIntoView({ inline: "center", block: "nearest" });
    }
  }

  function refresh(force) {
    const { first, last } = visibleRange();
    if (!force && first === painted.from && last === painted.to) return;
    painted = { from: first, to: last };
    let url =
      "/rows?view=" + encodeURIComponent(state.view) + "&from=" + first + "&to=" + last;
    if (state.selectedText) url += "&sel=" + encodeURIComponent(state.selectedText);
    if (inflight) inflight.abort();
    const ac = new AbortController();
    inflight = ac;
    fetch(url, { signal: ac.signal })
      .then((r) => r.json())
      .then((data) => {
        inflight = null;
        if (data.total !== state.total) {
          state.total = data.total;
          layout();
        }
        paint(data.rows, data.from);
        drawMinimap();
      })
      .catch((e) => {
        if (e.name !== "AbortError") console.error(e);
      });
  }

  function scrollToPos(pos) {
    const domY = (pos * ROW_H) / scale;
    let top = domY - viewportH / 3;
    top = Math.max(0, Math.min(top, spacer.offsetHeight - viewportH));
    scroller.scrollTop = top;
    refresh(true);
  }

  // ---- minimap -----------------------------------------------------------
  function drawLane(ctx, positions, x, color, total, H) {
    ctx.fillStyle = color;
    let last = -1;
    for (const p of positions) {
      const y = Math.floor((p / total) * H); // one tick per pixel row (bucketed)
      if (y === last) continue;
      last = y;
      ctx.fillRect(x, y, 6, 3);
    }
  }
  function drawMinimap() {
    if (!state.showMinimap) return;
    const rect = minimap.getBoundingClientRect();
    const W = rect.width;
    const H = rect.height;
    if (!W || !H) return;
    const dpr = window.devicePixelRatio || 1;
    minimap.width = W * dpr;
    minimap.height = H * dpr;
    const ctx = minimap.getContext("2d");
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, W, H);
    const total = Math.max(state.total, 1);
    drawLane(ctx, state.bookmarkPos, 0, "#f1c40f", total, H); // bookmarks (gold)
    drawLane(ctx, state.matches, 6, "#e67e22", total, H); // search matches (orange)
    drawLane(ctx, state.visiblePos, 12, "#16a085", total, H); // revealed ranges (teal)
    if (state.selectedPos >= 0) {
      ctx.fillStyle = "#2980b9"; // selected line (blue, full width)
      ctx.fillRect(0, Math.floor((state.selectedPos / total) * H), W, 2);
    }
    // viewport box
    const topPos = (scroller.scrollTop * scale) / ROW_H;
    const y = (topPos / total) * H;
    const h = Math.max(3, (viewportH / ROW_H / total) * H);
    ctx.fillStyle = "rgba(52,152,219,0.14)";
    ctx.fillRect(0, y, W, h);
    ctx.strokeStyle = "rgba(41,128,185,0.55)";
    ctx.strokeRect(0.5, y + 0.5, W - 1, Math.max(1, h - 1));
  }
  // Resolve the bookmark / visible-range / selected-line positions for the minimap
  // in one /positions request (raw line numbers -> displayed positions).
  function resolveMarkers() {
    const p = tokenParams();
    const bms = parseRangeSpec(p.get("bookmarks"));
    const vis = parseRangeSpec(p.get("visible"));
    const sel = parseInt(p.get("selected") || "", 10);
    const lines = [...new Set([...bms, ...vis, ...(isNaN(sel) ? [] : [sel])])];
    if (!lines.length) {
      state.bookmarkPos = [];
      state.visiblePos = [];
      state.selectedPos = -1;
      drawMinimap();
      return;
    }
    fetch("/positions?view=" + encodeURIComponent(state.view) + "&lines=" + lines.join(","))
      .then((r) => r.json())
      .then((d) => {
        const m = d.positions || {};
        const at = (l) => (m[l] != null ? m[l] : null);
        state.bookmarkPos = bms.map(at).filter((x) => x != null).sort((a, b) => a - b);
        state.visiblePos = vis.map(at).filter((x) => x != null).sort((a, b) => a - b);
        state.selectedPos = !isNaN(sel) && m[sel] != null ? m[sel] : -1;
        drawMinimap();
      })
      .catch(() => {});
  }
  minimap.addEventListener("click", function (e) {
    const rect = minimap.getBoundingClientRect();
    const frac = (e.clientY - rect.top) / rect.height;
    const pos = Math.max(0, Math.min(state.total - 1, Math.floor(frac * state.total)));
    scrollToPos(pos);
  });

  // ---- search + match navigation ----------------------------------------
  function updateCounter() {
    const term = tokenParams().get("search") || "";
    if (!term) {
      counter.hidden = true;
      return;
    }
    counter.hidden = false;
    matchN.textContent = state.matches.length
      ? state.matchIdx + 1 + " / " + state.matches.length
      : "0 found";
  }

  function fetchMatches(jump) {
    const term = tokenParams().get("search") || "";
    if (!term) {
      state.matches = [];
      state.matchIdx = -1;
      updateCounter();
      refresh(true);
      return;
    }
    fetch("/matches?view=" + encodeURIComponent(state.view))
      .then((r) => r.json())
      .then((d) => {
        state.matches = d.positions || [];
        if (state.matches.length === 0) {
          state.matchIdx = -1;
        } else if (jump) {
          const top = Math.floor((scroller.scrollTop * scale) / ROW_H);
          let idx = state.matches.findIndex((p) => p >= top);
          if (idx < 0) idx = 0;
          state.matchIdx = idx;
          scrollToPos(state.matches[idx]);
        }
        updateCounter();
        refresh(true);
      })
      .catch((e) => console.error(e));
  }

  function goMatch(delta) {
    if (state.matches.length === 0) return;
    state.matchIdx =
      (state.matchIdx + delta + state.matches.length) % state.matches.length;
    scrollToPos(state.matches[state.matchIdx]);
    updateCounter();
  }

  // ---- recent-search history (localStorage, like the TUI's history) ------
  const HKEY = "zerch-search-history";
  const searchHistoryEl = $("search-history");
  function loadHistory() {
    try {
      return JSON.parse(localStorage.getItem(HKEY) || "[]");
    } catch (_e) {
      return [];
    }
  }
  function saveHistory(list) {
    try {
      localStorage.setItem(HKEY, JSON.stringify(list.slice(0, 100)));
    } catch (_e) {}
  }
  function rememberTerm(term) {
    if (!term) return;
    const list = loadHistory().filter((t) => t !== term);
    list.unshift(term);
    saveHistory(list);
  }
  function showHistory() {
    const q = searchInput.value.toLowerCase();
    const list = loadHistory().filter((t) => !q || t.toLowerCase().includes(q));
    searchHistoryEl.textContent = "";
    if (list.length === 0) {
      searchHistoryEl.hidden = true;
      return;
    }
    list.forEach((term) => {
      const row = document.createElement("div");
      row.className = "sh-item";
      const t = document.createElement("span");
      t.className = "t";
      t.textContent = term;
      const x = document.createElement("button");
      x.className = "x";
      x.textContent = "×";
      // mousedown, not click, so the action beats the input's blur.
      row.addEventListener("mousedown", (e) => {
        e.preventDefault();
        searchInput.value = term;
        searchClear.hidden = false;
        hideHistory();
        runSearch();
      });
      x.addEventListener("mousedown", (e) => {
        e.preventDefault();
        e.stopPropagation();
        saveHistory(loadHistory().filter((h) => h !== term));
        showHistory();
      });
      row.appendChild(t);
      row.appendChild(x);
      searchHistoryEl.appendChild(row);
    });
    const foot = document.createElement("div");
    foot.className = "sh-foot";
    foot.textContent = "Clear history";
    foot.addEventListener("mousedown", (e) => {
      e.preventDefault();
      saveHistory([]);
      hideHistory();
    });
    searchHistoryEl.appendChild(foot);
    searchHistoryEl.hidden = false;
  }
  const hideHistory = () => (searchHistoryEl.hidden = true);

  function runSearch() {
    const term = searchInput.value;
    rememberTerm(term.trim());
    hideHistory();
    const p = tokenParams();
    if (term) p.set("search", term);
    else p.delete("search");
    if (term && state.regexOn) p.set("srx", "1");
    else p.delete("srx");
    commitToken(p);
    painted = { from: -1, to: -1 };
    fetchMatches(true);
  }

  function clearSearch() {
    searchInput.value = "";
    searchClear.hidden = true;
    const p = tokenParams();
    p.delete("search");
    p.delete("srx");
    commitToken(p);
    state.matches = [];
    state.matchIdx = -1;
    updateCounter();
    painted = { from: -1, to: -1 };
    refresh(true);
  }

  // ---- filters & highlights (right panel) --------------------------------
  const splitList = (v) => (v ? v.split(",") : []);
  function bits(v, len, dflt) {
    const raw = splitList(v);
    const out = [];
    for (let i = 0; i < len; i++) out.push(raw[i] === undefined ? dflt : raw[i] === "1");
    return out;
  }
  const setBits = (p, key, arr) =>
    arr.some((x) => x) ? p.set(key, arr.map((x) => (x ? "1" : "0")).join(",")) : p.delete(key);

  function readFilters(p) {
    const terms = splitList(p.get("filters"));
    const en = bits(p.get("enabled"), terms.length, true);
    const fc = bits(p.get("fcase"), terms.length, false);
    const fr = bits(p.get("frx"), terms.length, false);
    const fn = bits(p.get("fneg"), terms.length, false);
    return terms.map((t, i) => ({
      term: t,
      enabled: en[i],
      case: fc[i],
      regex: fr[i],
      negate: fn[i],
    }));
  }
  function writeFilters(p, filters) {
    if (filters.length === 0) {
      ["filters", "enabled", "mode", "fcase", "frx", "fneg"].forEach((k) => p.delete(k));
      return;
    }
    p.set("filters", filters.map((f) => f.term).join(","));
    p.set("enabled", filters.map((f) => (f.enabled ? "1" : "0")).join(","));
    p.set("mode", p.get("mode") === "OR" ? "OR" : "AND");
    setBits(p, "fcase", filters.map((f) => f.case));
    setBits(p, "frx", filters.map((f) => f.regex));
    setBits(p, "fneg", filters.map((f) => f.negate));
  }

  function readHighlights(p) {
    const terms = splitList(p.get("highlights"));
    const hc = bits(p.get("hcase"), terms.length, false);
    const hr = bits(p.get("hrx"), terms.length, false);
    const he = bits(p.get("henabled"), terms.length, true);
    return terms.map((t, i) => ({
      term: t,
      case: hc[i],
      regex: hr[i],
      enabled: he[i],
      color: PALETTE[i % PALETTE.length],
    }));
  }
  function writeHighlights(p, hs) {
    if (hs.length === 0) {
      ["highlights", "hcase", "hrx", "henabled"].forEach((k) => p.delete(k));
      return;
    }
    p.set("highlights", hs.map((h) => h.term).join(","));
    setBits(p, "hcase", hs.map((h) => h.case));
    setBits(p, "hrx", hs.map((h) => h.regex));
    // henabled is emitted only when some highlight is disabled (matches serialize_view).
    if (hs.some((h) => !h.enabled)) p.set("henabled", hs.map((h) => (h.enabled ? "1" : "0")).join(","));
    else p.delete("henabled");
  }

  // Apply a mutation to the current filter/highlight lists, commit, and refresh.
  function editLists(mutate, displayedChanged) {
    const p = tokenParams();
    const filters = readFilters(p);
    const highlights = readHighlights(p);
    mutate(filters, highlights, p);
    writeFilters(p, filters);
    writeHighlights(p, highlights);
    commitToken(p);
    painted = { from: -1, to: -1 };
    refresh(true);
    renderPanel();
    fetchCounts();
    if (displayedChanged && (tokenParams().get("search") || "")) fetchMatches(false);
  }

  function tglBtn(label, on, cls, onClick) {
    const b = document.createElement("button");
    b.className = "tgl" + (cls ? " " + cls : "") + (on ? " on" : "");
    b.textContent = label;
    b.addEventListener("click", onClick);
    return b;
  }
  function countBadge(val) {
    const c = document.createElement("span");
    c.className = "count" + (val === -1 ? " err" : "");
    c.textContent = val === undefined || val === null ? "" : val === -1 ? "!" : val.toLocaleString();
    return c;
  }
  function fetchCounts() {
    fetch("/counts?view=" + encodeURIComponent(state.view))
      .then((r) => r.json())
      .then((d) => {
        state.counts = d;
        renderPanel();
      })
      .catch(() => {});
  }

  function renderPanel() {
    const p = tokenParams();
    const filters = readFilters(p);
    const highlights = readHighlights(p);
    modeToggle.textContent = p.get("mode") === "OR" ? "OR" : "AND";
    modeToggle.classList.toggle("or", p.get("mode") === "OR");

    filterList.textContent = "";
    if (filters.length === 0) {
      const e = document.createElement("div");
      e.className = "empty";
      e.textContent = "No filters";
      filterList.appendChild(e);
    }
    filters.forEach((f, i) => {
      const row = document.createElement("div");
      row.className = "prow" + (f.enabled ? "" : " off");
      const en = document.createElement("input");
      en.type = "checkbox";
      en.checked = f.enabled;
      en.className = "en";
      en.addEventListener("change", () =>
        editLists((fs) => (fs[i].enabled = en.checked), true)
      );
      const term = document.createElement("span");
      term.className = "term" + (f.negate ? " neg" : "");
      term.textContent = f.term;
      row.appendChild(en);
      row.appendChild(term);
      row.appendChild(tglBtn("Aa", f.case, "case", () => editLists((fs) => (fs[i].case = !fs[i].case), true)));
      row.appendChild(tglBtn(".*", f.regex, "rx", () => editLists((fs) => (fs[i].regex = !fs[i].regex), true)));
      row.appendChild(tglBtn("≠", f.negate, "neg", () => editLists((fs) => (fs[i].negate = !fs[i].negate), true)));
      row.appendChild(countBadge(state.counts.filters[i]));
      const rm = document.createElement("button");
      rm.className = "rm";
      rm.textContent = "×";
      rm.addEventListener("click", () => editLists((fs) => fs.splice(i, 1), true));
      row.appendChild(rm);
      filterList.appendChild(row);
    });

    highlightList.textContent = "";
    if (highlights.length === 0) {
      const e = document.createElement("div");
      e.className = "empty";
      e.textContent = "No highlights";
      highlightList.appendChild(e);
    }
    highlights.forEach((h, i) => {
      const row = document.createElement("div");
      row.className = "prow" + (h.enabled ? "" : " off");
      const en = document.createElement("input");
      en.type = "checkbox";
      en.checked = h.enabled;
      en.className = "en";
      en.addEventListener("change", () => editLists((_f, hs) => (hs[i].enabled = en.checked), false));
      const sw = document.createElement("span");
      sw.className = "swatch";
      sw.style.background = h.color;
      const term = document.createElement("span");
      term.className = "term";
      term.textContent = h.term;
      row.appendChild(en);
      row.appendChild(sw);
      row.appendChild(term);
      row.appendChild(tglBtn("Aa", h.case, "case", () => editLists((_f, hs) => (hs[i].case = !hs[i].case), false)));
      row.appendChild(tglBtn(".*", h.regex, "rx", () => editLists((_f, hs) => (hs[i].regex = !hs[i].regex), false)));
      row.appendChild(countBadge(state.counts.highlights[i]));
      const rm = document.createElement("button");
      rm.className = "rm";
      rm.textContent = "×";
      rm.addEventListener("click", () => editLists((_f, hs) => hs.splice(i, 1), false));
      row.appendChild(rm);
      highlightList.appendChild(row);
    });

    renderRanges();
    renderLevelChips();
  }

  function addFilter() {
    const term = searchInput.value.trim();
    if (!term) return;
    editLists((fs) => {
      if (!fs.some((f) => f.term === term))
        fs.push({ term, enabled: true, case: false, regex: state.regexOn, negate: false });
    }, true);
    searchInput.value = "";
    searchClear.hidden = true;
  }
  function addHighlight() {
    const term = searchInput.value.trim();
    if (!term) return;
    editLists((_f, hs) => {
      if (!hs.some((h) => h.term === term))
        hs.push({ term, case: false, regex: state.regexOn, enabled: true });
    }, false);
    searchInput.value = "";
    searchClear.hidden = true;
  }

  // ---- bookmarks, visible ranges, hidden lines (token sets) --------------
  function parseRangeSpec(v) {
    const out = [];
    if (!v) return out;
    v.split(",").forEach((piece) => {
      const m = /^(\d+)(?:-(\d+))?$/.exec(piece.trim());
      if (m) {
        const lo = +m[1];
        const hi = m[2] ? +m[2] : lo;
        for (let n = Math.min(lo, hi); n <= Math.max(lo, hi); n++) out.push(n);
      }
    });
    return out;
  }
  function encodeRuns(nums) {
    const s = [...new Set(nums)].sort((a, b) => a - b);
    const out = [];
    let i = 0;
    while (i < s.length) {
      let j = i;
      while (j + 1 < s.length && s[j + 1] === s[j] + 1) j++;
      out.push(i === j ? String(s[i]) : s[i] + "-" + s[j]);
      i = j + 1;
    }
    return out.join(",");
  }
  const currentSets = (p) => ({
    bookmarks: new Set(parseRangeSpec(p.get("bookmarks"))),
    visible: new Set(parseRangeSpec(p.get("visible"))),
    hidden: new Set(parseRangeSpec(p.get("hidden"))),
  });
  function writeSets(p, s) {
    const bm = [...s.bookmarks].sort((a, b) => a - b);
    if (bm.length) p.set("bookmarks", bm.join(","));
    else p.delete("bookmarks");
    const vis = encodeRuns([...s.visible]);
    if (vis) p.set("visible", vis);
    else p.delete("visible");
    const hid = encodeRuns([...s.hidden]);
    if (hid) p.set("hidden", hid);
    else p.delete("hidden");
  }
  function editSets(mutate) {
    const p = tokenParams();
    const s = currentSets(p);
    mutate(s);
    writeSets(p, s);
    commitToken(p);
    painted = { from: -1, to: -1 };
    refresh(true);
    renderPanel();
    resolveMarkers();
    if (tokenParams().get("search") || "") fetchMatches(false);
  }
  function toggleBookmark(n) {
    editSets((s) => (s.bookmarks.has(n) ? s.bookmarks.delete(n) : s.bookmarks.add(n)));
  }
  function hideLine(n) {
    editSets((s) => {
      s.hidden.add(n);
      s.bookmarks.delete(n);
      s.visible.delete(n);
    });
  }
  function revealRange(a, b) {
    editSets((s) => {
      for (let n = a; n <= b; n++) {
        s.visible.add(n);
        s.hidden.delete(n);
      }
    });
  }
  const revealContext = (n) =>
    revealRange(Math.max(1, n - 5), Math.min(state.fileLines, n + 5));

  // Double-click anchors a line as the "selected" cursor line (persisted in the
  // token, so it rides along in a shared URL — matching the TUI's cursor line).
  function setSelected(n) {
    const p = tokenParams();
    p.set("selected", String(n));
    commitToken(p);
    painted = { from: -1, to: -1 };
    refresh(true);
    resolveMarkers();
  }

  function gotoLine(line) {
    fetch("/goto?view=" + encodeURIComponent(state.view) + "&line=" + line)
      .then((r) => (r.ok ? r.json() : null))
      .then((d) => {
        if (d) scrollToPos(d.pos);
      })
      .catch(() => {});
  }
  function goBookmark(delta) {
    const bms = parseRangeSpec(tokenParams().get("bookmarks")).sort((a, b) => a - b);
    if (!bms.length) return;
    state.bmIdx = (state.bmIdx + delta + bms.length) % bms.length;
    gotoLine(bms[state.bmIdx]);
  }

  function renderRanges() {
    const p = tokenParams();
    const s = currentSets(p);
    rangeList.textContent = "";
    const bms = [...s.bookmarks].sort((a, b) => a - b);
    const vis = [...s.visible].sort((a, b) => a - b);
    if (bms.length === 0 && vis.length === 0) {
      const e = document.createElement("div");
      e.className = "empty";
      e.textContent = "No bookmarks or revealed ranges";
      rangeList.appendChild(e);
      return;
    }
    bms.forEach((n) =>
      rangeList.appendChild(
        rangeRow("★", n, () => toggleBookmark(n), n, () =>
          // ★ → move this bookmark to a plain revealed line.
          editSets((st) => {
            st.bookmarks.delete(n);
            st.visible.add(n);
          })
        )
      )
    );
    // Coalesce visible lines into run labels for display.
    encodeRuns(vis)
      .split(",")
      .filter(Boolean)
      .forEach((label) => {
        const first = +label.split("-")[0];
        rangeList.appendChild(
          rangeRow(
            "☆",
            label,
            () => editSets((st) => parseRangeSpec(label).forEach((n) => st.visible.delete(n))),
            first,
            () =>
              // ☆ → promote this revealed run to bookmarks.
              editSets((st) =>
                parseRangeSpec(label).forEach((n) => {
                  st.visible.delete(n);
                  st.bookmarks.add(n);
                })
              )
          )
        );
      });
  }
  function rangeRow(icon, label, onRemove, jumpLine, onToggleKind) {
    const row = document.createElement("div");
    row.className = "prow";
    const ic = document.createElement("button");
    ic.className = "star-btn";
    ic.textContent = icon;
    ic.title = icon === "★" ? "Convert to a revealed range" : "Promote to a bookmark";
    ic.addEventListener("click", onToggleKind);
    const term = document.createElement("span");
    term.className = "term";
    term.textContent = " " + label;
    const jump = document.createElement("button");
    jump.className = "jump";
    jump.textContent = "→";
    jump.title = "Jump";
    jump.addEventListener("click", () => gotoLine(jumpLine));
    const rm = document.createElement("button");
    rm.className = "rm";
    rm.textContent = "×";
    rm.addEventListener("click", onRemove);
    row.appendChild(ic);
    row.appendChild(term);
    row.appendChild(jump);
    row.appendChild(rm);
    return row;
  }
  function addRangeFromInput() {
    const nums = parseRangeSpec(rangeInput.value);
    if (nums.length) {
      editSets((s) => nums.forEach((n) => {
        if (n >= 1 && n <= state.fileLines) {
          s.visible.add(n);
          s.hidden.delete(n);
        }
      }));
      rangeInput.value = "";
    }
  }

  // ---- level filter chips ------------------------------------------------
  let levelData = null; // { counts:[c0..c6], names:[...] }
  function fetchLevels() {
    fetch("/levels")
      .then((r) => r.json())
      .then((d) => {
        levelData = d;
        renderLevelChips();
      })
      .catch(() => {});
  }
  const activeLevels = () => {
    const v = tokenParams().get("levels");
    return v ? v.split(",").map(Number).filter((n) => !isNaN(n)) : [];
  };
  function presentLevels() {
    const out = [];
    if (!levelData) return out;
    for (let c = 0; c <= 6; c++) if ((levelData.counts[c] || 0) > 0) out.push(c);
    return out;
  }
  // The token stores the restrict-to set; empty (or all present) = no restriction.
  function setLevels(list) {
    const present = presentLevels();
    const sorted = [...new Set(list)].sort((a, b) => a - b);
    const p = tokenParams();
    if (sorted.length === 0 || sorted.length === present.length) p.delete("levels");
    else p.set("levels", sorted.join(","));
    commitToken(p);
    painted = { from: -1, to: -1 };
    refresh(true);
    renderPanel();
    resolveMarkers();
    if (tokenParams().get("search") || "") fetchMatches(false);
  }
  function renderLevelChips() {
    if (!levelData) return;
    const present = presentLevels();
    levelChips.textContent = "";
    if (present.length <= 1) return; // nothing meaningful to filter by
    const active = activeLevels();
    const shown = active.length ? new Set(active) : new Set(present);
    present.forEach((c) => {
      const chip = document.createElement("button");
      chip.className = "chip" + (shown.has(c) ? "" : " off");
      const dot = document.createElement("span");
      dot.className = "dot";
      dot.style.background = LEVEL_COLORS[c];
      const name = document.createElement("span");
      name.textContent = c === 0 ? "no level" : levelData.names[c];
      const cnt = document.createElement("span");
      cnt.className = "c";
      cnt.textContent = (levelData.counts[c] || 0).toLocaleString();
      chip.appendChild(dot);
      chip.appendChild(name);
      chip.appendChild(cnt);
      chip.addEventListener("click", () => {
        const s = new Set(shown);
        if (s.has(c)) s.delete(c);
        else s.add(c);
        if (s.size === 0) return; // keep at least one level on
        setLevels([...s]);
      });
      levelChips.appendChild(chip);
    });
  }

  // ---- time histogram ----------------------------------------------------
  const histogram = $("histogram");
  const thBars = $("th-bars");
  let histData = null;
  let brush = null;
  let brushEl = null;

  const fmtTime = (ms) => {
    try {
      return new Date(ms).toLocaleString();
    } catch (_e) {
      return String(ms);
    }
  };
  const timeRange = () => {
    const v = tokenParams().get("tr");
    if (!v) return null;
    const [a, b] = v.split(",").map(Number);
    return isNaN(a) || isNaN(b) ? null : [a, b];
  };
  function setTimeRange(from, to) {
    const p = tokenParams();
    if (from == null) p.delete("tr");
    else p.set("tr", from + "," + to);
    commitToken(p);
    painted = { from: -1, to: -1 };
    refresh(true);
    drawHistogram();
    resolveMarkers();
    if (tokenParams().get("search") || "") fetchMatches(false);
  }
  function fetchHistogram() {
    fetch("/histogram?buckets=160")
      .then((r) => r.json())
      .then((d) => {
        histData = d && d.counts ? d : null;
        drawHistogram();
      })
      .catch(() => {});
  }
  function drawHistogram() {
    if (histogram.hidden || !histData) return;
    const { counts, levels: lv, min, max } = histData;
    const peak = Math.max(1, ...counts);
    thBars.textContent = "";
    for (let i = 0; i < counts.length; i++) {
      const bar = document.createElement("div");
      bar.className = "th-bar";
      bar.style.height = Math.max(1, (counts[i] / peak) * 100) + "%";
      const code = lv[i] || 0;
      if (code > 0) bar.style.background = LEVEL_COLORS[code];
      bar.title = fmtTime(min + ((max - min) * i) / counts.length) + " — " + counts[i];
      thBars.appendChild(bar);
    }
    brushEl = null;
    $("th-min").textContent = fmtTime(min);
    $("th-max").textContent = fmtTime(max);
    const tr = timeRange();
    const center = $("th-center");
    center.textContent = "";
    if (tr) {
      const label = document.createElement("span");
      label.className = "th-range";
      label.textContent = fmtTime(tr[0]) + " – " + fmtTime(tr[1]) + " ";
      const clear = document.createElement("button");
      clear.className = "th-clear";
      clear.textContent = "×";
      clear.title = "Clear time range";
      clear.addEventListener("click", () => setTimeRange(null));
      center.appendChild(label);
      center.appendChild(clear);
    } else {
      const hint = document.createElement("span");
      hint.className = "th-hint";
      hint.textContent = "drag to filter by time";
      center.appendChild(hint);
    }
  }
  function drawBrushOverlay(x0, x1) {
    if (!brushEl) {
      brushEl = document.createElement("div");
      brushEl.className = "th-sel";
      thBars.appendChild(brushEl);
    }
    const lo = Math.min(x0, x1);
    brushEl.style.left = lo + "px";
    brushEl.style.width = Math.abs(x1 - x0) + "px";
  }
  thBars.addEventListener("mousedown", (e) => {
    const rect = thBars.getBoundingClientRect();
    brush = { x0: e.clientX - rect.left, rect };
    drawBrushOverlay(brush.x0, brush.x0);
    e.preventDefault();
  });
  window.addEventListener("mousemove", (e) => {
    if (!brush) return;
    drawBrushOverlay(brush.x0, e.clientX - brush.rect.left);
  });
  window.addEventListener("mouseup", (e) => {
    if (!brush) return;
    const x = e.clientX - brush.rect.left;
    const w = brush.rect.width;
    let a = Math.min(brush.x0, x) / w;
    let b = Math.max(brush.x0, x) / w;
    brush = null;
    if (brushEl) {
      brushEl.remove();
      brushEl = null;
    }
    if (!histData || b - a < 0.005) {
      if (timeRange()) setTimeRange(null); // a click clears any range
      return;
    }
    a = Math.max(0, a);
    b = Math.min(1, b);
    const span = histData.max - histData.min;
    setTimeRange(Math.round(histData.min + span * a), Math.round(histData.min + span * b));
  });
  function toggleHistogram(on) {
    histogram.hidden = !on;
    try {
      localStorage.setItem("zerch-histogram", on ? "1" : "0");
    } catch (_e) {}
    if (on) fetchHistogram();
    layout();
    refresh(true);
    drawMinimap();
  }

  // ---- menu, help overlay, display settings ------------------------------
  const menu = $("menu");
  const helpOverlay = $("help-overlay");
  function closeMenu() {
    menu.hidden = true;
  }
  $("menu-btn").addEventListener("click", function (e) {
    e.stopPropagation();
    menu.hidden = !menu.hidden;
  });
  document.addEventListener("click", closeMenu);
  $("menu-shell").addEventListener("click", function () {
    closeMenu();
    fetch("/shell?view=" + encodeURIComponent(state.view))
      .then((r) => r.json())
      .then((d) => {
        const done = () => flash("Copied!");
        if (navigator.clipboard && navigator.clipboard.writeText) {
          navigator.clipboard.writeText(d.cmd).then(done, () => flash(d.cmd));
        } else {
          flash(d.cmd);
        }
      })
      .catch(() => {});
  });
  $("menu-help").addEventListener("click", function () {
    closeMenu();
    helpOverlay.hidden = false;
  });
  $("help-close").addEventListener("click", () => (helpOverlay.hidden = true));
  helpOverlay.addEventListener("click", function (e) {
    if (e.target === helpOverlay) helpOverlay.hidden = true;
  });
  function flash(msg) {
    let el = $("flash");
    if (!el) {
      el = document.createElement("div");
      el.id = "flash";
      el.className = "flash";
      document.body.appendChild(el);
    }
    el.textContent = msg;
    el.hidden = false;
    clearTimeout(flash._t);
    flash._t = setTimeout(() => (el.hidden = true), 1500);
  }

  $("opt-zebra").addEventListener("change", function () {
    document.getElementById("app").classList.toggle("no-zebra", !this.checked);
    try {
      localStorage.setItem("zerch-zebra", this.checked ? "1" : "0");
    } catch (_e) {}
  });
  $("opt-minimap").addEventListener("change", function () {
    state.showMinimap = this.checked;
    minimap.hidden = !this.checked;
    try {
      localStorage.setItem("zerch-minimap", this.checked ? "1" : "0");
    } catch (_e) {}
    drawMinimap();
  });
  $("opt-histogram").addEventListener("change", function () {
    toggleHistogram(this.checked);
  });

  // ---- events ------------------------------------------------------------
  $("range-add-btn").addEventListener("click", addRangeFromInput);
  rangeInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      addRangeFromInput();
    }
  });
  $("filter-btn").addEventListener("click", addFilter);
  $("highlight-btn").addEventListener("click", addHighlight);
  modeToggle.addEventListener("click", () =>
    editLists((_f, _h, p) => p.set("mode", p.get("mode") === "OR" ? "AND" : "OR"), true)
  );
  $("panel-toggle").addEventListener("click", function () {
    const c = rightPanel.classList.toggle("collapsed");
    this.textContent = c ? "◀" : "▶";
    try {
      localStorage.setItem("zerch-panel-collapsed", c ? "1" : "0");
    } catch (_e) {}
  });
  searchBtn.addEventListener("click", runSearch);
  searchClear.addEventListener("click", clearSearch);
  $("next-match").addEventListener("click", () => goMatch(1));
  $("prev-match").addEventListener("click", () => goMatch(-1));
  regexToggle.addEventListener("click", function () {
    state.regexOn = !state.regexOn;
    regexToggle.classList.toggle("on", state.regexOn);
    if (searchInput.value) runSearch();
  });
  searchInput.addEventListener("input", function () {
    searchClear.hidden = !searchInput.value;
    showHistory();
  });
  searchInput.addEventListener("focus", showHistory);
  searchInput.addEventListener("blur", () => setTimeout(hideHistory, 120));
  searchInput.addEventListener("keydown", function (e) {
    if (e.key === "Enter") {
      e.preventDefault();
      runSearch();
    } else if (e.key === "Escape") {
      if (!searchHistoryEl.hidden) {
        hideHistory();
      } else {
        clearSearch();
        searchInput.blur();
      }
    }
  });

  // Selecting text in the log mirrors it into the search box and teal-highlights it
  // across rows (literal, not executed) — matching zercher's selectedText behaviour.
  logViewer.addEventListener("mouseup", function () {
    const sel = window.getSelection();
    const text = sel ? sel.toString() : "";
    if (text) {
      state.selectedText = text;
      searchInput.value = text;
      searchClear.hidden = false;
      refresh(true);
    } else if (state.selectedText) {
      state.selectedText = "";
      refresh(true);
    }
  });

  document.addEventListener("keydown", function (e) {
    if (e.key === "Escape" && !helpOverlay.hidden) {
      helpOverlay.hidden = true;
      return;
    }
    const tag = (e.target && e.target.tagName) || "";
    if (tag === "INPUT" || tag === "TEXTAREA") return;
    if (e.key === "?") {
      helpOverlay.hidden = !helpOverlay.hidden;
    } else if (e.key === "/") {
      e.preventDefault();
      searchInput.focus();
      searchInput.select();
    } else if (e.key === "n") {
      goMatch(1);
    } else if (e.key === "N") {
      goMatch(-1);
    } else if (e.key === "]") {
      goBookmark(1);
    } else if (e.key === "[") {
      goBookmark(-1);
    } else if (e.key === "g") {
      scroller.scrollTop = 0;
      refresh(true);
    } else if (e.key === "G") {
      scroller.scrollTop = spacer.offsetHeight;
      refresh(true);
    }
  });

  let raf = 0;
  scroller.addEventListener("scroll", function () {
    if (raf) return;
    raf = requestAnimationFrame(function () {
      raf = 0;
      refresh(false);
      drawMinimap();
    });
  });
  window.addEventListener("resize", function () {
    layout();
    refresh(true);
    drawMinimap();
  });

  // ---- init from the seed token -----------------------------------------
  (function init() {
    const p = tokenParams();
    const term = p.get("search") || "";
    searchInput.value = term;
    searchClear.hidden = !term;
    state.regexOn = p.get("srx") === "1";
    regexToggle.classList.toggle("on", state.regexOn);

    try {
      if (localStorage.getItem("zerch-panel-collapsed") === "1") {
        rightPanel.classList.add("collapsed");
        $("panel-toggle").textContent = "◀";
      }
      if (localStorage.getItem("zerch-zebra") === "0") {
        document.getElementById("app").classList.add("no-zebra");
        $("opt-zebra").checked = false;
      }
      if (localStorage.getItem("zerch-minimap") === "0") {
        state.showMinimap = false;
        minimap.hidden = true;
        $("opt-minimap").checked = false;
      }
    } catch (_e) {}

    // Time histogram: only offered when the file has timestamps. Shown if the pref
    // is on, or if the seed view already carries a time range (e.g. a shared link).
    if (boot.hastime) {
      $("opt-histogram-wrap").hidden = false;
      let showHist = false;
      try {
        showHist = localStorage.getItem("zerch-histogram") === "1";
      } catch (_e) {}
      if (timeRange()) showHist = true;
      if (showHist) {
        $("opt-histogram").checked = true;
        histogram.hidden = false;
        fetchHistogram();
      }
    }

    renderPanel();
    fetchCounts();
    fetchLevels();
    layout();
    refresh(true);
    resolveMarkers();
    if (term) fetchMatches(true);
  })();
})();
