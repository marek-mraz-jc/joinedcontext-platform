// The standalone assistant (T-4694): a static copy of the widget any host may serve, at
// `/{publicId}` as a page of its own and at `/{publicId}?embed=1` inside the launcher's frame
// (embed.js with `data-deployment`). It asks the chat's host only, the address in
// <meta name="jc-api">, for the deployment's configuration, then starts widget.js on it. It
// holds no secret and keeps no state of its own: the widget keeps the conversation in this
// tab's sessionStorage, so a reload of the page resumes it (T-4685).
"use strict";
var JCApp = (function () {
  var ID = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;

  function origin(value) {
    var match = /^(https?:\/\/[a-z0-9.-]+(?::\d{1,5})?)(?:\/|$)/i.exec(String(value || ""));
    return match ? match[1].toLowerCase() : null;
  }

  /** The deployment a page address names: `/{publicId}`, or `?d=` on a host that rewrites nothing. */
  function deployment(pathname, search) {
    var asked = new URLSearchParams(search || "").get("d") || String(pathname || "").split("/")[1] || "";
    return ID.test(asked) ? asked : null;
  }

  /**
   * Whether a framed copy may show: the page framing it, as the browser records it
   * (`ancestorOrigins`) or else as the referrer names it, is one of the deployment's origins. A
   * static host cannot send a per-deployment `frame-ancestors`, so the page refuses itself; the
   * chat refuses every other origin anyway.
   */
  function framedBy(ancestors, referrer, frameOrigins) {
    var parent = origin(ancestors && ancestors.length ? ancestors[0] : referrer);
    return Boolean(parent) && (frameOrigins || []).some(function (o) { return origin(o) === parent; });
  }

  /**
   * Whether the page may show at all: on its own, or framed by one of the deployment's origins.
   * Any frame counts, `?embed=1` or not: the query only picks the layout, so a page framed without
   * it is checked the same (T-6218).
   */
  function mayShow(framed, ancestors, referrer, frameOrigins) {
    return !framed || framedBy(ancestors, referrer, frameOrigins);
  }

  function start() {
    var root = document.getElementById("jc-chat");
    var meta = document.querySelector('meta[name="jc-api"]');
    var api = origin(meta && meta.content);
    var id = deployment(location.pathname, location.search);
    var params = new URLSearchParams(location.search);
    var framed = window.parent !== window;
    var embedded = params.get("embed") === "1" && framed;
    function missing() {
      root.textContent = "";
      var text = document.createElement("p");
      text.className = "jc-missing";
      text.textContent = "Tento asistent tu neodpovedá. / This assistant does not answer here.";
      root.appendChild(text);
    }
    if (!root || !api || !id) return missing();
    if (!embedded) document.body.classList.add("jc-page");
    fetch(api + "/api/v1/d/" + id + "/config", { credentials: "omit" })
      .then(function (answer) { return answer.ok ? answer.json() : Promise.reject(answer.status); })
      .then(function (config) {
        if (!mayShow(framed, location.ancestorOrigins, document.referrer, config.frameOrigins)) return missing();
        config.api = api;
        config.assets = "/";
        document.documentElement.lang = (config.languages && config.languages[0]) || "sk";
        if (config.title) document.title = typeof config.title === "string" ? config.title : document.title;
        root.setAttribute("data-config", JSON.stringify(config));
        var script = document.createElement("script");
        script.src = "/widget.js";
        document.body.appendChild(script);
      })
      .catch(missing);
  }

  if (typeof document !== "undefined") start();
  return { deployment: deployment, framedBy: framedBy, mayShow: mayShow, origin: origin };
})();
if (typeof module !== "undefined") module.exports = JCApp;
