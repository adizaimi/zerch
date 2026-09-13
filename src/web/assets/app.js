// zerch web UI — vanilla JS, no build step. Read-only virtual-list viewer;
// interactivity (search, filters, bookmarks, minimap) lands in later commits.
"use strict";

(function () {
  const boot = (function () {
    try {
      return JSON.parse(document.getElementById("zerch-bootstrap").textContent);
    } catch (_e) {
      return { file: "", total: 0, view: "" };
    }
  })();

  const ROW_H = 24; // fixed slot height (line rows and gaps share it in v1)
  const OVERSCAN = 8;
  const SAFE_MAX = 15000000; // browsers cap element height; scale past this

  const scroller = document.getElementById("scroller");
  const spacer = document.getElementById("spacer");
  const rowsEl = document.getElementById("rows");

  document.getElementById("file-name").textContent = boot.file || "(stdin)";

  const state = {
    view: boot.view || "", // the --view token; the URL *is* the state
    total: boot.total || 0, // displayed-item count (corrected by /rows)
    fileLines: boot.total || 0, // raw line count, for gutter width
  };
  document.getElementById("total-lines").textContent =
    state.fileLines.toLocaleString() + " lines";

  // Gutter width from the largest line number's digit count (min 3 digits).
  (function setGutter() {
    const digits = Math.max(3, String(state.fileLines).length);
    document.documentElement.style.setProperty(
      "--gutter-width",
      digits * 8 + 28 + "px"
    );
  })();

  let viewportH = scroller.clientHeight;
  let domH = 0;
  let scale = 1;

  // Size the scroll spacer. Above SAFE_MAX px the virtual space is mapped linearly
  // onto a capped DOM height, so multi-GB files still scroll (below the cap: 1:1).
  function layout() {
    viewportH = scroller.clientHeight;
    const virtualH = state.total * ROW_H;
    domH = Math.min(virtualH, SAFE_MAX);
    scale =
      virtualH > domH && domH > viewportH
        ? (virtualH - viewportH) / (domH - viewportH)
        : 1;
    spacer.style.height = Math.max(domH, 1) + "px";
  }

  function makeRow(item, index, topPx) {
    if (item.t === "g") {
      const g = document.createElement("div");
      g.className = "gap-row";
      g.style.top = topPx + "px";
      g.textContent =
        "··· " +
        item.count.toLocaleString() +
        " hidden (" +
        item.start +
        "–" +
        item.end +
        ") ···";
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
    // Recompute positioning against the live scroll so rows land correctly even if
    // the user scrolled while the fetch was in flight. At scale 1 this is absolute
    // (topPx === index * ROW_H); when scaled, rows sit relative to the viewport.
    const domTop = scroller.scrollTop;
    const virtualTop = domTop * scale;
    const frag = document.createDocumentFragment();
    for (let k = 0; k < rows.length; k++) {
      const index = from + k;
      const topPx = domTop + (index * ROW_H - virtualTop);
      frag.appendChild(makeRow(rows[k], index, topPx));
    }
    rowsEl.textContent = "";
    rowsEl.appendChild(frag);
  }

  function refresh(force) {
    const { first, last } = visibleRange();
    if (!force && first === painted.from && last === painted.to) return;
    painted = { from: first, to: last };
    const url =
      "/rows?view=" +
      encodeURIComponent(state.view) +
      "&from=" +
      first +
      "&to=" +
      last;
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

  layout();
  refresh(true);
})();
