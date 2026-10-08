// runmar.sh: theme toggle, copy buttons, the terminal replay, and the docs
// on-page TOC highlight. Everything works without it.
(function () {
  "use strict";
  var doc = document.documentElement;
  var reduce = window.matchMedia && matchMedia("(prefers-reduced-motion: reduce)").matches;

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
  document.querySelectorAll(".prose pre").forEach(function (pre) {
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

  // Terminal replay. The full transcript is in the HTML; lines are hidden
  // (visibility only, so nothing moves) and revealed in order. Command text
  // is "typed" by widening a 1ch-per-character clip.
  var term = document.querySelector(".term.anim");
  if (!term) return;
  var lines = Array.prototype.slice.call(term.querySelectorAll(".ln"));
  var replay = document.querySelector(".replay");
  if (reduce) {
    lines.forEach(function (l) { l.classList.add("on"); });
    if (replay) replay.hidden = true;
    return;
  }
  var timer = null;
  var run = 0;
  function sleep(ms, id) {
    return new Promise(function (resolve) {
      timer = setTimeout(function () { if (id === run) resolve(); }, ms);
    });
  }
  function typeLine(line, id) {
    var tx = line.querySelector(".tx");
    var n = tx ? tx.textContent.length : 0;
    line.classList.add("on", "typing");
    var i = 0;
    return new Promise(function (resolve) {
      (function step() {
        if (id !== run) return;
        line.style.setProperty("--n", i);
        if (i >= n) {
          line.classList.remove("typing");
          line.style.removeProperty("--n");
          resolve();
          return;
        }
        i += 1;
        timer = setTimeout(step, 14 + Math.random() * 30);
      })();
    });
  }
  async function play() {
    var id = ++run;
    clearTimeout(timer);
    lines.forEach(function (l) { l.classList.remove("on", "typing", "cursor"); l.style.removeProperty("--n"); });
    term.scrollTop = 0;
    await sleep(350, id);
    for (var k = 0; k < lines.length; k++) {
      if (id !== run) return;
      var line = lines[k];
      if (line.classList.contains("cmd")) {
        line.classList.add("on", "cursor");
        await sleep(line.classList.contains("cont") ? 120 : 420, id);
        line.classList.remove("cursor");
        await typeLine(line, id);
      } else {
        line.classList.add("on");
      }
      var below = line.offsetTop + line.offsetHeight - term.clientHeight;
      if (below > term.scrollTop) term.scrollTop = below;
      var wait = parseInt(line.getAttribute("data-wait") || "", 10);
      await sleep(isNaN(wait) ? (line.classList.contains("cmd") ? 160 : 45) : wait, id);
    }
    var last = lines[lines.length - 1];
    if (last && last.classList.contains("end")) last.classList.add("cursor");
  }
  if (replay) replay.addEventListener("click", play);
  if ("IntersectionObserver" in window) {
    var started = false;
    var obs = new IntersectionObserver(function (entries) {
      if (!started && entries[0].isIntersecting) { started = true; obs.disconnect(); play(); }
    }, { threshold: 0.25 });
    obs.observe(term);
  } else {
    play();
  }
})();
