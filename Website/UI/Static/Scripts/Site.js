// RuneAgent site: the menu toggle and the copy buttons. Nothing else runs,
// nothing is sent anywhere, and no HTML is ever built from strings.
"use strict";

document.addEventListener("DOMContentLoaded", () => {
  const toggle = document.querySelector(".menu-toggle");
  const nav = document.getElementById("site-nav");
  if (toggle && nav) {
    toggle.addEventListener("click", () => {
      const open = nav.classList.toggle("open");
      toggle.setAttribute("aria-expanded", String(open));
    });
  }

  for (const button of document.querySelectorAll("button.copy[data-copy]")) {
    button.addEventListener("click", async () => {
      const source = document.getElementById(button.dataset.copy);
      if (!source) return;
      const label = button.textContent;
      try {
        await navigator.clipboard.writeText(source.textContent.trim());
        button.textContent = "Copied";
        button.dataset.state = "done";
      } catch {
        button.textContent = "Select and copy";
      }
      setTimeout(() => {
        button.textContent = label;
        delete button.dataset.state;
      }, 1800);
    });
  }
});
