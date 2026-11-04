/* Before the first paint: a reader who followed a link sees the new page start
 * where they were, never the top flashing past first (owner, 2026-11-03).
 * Reading works without this file; it hides the page only for the moment
 * before Site.js restores the position, and never longer than 2.5s.
 */
(() => {
  "use strict";
  try {
    const from = Number(sessionStorage.getItem("rune.from")) || 0;
    if (from > 0 && !location.hash) {
      document.documentElement.style.visibility = "hidden";
      setTimeout(() => { document.documentElement.style.visibility = ""; }, 2500);
    }
  } catch { /* storage refused */ }
})();
