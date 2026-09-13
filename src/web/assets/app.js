// zerch web UI — vanilla JS, no build step. Grows commit-by-commit into the full
// viewer (virtual list, filters, highlights, minimap). Commit 2 is the scaffold:
// read the server-injected bootstrap and show the file name + line count.
"use strict";

(function () {
  function bootstrap() {
    const el = document.getElementById("zerch-bootstrap");
    try {
      return JSON.parse(el.textContent);
    } catch (_e) {
      return { file: "", total: 0, view: "" };
    }
  }

  const boot = bootstrap();
  document.getElementById("file-name").textContent = boot.file || "(stdin)";
  const total = boot.total || 0;
  document.getElementById("total-lines").textContent =
    total.toLocaleString() + " lines";
})();
