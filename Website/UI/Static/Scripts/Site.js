/* Reading and navigation work without scripts; these are enhancements only. */
(() => {
  "use strict";
  // The product name arrives from the left once the page has moved 48px: the
  // same moment, curve and clip as atoryn.com.
  const header = document.querySelector(".top");
  if (header) {
    let queued = false;
    const readHeader = () => {
      queued = false;
      header.classList.toggle("is-scrolled", (window.scrollY || 0) > 48);
    };
    addEventListener("scroll", () => {
      if (!queued) { queued = true; requestAnimationFrame(readHeader); }
    }, { passive: true });
    readHeader();
  }
  // The manual keeps the chapters the reader opened, so choosing another
  // article never closes them all (owner, 2026-11-03).
  const groups = [...document.querySelectorAll(".docs-group")];
  const OPENED = "rune.docs.opened";
  const stored = (key, fallback) => {
    try { return JSON.parse(sessionStorage.getItem(key)) ?? fallback; } catch { return fallback; }
  };
  const remember = (key, value) => {
    try { sessionStorage.setItem(key, JSON.stringify(value)); } catch { /* storage refused */ }
  };
  if (groups.length) {
    const opened = stored(OPENED, []);
    for (const group of groups) {
      const title = group.querySelector("summary").textContent.trim();
      if (opened.includes(title)) group.open = true;
      group.addEventListener("toggle", () => {
        remember(OPENED, groups.filter((one) => one.open).map((one) => one.querySelector("summary").textContent.trim()));
      });
    }
  }
  // Any link that leads to another page of this site remembers where the
  // reader was; the next page starts there and glides to its top, so no article
  // ever jumps to the top at once (owner, 2026-11-03).
  const FROM = "rune.from";
  const LANG = "rune.lang.from";
  const SIDEBAR_SCROLL = "rune.sidebar.scroll";
  const sidebar = document.querySelector(".docs-sidebar");
  const reduce = matchMedia("(prefers-reduced-motion: reduce)");
  // Choosing a language is not leaving the article: the reader stays exactly
  // where they were, as on atoryn.com (owner, 2026-11-05). Every other link
  // starts a new page where the reader was and glides to its top.
  for (const link of document.querySelectorAll("a[hreflang]")) {
    if (!(link instanceof HTMLAnchorElement)) continue;
    link.addEventListener("click", (event) => {
      if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
      remember(LANG, Math.round(window.scrollY));
    });
  }
  for (const link of document.querySelectorAll("a[href]")) {
    const href = link.getAttribute("href") || "";
    if (!href || href.startsWith("#") || link.target === "_blank" || link.hasAttribute("download")) continue;
    if (link.hasAttribute("hreflang")) continue;
    let target;
    try { target = new URL(link.href, location.href); } catch { continue; }
    if (target.origin !== location.origin || target.hash || target.pathname === location.pathname) continue;
    link.addEventListener("click", (event) => {
      if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
      remember(FROM, Math.round(window.scrollY));
      if (sidebar) remember(SIDEBAR_SCROLL, Math.round(sidebar.scrollTop));
    });
  }
  if (sidebar) {
    try {
      const sbScroll = Number(sessionStorage.getItem(SIDEBAR_SCROLL)) || 0;
      if (sbScroll > 0) sidebar.scrollTop = sbScroll;
    } catch { /* storage refused */ }
  }
  // A reader who chose another language returns to the same spot, at once, and
  // the page never glides: the words changed, the place did not.
  let langFrom = 0;
  try { langFrom = Number(sessionStorage.getItem(LANG)) || 0; sessionStorage.removeItem(LANG); } catch { /* storage refused */ }
  if (langFrom > 0 && !location.hash) {
    const start = Math.min(langFrom, document.documentElement.scrollHeight - innerHeight);
    if (start > 0) window.scrollTo({ top: start, behavior: "instant" });
  }
  let from = 0;
  try { from = Number(sessionStorage.getItem(FROM)) || 0; sessionStorage.removeItem(FROM); } catch { /* storage refused */ }
  if (from > 0 && !location.hash) {
    const start = Math.min(from, document.documentElement.scrollHeight - innerHeight);
    if (start > 0) {
      window.scrollTo({ top: start, behavior: "instant" });
      requestAnimationFrame(() => window.scrollTo({ top: 0, behavior: reduce.matches ? "instant" : "smooth" }));
    }
  }
  // Restore.js held the page back before its first paint; it is ready now.
  document.documentElement.style.visibility = "";
  // One click copies. The asynchronous clipboard exists only in secure
  // contexts; a plain-http preview still copies through the selection
  // command. Only if both fail is the text selected for Ctrl+C.
  const copyText = async (text) => {
    if (navigator.clipboard && window.isSecureContext) {
      try { await navigator.clipboard.writeText(text); return true; } catch { /* fall through */ }
    }
    const area = document.createElement("textarea");
    area.value = text;
    area.readOnly = true;
    area.className = "sr";
    document.body.append(area);
    area.focus({ preventScroll: true });
    area.select();
    let copied = false;
    try { copied = document.execCommand("copy"); } catch { copied = false; }
    area.remove();
    return copied;
  };
  // Manual code blocks get the same corner control; without scripts the text
  // simply stays selectable. Built from nodes, never from markup strings.
  const NS = "http://www.w3.org/2000/svg";
  const icon = (name, shapes) => {
    const svg = document.createElementNS(NS, "svg");
    svg.setAttribute("class", name);
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("aria-hidden", "true");
    for (const [tag, attrs] of shapes) {
      const shape = document.createElementNS(NS, tag);
      for (const [key, value] of Object.entries(attrs)) shape.setAttribute(key, value);
      svg.append(shape);
    }
    return svg;
  };
  document.querySelectorAll(".prose pre.block").forEach((pre, index) => {
    const code = pre.querySelector("code") || pre;
    if (!code.id) code.id = "code-" + (index + 1);
    const button = document.createElement("button");
    button.type = "button";
    button.className = "code-copy";
    button.dataset.copy = code.id;
    button.title = "Copy";
    button.setAttribute("aria-label", "Copy this code");
    button.append(
      icon("i-copy", [["rect", { x: "8.5", y: "8.5", width: "11", height: "11", rx: "2.5" }], ["path", { d: "M15.5 8.5V7a2.5 2.5 0 0 0-2.5-2.5H7A2.5 2.5 0 0 0 4.5 7v6A2.5 2.5 0 0 0 7 15.5h1.5" }]]),
      icon("i-done", [["path", { d: "m5 12.5 4.5 4.5L19 7.5" }]]),
    );
    pre.append(button);
  });
  document.querySelectorAll("[data-copy]").forEach((button) => {
    const code = document.getElementById(button.dataset.copy);
    if (!code) return;
    const label = button.getAttribute("aria-label");
    let timer = 0;
    button.addEventListener("click", async () => {
      clearTimeout(timer);
      if (await copyText(code.textContent)) {
        button.dataset.state = "done";
        button.setAttribute("aria-label", "Copied");
      } else {
        const range = document.createRange();
        range.selectNodeContents(code);
        const selection = getSelection();
        selection.removeAllRanges();
        selection.addRange(range);
        button.dataset.state = "select";
        button.setAttribute("aria-label", "Selected; press Ctrl+C to copy");
      }
      button.focus({ preventScroll: true });
      timer = setTimeout(() => { delete button.dataset.state; button.setAttribute("aria-label", label); }, 2000);
    });
  });
  // The contents mark the section being read: the last heading above the reading line, and the last one
  // once the page can scroll no further, so a short closing section is still reached. A heading chosen in
  // the contents stays marked while the page travels to it, until the reader scrolls on their own.
  const links = [...document.querySelectorAll('.index a[href^="#"]')];
  const headings = links.map((link) => document.getElementById(link.hash.slice(1))).filter(Boolean);
  if (headings.length) {
    let queued = false;
    let chosen = headings.find((heading) => "#" + heading.id === location.hash) || null;
    const mark = (current) => {
      for (const link of links) {
        if (link.hash === "#" + current.id) link.setAttribute("aria-current", "true");
        else link.removeAttribute("aria-current");
      }
    };
    const update = () => {
      queued = false;
      if (chosen) { mark(chosen); return; }
      const root = document.documentElement;
      let current = headings[0];
      if (Math.ceil(scrollY + innerHeight) >= root.scrollHeight - 2) current = headings[headings.length - 1];
      else for (const heading of headings) if (heading.getBoundingClientRect().top <= 150) current = heading;
      mark(current);
    };
    for (const link of links) {
      link.addEventListener("click", () => {
        chosen = document.getElementById(link.hash.slice(1));
        if (chosen) mark(chosen);
      });
    }
    const release = () => { chosen = null; };
    addEventListener("wheel", release, { passive: true });
    addEventListener("touchmove", release, { passive: true });
    addEventListener("keydown", (event) => { if (["ArrowDown", "ArrowUp", "PageDown", "PageUp", "Home", "End", " "].includes(event.key)) release(); });
    addEventListener("scroll", () => { if (!queued) { queued = true; requestAnimationFrame(update); } }, { passive: true });
    update();
  }
  // Light rests while it is off screen.
  if ("IntersectionObserver" in window) {
    const observer = new IntersectionObserver((entries) => {
      for (const entry of entries) entry.target.classList.toggle("is-offscreen", !entry.isIntersecting);
    });
    document.querySelectorAll(".alive").forEach((node) => observer.observe(node));
  }
  // The install choice starts on the visitor's own system; it stays theirs to change.
  const platform = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || "";
  const system = /^mac/i.test(platform) ? "os-macos" : /^win/i.test(platform) ? "os-windows" : "";
  const choice = system && document.getElementById(system);
  if (choice) choice.checked = true;
  // Phones keep the manual's index in a drawer: the three lines — or the mark —
  // open it from the left, the way the rest of the web does, and the chapters
  // are already open inside it (owner, 2026-11-03).
  const menu = document.querySelector(".docs-menu");
  if (menu) menu.open = true;
  const drawer = document.getElementById("docs-nav");
  const toggle = document.querySelector(".menu-toggle");
  const scrim = document.querySelector(".scrim");
  const narrow = matchMedia("(max-width: 820px)");
  if (drawer && toggle) {
    const setDrawer = (open) => {
      drawer.classList.toggle("is-open", open);
      if (scrim) scrim.classList.toggle("is-open", open);
      toggle.setAttribute("aria-expanded", String(open));
      document.body.style.overflow = open ? "hidden" : "";
    };
    const closeDrawer = () => { setDrawer(false); toggle.focus({ preventScroll: true }); };
    toggle.addEventListener("click", () => setDrawer(!drawer.classList.contains("is-open")));
    const close = drawer.querySelector(".drawer-close");
    if (close) close.addEventListener("click", closeDrawer);
    if (scrim) scrim.addEventListener("click", closeDrawer);
    addEventListener("keydown", (event) => {
      if (event.key === "Escape" && drawer.classList.contains("is-open")) closeDrawer();
    });
    // On a phone the mark opens the manual's menu instead of leaving the article.
    const brand = document.querySelector(".brand");
    if (brand) brand.addEventListener("click", (event) => {
      if (!narrow.matches) return;
      event.preventDefault();
      setDrawer(true);
    });
    narrow.addEventListener("change", () => { if (!narrow.matches) setDrawer(false); });
  }

  // Language switcher menu: close on outside click or Escape, scroll active item into view.
  const langSwitch = document.querySelector(".lang-switch");
  if (langSwitch) {
    const langTrigger = langSwitch.querySelector(".lang-trigger");
    const activeItem = langSwitch.querySelector('.lang-item[aria-current="true"]');
    // Scrolling the active language into view must move the menu alone. Asking
    // the browser for it moves the page too, by a few pixels, the moment the
    // menu opens (measured: 800px to 781px), so the menu is scrolled itself.
    const langMenu = langSwitch.querySelector(".lang-menu");
    langSwitch.addEventListener("toggle", () => {
      if (!langSwitch.open || !langMenu || !activeItem) return;
      const top = activeItem.offsetTop;
      const bottom = top + activeItem.offsetHeight;
      if (bottom > langMenu.scrollTop + langMenu.clientHeight) langMenu.scrollTop = bottom - langMenu.clientHeight;
      else if (top < langMenu.scrollTop) langMenu.scrollTop = top;
    });
    document.addEventListener("click", (event) => {
      if (langSwitch.open && !langSwitch.contains(event.target)) {
        langSwitch.open = false;
      }
    });
    addEventListener("keydown", (event) => {
      if (event.key === "Escape" && langSwitch.open) {
        langSwitch.open = false;
        if (langTrigger) langTrigger.focus({ preventScroll: true });
      }
    });
  }
})();
