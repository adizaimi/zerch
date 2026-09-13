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
  const modeToggle = $("mode-toggle");
  const rightPanel = $("right-panel");

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
    matches: [],
    matchIdx: -1,
    selectedText: "",
    regexOn: false,
  };
  $("total-lines").textContent = state.fileLines.toLocaleString() + " lines";

  (function setGutter() {
    const digits = Math.max(3, String(state.fileLines).length);
    document.documentElement.style.setProperty("--gutter-width", digits * 8 + 28 + "px");
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

  function makeRow(item, index, topPx, isCurrent) {
    if (item.t === "g") {
      const g = document.createElement("div");
      g.className = "gap-row";
      g.style.top = topPx + "px";
      g.textContent =
        "··· " + item.count.toLocaleString() + " hidden (" + item.start + "–" + item.end + ") ···";
      return g;
    }
    const row = document.createElement("div");
    row.className = "line-row" + (index % 2 === 1 ? " zebra" : "");
    row.style.top = topPx + "px";
    row.dataset.n = item.n;

    const gutter = document.createElement("div");
    gutter.className = "line-number";
    gutter.style.boxShadow = "inset 3px 0 0 " + item.lvl;
    gutter.textContent = item.n;

    const content = document.createElement("div");
    content.className = "line-content";
    content.innerHTML = item.html; // server-rendered + server-escaped
    if (isCurrent) {
      content.querySelectorAll(".s").forEach((s) => s.classList.add("s-cur"));
    }

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
    const frag = document.createDocumentFragment();
    for (let k = 0; k < rows.length; k++) {
      const index = from + k;
      const topPx = domTop + (index * ROW_H - virtualTop);
      frag.appendChild(makeRow(rows[k], index, topPx, index === curPos));
    }
    rowsEl.textContent = "";
    rowsEl.appendChild(frag);
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

  function runSearch() {
    const term = searchInput.value;
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
    if (displayedChanged && (tokenParams().get("search") || "")) fetchMatches(false);
  }

  function tglBtn(label, on, cls, onClick) {
    const b = document.createElement("button");
    b.className = "tgl" + (cls ? " " + cls : "") + (on ? " on" : "");
    b.textContent = label;
    b.addEventListener("click", onClick);
    return b;
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
      const rm = document.createElement("button");
      rm.className = "rm";
      rm.textContent = "×";
      rm.addEventListener("click", () => editLists((_f, hs) => hs.splice(i, 1), false));
      row.appendChild(rm);
      highlightList.appendChild(row);
    });
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

  // ---- events ------------------------------------------------------------
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
  });
  searchInput.addEventListener("keydown", function (e) {
    if (e.key === "Enter") {
      e.preventDefault();
      runSearch();
    } else if (e.key === "Escape") {
      clearSearch();
      searchInput.blur();
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
    const tag = (e.target && e.target.tagName) || "";
    if (tag === "INPUT" || tag === "TEXTAREA") return;
    if (e.key === "/") {
      e.preventDefault();
      searchInput.focus();
      searchInput.select();
    } else if (e.key === "n") {
      goMatch(1);
    } else if (e.key === "N") {
      goMatch(-1);
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
    });
  });
  window.addEventListener("resize", function () {
    layout();
    refresh(true);
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
    } catch (_e) {}

    renderPanel();
    layout();
    refresh(true);
    if (term) fetchMatches(true);
  })();
})();
