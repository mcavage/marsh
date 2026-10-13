// runmar.sh: theme toggle, copy buttons, and the docs
// on-page TOC highlight. Everything works without it.
(function () {
  "use strict";
  var doc = document.documentElement;

  // Theme toggle: dark first; the choice is remembered on this device.
  var toggle = document.querySelector(".theme");
  if (toggle) {
    toggle.addEventListener("click", function () {
      var light = doc.dataset.theme
        ? doc.dataset.theme === "light"
        : matchMedia("(prefers-color-scheme: light)").matches;
      doc.dataset.theme = light ? "dark" : "light";
      try { localStorage.setItem("theme", doc.dataset.theme); } catch (e) {}
    });
  }

  // Copy buttons.
  function copyText(text, button) {
    var done = function () {
      button.textContent = "Copied";
      button.classList.add("done");
      setTimeout(function () { button.textContent = "Copy"; button.classList.remove("done"); }, 1600);
    };
    if (navigator.clipboard && window.isSecureContext) {
      navigator.clipboard.writeText(text).then(done, function () {});
    } else {
      var area = document.createElement("textarea");
      area.value = text;
      area.setAttribute("readonly", "");
      area.style.position = "absolute";
      area.style.left = "-9999px";
      document.body.appendChild(area);
      area.select();
      try { document.execCommand("copy"); done(); } catch (e) {}
      document.body.removeChild(area);
    }
  }
  function button(label) {
    var b = document.createElement("button");
    b.type = "button";
    b.className = "copy";
    b.textContent = "Copy";
    b.setAttribute("aria-label", label);
    return b;
  }
  document.querySelectorAll("[data-copy]").forEach(function (el) {
    var b = el.querySelector(".copy");
    if (b) b.addEventListener("click", function () { copyText(el.getAttribute("data-copy"), b); });
  });
  // Docs code blocks. In a transcript (```console) copy only the commands.
  document.querySelectorAll(".prose pre, .home pre.example").forEach(function (pre) {
    var code = pre.querySelector("code");
    if (!code) return;
    var b = button("Copy code");
    b.addEventListener("click", function () {
      var text = code.textContent;
      if (code.classList.contains("language-console")) {
        text = text.split("\n").filter(function (l) { return /^(\S*\$|>) /.test(l); })
          .map(function (l) { return l.replace(/^(\S*\$|>) /, ""); }).join("\n");
      }
      copyText(text.replace(/\n+$/, "") + "\n", b);
    });
    pre.appendChild(b);
  });

  // On-page TOC: mark the section being read.
  var toc = document.querySelectorAll(".toc a");
  if (toc.length && "IntersectionObserver" in window) {
    var links = {};
    toc.forEach(function (a) { links[decodeURIComponent(a.hash.slice(1))] = a; });
    var current = null;
    var seen = new IntersectionObserver(function (entries) {
      entries.forEach(function (e) {
        if (e.isIntersecting && links[e.target.id]) {
          if (current) current.classList.remove("on");
          current = links[e.target.id];
          current.classList.add("on");
        }
      });
    }, { rootMargin: "-10% 0px -70% 0px" });
    Object.keys(links).forEach(function (id) {
      var h = document.getElementById(id);
      if (h) seen.observe(h);
    });
  }

})();
