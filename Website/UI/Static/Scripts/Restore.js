/* Before the first paint: a reader who followed a link sees the new page start
 * where they were, never the top flashing past first (owner, 2026-11-03).
 * The manual's chapters are opened here too: a new article must never show
 * them all closed and then pop them open (owner, 2026-11-13).
 * Reading works without this file; it hides the page only for the moment
 * before Site.js restores the position, and never longer than 2.5s.
 */
(() => {
  "use strict";
  try {
    const from = Number(sessionStorage.getItem("rune.from")) || Number(sessionStorage.getItem("rune.lang.from")) || 0;
    if (from > 0 && !location.hash) {
      document.documentElement.style.visibility = "hidden";
      setTimeout(() => { document.documentElement.style.visibility = ""; }, 2500);
    }
  } catch { /* storage refused */ }
  // The same list Site.js keeps (rune.docs.opened), applied while the parser
  // is still building the sidebar; the observer goes away once the page is
  // parsed, so nothing later touches the reader's own choices.
  try {
    const opened = JSON.parse(sessionStorage.getItem("rune.docs.opened")) || [];
    if (opened.length) {
      const kind = /\/changelog(?:\/|$)/.test(location.pathname) ? "changelog:" : "docs:";
      const done = new WeakSet();
      const apply = () => {
        const groups = [...document.querySelectorAll("details.docs-group")];
        groups.forEach((group, index) => {
          if (done.has(group)) return;
          done.add(group);
          if (opened.includes(kind + index)) group.open = true;
        });
      };
      const observer = new MutationObserver(apply);
      observer.observe(document, { childList: true, subtree: true });
      apply();
      document.addEventListener("DOMContentLoaded", () => observer.disconnect());
    }
  } catch { /* storage refused */ }
})();
