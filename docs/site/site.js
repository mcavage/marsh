// runmar.sh: theme toggle, copy buttons, shell-session replay, and the docs
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

  // Replay highlights the shell commands and their filesystem effects together.
  // The complete session remains readable, including during playback.
  var session = document.querySelector(".shell-session");
  if (session) {
    var replays = session.querySelectorAll(".session-replay");
    var stage = session.querySelector(".session-stage");
    var command = session.querySelector(".session-command");
    var motion = matchMedia("(prefers-reduced-motion: reduce)");
    var phases = [
      { label: "A Linux shell at your project's path.", command: "marsh", delay: 1100 },
      { label: "split snapshots the project and creates private copies.", command: "split { … }", delay: 1300 },
      { label: "Both branches run, each on its own files.", command: "claude … && make test\ncodex … && make test", delay: 1500 },
      { label: "join returns each branch's output, status, and patch.", command: "} | join --keep", delay: 1100 },
      { label: "Back at the prompt: inspect, apply, and test.", command: "less …/api/diff.patch\ngit apply …/api/diff.patch\nmake test", delay: 1400 }
    ];
    var timer = null;
    var playing = false;

    function clearHighlights() {
      session.querySelectorAll(".is-active, .is-cursor").forEach(function (el) {
        el.classList.remove("is-active", "is-cursor");
      });
    }
    function finish() {
      clearTimeout(timer);
      timer = null;
      playing = false;
      clearHighlights();
      replays.forEach(function (button) {
        button.textContent = "Replay";
        button.setAttribute("aria-label", "Replay shell session animation");
      });
      stage.textContent = "A Linux shell. Private copies. Patches you choose.";
      if (command) command.textContent = "split { … } | join --keep";
    }
    function step(index) {
      if (!playing) return;
      if (index === phases.length) { finish(); return; }
      clearHighlights();
      var lines = session.querySelectorAll('[data-session-phase="' + index + '"]');
      lines.forEach(function (line) { line.classList.add("is-active"); });
      if (lines.length) lines[lines.length - 1].classList.add("is-cursor");
      session.querySelectorAll('[data-file-phase="' + index + '"]').forEach(function (el) {
        el.classList.add("is-active");
      });
      stage.textContent = phases[index].label;
      if (command) command.textContent = phases[index].command;
      timer = setTimeout(function () { step(index + 1); }, phases[index].delay);
    }
    function play() {
      if (motion.matches || document.hidden) return;
      finish();
      playing = true;
      replays.forEach(function (button) {
        button.textContent = "Stop";
        button.setAttribute("aria-label", "Stop shell session animation");
      });
      step(0);
    }
    replays.forEach(function (button) {
      button.addEventListener("click", function () { if (playing) finish(); else play(); });
      button.hidden = motion.matches;
    });
    if (motion.addEventListener) motion.addEventListener("change", function () {
      finish();
      replays.forEach(function (button) { button.hidden = motion.matches; });
    });
    document.addEventListener("visibilitychange", function () { if (document.hidden) finish(); });
    if ("IntersectionObserver" in window) {
      var start = new IntersectionObserver(function (entries) {
        if (entries[0].isIntersecting) { start.disconnect(); play(); }
      }, { threshold: 0.2 });
      start.observe(session.querySelector(".session-window"));
    }
  }

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
