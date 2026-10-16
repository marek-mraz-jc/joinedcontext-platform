// The one-line launcher a city site includes (T-4138, Architecture/23 §3):
//   <script src="https://assistant.{domain}/d/{publicId}/embed.js" defer></script>
// One round button in the corner opens the chat page of this host in an iframe; the button and
// the panel live in a shadow root, so neither the site's styles nor ours leak. It carries no
// secret: the chat answers only on the deployment's allowed origins, and the iframe's own page
// may be framed by them alone. The accent colour comes from `data-color` on the script tag or
// the CSS variable `--jc-assistant-color`; the language from `data-lang` alone, never the page's
// own `lang` (T-4280): the chat opens in the deployment's language, Slovak, until the visitor
// switches it.
"use strict";
(function () {
  var me = document.currentScript;
  if (!me || !me.src) return;
  var src = new URL(me.src);
  var match = /^\/d\/([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)\/embed\.js$/.exec(src.pathname);
  if (!match) return;
  var publicId = match[1];
  var pageLang = String(me.getAttribute("data-lang") || "").slice(0, 2).toLowerCase();
  var colour = me.getAttribute("data-color") ||
    getComputedStyle(document.documentElement).getPropertyValue("--jc-assistant-color").trim();
  if (!/^#[0-9a-fA-F]{6}$/.test(colour)) colour = "#1f5fae";
  var LABELS = {
    sk: { open: "Otvoriť asistenta", close: "Zavrieť asistenta", title: "Asistent" },
    cs: { open: "Otevřít asistenta", close: "Zavřít asistenta", title: "Asistent" },
    en: { open: "Open the assistant", close: "Close the assistant", title: "Assistant" }
  };
  var words = LABELS[pageLang] || LABELS.sk;

  var CSS =
    ":host{all:initial}" +
    ".jc-launch{position:fixed;right:20px;bottom:20px;z-index:2147483000;width:56px;height:56px;border:0;" +
    "border-radius:50%;background:var(--jc-accent);color:#fff;cursor:pointer;box-shadow:0 4px 14px rgba(0,0,0,.25);" +
    "display:flex;align-items:center;justify-content:center;padding:0}" +
    ".jc-launch:focus-visible{outline:3px solid var(--jc-accent);outline-offset:3px}" +
    ".jc-launch svg{width:28px;height:28px;fill:currentColor}" +
    ".jc-panel{position:fixed;right:20px;bottom:88px;z-index:2147483000;width:400px;height:min(640px,calc(100vh - 108px));" +
    "border-radius:12px;overflow:hidden;box-shadow:0 8px 30px rgba(0,0,0,.3);background:#fff}" +
    ".jc-panel[hidden]{display:none}" +
    ".jc-panel iframe{width:100%;height:100%;border:0;display:block}" +
    "@media (max-width:640px){.jc-panel{inset:0;width:100%;height:100%;border-radius:0}" +
    ".jc-open .jc-launch{display:none}}" +
    "@media (prefers-reduced-motion:no-preference){.jc-launch{transition:transform .15s}.jc-launch:hover{transform:scale(1.05)}}";

  var host = document.createElement("div");
  host.setAttribute("data-jc-assistant", publicId);
  var shadow = host.attachShadow({ mode: "open" });
  // A constructed sheet is not inline style, so a site whose policy forbids inline styles still
  // shows the launcher; an older browser gets a <style> element.
  try {
    var sheet = new CSSStyleSheet();
    sheet.replaceSync(CSS);
    shadow.adoptedStyleSheets = [sheet];
  } catch (e) {
    var style = document.createElement("style");
    style.textContent = CSS;
    shadow.appendChild(style);
  }
  var wrap = document.createElement("div");
  wrap.style.setProperty("--jc-accent", colour);
  shadow.appendChild(wrap);

  var launch = document.createElement("button");
  launch.type = "button";
  launch.className = "jc-launch";
  launch.setAttribute("aria-expanded", "false");
  launch.setAttribute("aria-label", words.open);
  var svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("aria-hidden", "true");
  var path = document.createElementNS("http://www.w3.org/2000/svg", "path");
  path.setAttribute("d", "M4 4h16a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H9l-5 4v-4a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2zm3 6a1.5 1.5 0 1 0 0 3 1.5 1.5 0 0 0 0-3zm5 0a1.5 1.5 0 1 0 0 3 1.5 1.5 0 0 0 0-3zm5 0a1.5 1.5 0 1 0 0 3 1.5 1.5 0 0 0 0-3z");
  svg.appendChild(path);
  launch.appendChild(svg);

  var panel = document.createElement("div");
  panel.className = "jc-panel";
  panel.hidden = true;
  panel.id = "jc-assistant-panel";
  launch.setAttribute("aria-controls", panel.id);
  var frame = null;
  // Below 640 px the open panel takes the whole screen; the page behind is made inert, so focus
  // and a screen reader stay inside the chat until it closes.
  var phone = window.matchMedia("(max-width: 640px)");
  var silenced = [];
  function silence(on) {
    silenced.forEach(function (el) { el.inert = false; });
    silenced = [];
    if (!on || !phone.matches) return;
    Array.prototype.forEach.call(document.body.children, function (el) {
      if (el !== host && el !== spacer && !el.inert) { el.inert = true; silenced.push(el); }
    });
  }

  function open() {
    if (!frame) {
      // Loaded on the first open, so a page that never opens the chat never asks the service.
      frame = document.createElement("iframe");
      frame.title = words.title;
      frame.allow = "clipboard-write";
      frame.referrerPolicy = "no-referrer";
      frame.src = src.origin + "/d/" + publicId + "/widget?embed=1" + (pageLang ? "&lang=" + encodeURIComponent(pageLang) : "");
      panel.appendChild(frame);
    }
    panel.hidden = false;
    wrap.classList.add("jc-open");
    launch.setAttribute("aria-expanded", "true");
    launch.setAttribute("aria-label", words.close);
    silence(true);
    frame.focus();
  }

  function close() {
    if (panel.hidden) return;
    panel.hidden = true;
    wrap.classList.remove("jc-open");
    silence(false);
    launch.setAttribute("aria-expanded", "false");
    launch.setAttribute("aria-label", words.open);
    launch.focus();
  }

  launch.addEventListener("click", function () {
    if (panel.hidden) open();
    else close();
  });
  document.addEventListener("keydown", function (event) {
    if (event.key === "Escape") close();
  });
  // The chat page asks to close (its own close button or Escape inside it); only our frame may.
  window.addEventListener("message", function (event) {
    if (frame && event.source === frame.contentWindow && event.origin === src.origin &&
        event.data && event.data.type === "jc-assistant-close") {
      close();
    }
  });

  wrap.appendChild(panel);
  wrap.appendChild(launch);
  // The launcher sits over the page's last 96 px; this spacer at the end of the body lets the
  // host page scroll its last content clear of it. A site that lays out its own room sets
  // `--jc-assistant-inset: 0px`.
  var spacer = document.createElement("div");
  spacer.setAttribute("data-jc-assistant-inset", "");
  spacer.setAttribute("aria-hidden", "true");
  spacer.style.height = "var(--jc-assistant-inset, 96px)";
  function mount() {
    document.body.appendChild(spacer);
    document.body.appendChild(host);
  }
  if (document.body) mount();
  else document.addEventListener("DOMContentLoaded", mount);
})();
