/* Reading and navigation work without scripts; these are enhancements only. */
(() => {
  "use strict";
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
  const menu = document.querySelector(".docs-menu");
  if (menu && matchMedia("(max-width: 820px)").matches) menu.open = false;
})();
