// The knowledge assistant's public chat (API/05 §1.6, T-3058, AG-114, T-4138): the full page at
// /d/{publicId} and the panel the launcher (embed.js) opens. One question at a time to the chat
// route of this host, the answer streamed as Server-Sent Events, the conversation and its last
// turns kept in this page alone. No cookie, no storage, no third party, no HTML from the model.
"use strict";
(function () {
  var root = document.getElementById("jc-chat");
  if (!root) return;
  var config = JSON.parse(root.getAttribute("data-config") || "{}");
  var WORDS = {
    sk: { ask: "Napíšte otázku", send: "Odoslať", stop: "Zastaviť", stopped: "Odpoveď ste zastavili.",
      tools: "Živé dáta, ktoré smie asistent čítať", thinking: "Hľadám…", sources: "Zdroje",
      script: "Skript, ktorý asistent spustil", failed: "Odpoveď sa nepodarilo načítať. Skúste to znova.",
      offline: "Nepodarilo sa spojiť so službou. Skontrolujte pripojenie a skúste to znova.",
      busy: "Položili ste veľa otázok naraz. Počkajte chvíľu a skúste to znova.",
      paused: "Asistent teraz neodpovedá. Skúste to neskôr.", you: "Vy", assistant: "Asistent",
      reading: "Čítam", live: "Živé dáta", source: "Zdroj", fresh: "Nová konverzácia", copy: "Kopírovať",
      copied: "Odpoveď je skopírovaná.", close: "Zavrieť", suggested: "Môžete sa opýtať",
      dataset: "Otvoriť dataset", download: "Stiahnuť údaje", language: "Jazyk",
      table: "Údaje, z ktorých asistent odpovedá" },
    en: { ask: "Type your question", send: "Send", stop: "Stop", stopped: "You stopped the answer.",
      tools: "Live data the assistant may read", thinking: "Looking it up…", sources: "Sources",
      script: "Script the assistant ran", failed: "The answer could not be loaded. Try again.",
      offline: "The service could not be reached. Check your connection and try again.",
      busy: "You asked many questions at once. Wait a moment and try again.",
      paused: "The assistant is not answering right now. Try again later.", you: "You", assistant: "Assistant",
      reading: "Reading", live: "Live data", source: "Source", fresh: "New conversation", copy: "Copy",
      copied: "The answer is copied.", close: "Close", suggested: "You can ask",
      dataset: "Open the dataset", download: "Download the data", language: "Language",
      table: "The data the assistant answers from" },
    cs: { ask: "Napište otázku", send: "Odeslat", stop: "Zastavit", stopped: "Odpověď jste zastavili.",
      tools: "Živá data, která smí asistent číst", thinking: "Hledám…", sources: "Zdroje",
      script: "Skript, který asistent spustil", failed: "Odpověď se nepodařilo načíst. Zkuste to znovu.",
      offline: "Službu se nepodařilo zastihnout. Zkontrolujte připojení a zkuste to znovu.",
      busy: "Položili jste mnoho otázek najednou. Chvíli počkejte a zkuste to znovu.",
      paused: "Asistent teď neodpovídá. Zkuste to později.", you: "Vy", assistant: "Asistent",
      reading: "Čtu", live: "Živá data", source: "Zdroj", fresh: "Nová konverzace", copy: "Kopírovat",
      copied: "Odpověď je zkopírovaná.", close: "Zavřít", suggested: "Můžete se zeptat",
      dataset: "Otevřít dataset", download: "Stáhnout data", language: "Jazyk",
      table: "Data, ze kterých asistent odpovídá" }
  };
  var params = new URLSearchParams(location.search);
  var embedded = params.get("embed") === "1" && window.parent !== window;
  var offered = (config.languages || []).filter(function (l) { return WORDS[l]; });
  var lang = JCRender.language(params.get("lang"), config.languages, WORDS);
  var w = WORDS[lang];
  if (/^#[0-9a-fA-F]{6}$/.test(config.color || "")) {
    document.documentElement.style.setProperty("--accent", config.color);
  }

  function el(tag, cls, text) {
    var node = document.createElement(tag);
    if (cls) node.className = cls;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  // Every word on the page is bound to its key, so the language switch relabels them in place.
  var bound = [];
  function words(node, key, prop) {
    bound.push([node, key, prop || "textContent"]);
    node[prop || "textContent"] = w[key];
    return node;
  }
  function button(cls, key) {
    var b = el("button", cls);
    b.type = "button";
    return words(b, key);
  }

  var head = el("header", "jc-head");
  var title = words(el("h1"), "assistant");
  var actions = el("div", "jc-actions");
  var switches = [];
  if (offered.length > 1) {
    var picker = el("div", "jc-langs");
    picker.setAttribute("role", "group");
    words(picker, "language", "ariaLabel");
    offered.forEach(function (code) {
      var b = el("button", "jc-lang", code.toUpperCase());
      b.type = "button";
      b.lang = code;
      b.addEventListener("click", function () { relabel(code); });
      picker.appendChild(b);
      switches.push(b);
    });
    actions.appendChild(picker);
  }
  var fresh = button("jc-fresh", "fresh");
  actions.appendChild(fresh);
  if (embedded) {
    var close = button("jc-close", "close");
    close.addEventListener("click", closePanel);
    actions.appendChild(close);
  }
  head.appendChild(title);
  head.appendChild(actions);

  var toggles = [];
  var tools = null;
  if ((config.connectors || []).length > 0) {
    tools = el("details", "jc-tools");
    tools.appendChild(words(el("summary"), "tools"));
    config.connectors.forEach(function (name) {
      var label = el("label");
      var box = el("input");
      box.type = "checkbox";
      box.checked = true;
      box.value = name;
      label.appendChild(box);
      label.appendChild(document.createTextNode(name));
      tools.appendChild(label);
      toggles.push(box);
    });
  }

  var log = el("ol", "jc-log");
  words(log, "assistant", "ariaLabel");
  // The finished answer is read out once from here; the log itself is not live, so a streamed
  // answer is not announced character by character.
  var announce = el("p", "jc-sr");
  announce.setAttribute("aria-live", "polite");

  var form = el("form", "jc-form");
  var input = el("textarea");
  input.id = "jc-question";
  input.rows = 1;
  input.maxLength = config.questionChars || 1000;
  input.required = true;
  var label = words(el("label", "jc-sr"), "ask");
  label.htmlFor = "jc-question";
  words(input, "ask", "placeholder");
  var send = words(el("button", "jc-send"), "send");
  send.type = "submit";
  var stop = button("jc-stop", "stop");
  stop.hidden = true;
  form.appendChild(label);
  form.appendChild(input);
  form.appendChild(send);
  form.appendChild(stop);
  var foot = el("footer", "jc-foot");
  var disclaimer = el("p", "jc-disclaimer");
  foot.appendChild(form);
  foot.appendChild(disclaimer);

  root.appendChild(head);
  if (tools) root.appendChild(tools);
  root.appendChild(log);
  root.appendChild(foot);
  root.appendChild(announce);

  var conversation = null;
  var history = [];
  var turns = 0;
  var busy = null;
  var intro = null;

  function perLanguage(map) {
    if (!map) return null;
    return map[lang] || map[config.languages && config.languages[0]] || null;
  }

  // The empty chat: the greeting and the suggested questions, nothing else above the fold.
  function welcome() {
    intro = el("li", "jc-intro");
    if (config.greeting) intro.appendChild(el("p", "jc-greeting", config.greeting));
    var asks = (perLanguage(config.suggestions) || []).slice(0, 6);
    if (asks.length > 0) {
      var list = el("ul", "jc-suggest");
      list.setAttribute("aria-label", w.suggested);
      asks.forEach(function (question) {
        var item = el("li");
        var b = el("button", "", question);
        b.type = "button";
        b.addEventListener("click", function () { submit(question); });
        item.appendChild(b);
        list.appendChild(item);
      });
      intro.appendChild(list);
    }
    if (intro.children.length > 0) log.appendChild(intro);
  }

  function relabel(code) {
    lang = code;
    w = WORDS[code];
    document.documentElement.lang = code;
    bound.forEach(function (b) { b[0][b[2]] = w[b[1]]; });
    switches.forEach(function (b) { b.setAttribute("aria-pressed", String(b.lang === code)); });
    disclaimer.textContent = perLanguage(config.disclaimer) || "";
    disclaimer.hidden = !disclaimer.textContent;
    if (turns === 0 && !busy && config.enabled !== false) {
      log.textContent = "";
      welcome();
    }
  }

  function turn(cls, who) {
    var item = el("li", cls);
    item.appendChild(el("span", "jc-sr", who + ": "));
    log.appendChild(item);
    log.scrollTop = log.scrollHeight;
    return item;
  }

  function link(href, text, cls) {
    var a = el("a", cls || "", text);
    a.href = href;
    a.target = "_blank";
    a.rel = "noopener noreferrer";
    return a;
  }

  var ALLOWED = { p: 1, ul: 1, ol: 1, li: 1, strong: 1, em: 1, code: 1, br: 1 };

  // The renderer's tree as DOM nodes: text as text, only the elements it may produce.
  function build(node, ids, position) {
    if (typeof node === "string") return document.createTextNode(node);
    var out;
    if (node.tag === "a") {
      out = link(node.href, "");
    } else if (node.tag === "cite") {
      out = el("sup", "jc-cite");
      node.numbers.map(function (n) { return position[n]; })
        .filter(function (p, i, all) { return p && all.indexOf(p) === i; })
        .forEach(function (p, i) {
          if (i > 0) out.appendChild(document.createTextNode(","));
          var to = el("a", "", String(p));
          to.href = "#" + ids + "-" + p;
          to.setAttribute("aria-label", w.source + " " + p);
          out.appendChild(to);
        });
      return out;
    } else {
      out = el(ALLOWED[node.tag] ? node.tag : "span");
    }
    (node.children || []).forEach(function (child) { out.appendChild(build(child, ids, position)); });
    return out;
  }

  // The answer, formatted, its markers linked to the source cards under it, and its copy button.
  function show(item, text, citations) {
    turns += 1;
    var ids = "jc-src-" + turns;
    var grouped = JCRender.sources(citations);
    var body = el("div", "jc-answer");
    // Links stay links only to the deployment's own hosts and the sources this answer cites (T-4149).
    var hosts = (config.linkHosts || []).concat(grouped.list.map(function (source) { return source.domain; }));
    var linkable = function (url) { return JCRender.linkHost(url, hosts); };
    JCRender.markdown(text, function (n) { return Boolean(grouped.position[n]); }, linkable).forEach(function (block) {
      body.appendChild(build(block, ids, grouped.position));
    });
    if (item.streaming) {
      item.replaceChild(body, item.streaming);
      item.streaming = null;
    } else {
      item.appendChild(body);
    }
    announce.textContent = body.textContent;
    if (grouped.list.length > 0) {
      var list = el("ul", "jc-cards");
      list.setAttribute("aria-label", w.sources);
      grouped.list.forEach(function (source, i) {
        var li = el("li", "jc-card");
        li.id = ids + "-" + (i + 1);
        var name = (source.live ? w.live + ": " : "") + JCRender.label(source);
        var heading = source.url ? link(source.url, name) : el("span", "", name);
        li.appendChild(el("span", "jc-card-n", String(i + 1)));
        li.appendChild(heading);
        if (source.domain && !source.live) li.appendChild(el("span", "jc-domain", source.domain));
        list.appendChild(li);
      });
      item.appendChild(list);
    }
    var tools = el("div", "jc-answer-tools");
    var copy = el("button", "jc-copy", w.copy);
    copy.type = "button";
    copy.addEventListener("click", function () {
      if (!navigator.clipboard) return;
      navigator.clipboard.writeText(text).then(function () { announce.textContent = w.copied; }, function () {});
    });
    tools.appendChild(copy);
    item.appendChild(tools);
  }

  // A data round's rows as a table with the dataset and download links (T-4140).
  function showData(item, data) {
    var shown = JCRender.table(data);
    if (!shown) return;
    var figure = el("figure", "jc-data");
    var scroll = el("div", "jc-data-scroll");
    scroll.tabIndex = 0;
    scroll.setAttribute("role", "region");
    scroll.setAttribute("aria-label", w.table);
    var table = el("table");
    if (shown.caption) table.appendChild(el("caption", "", shown.caption));
    var headRow = el("tr");
    shown.columns.forEach(function (c) {
      var th = el("th", "", c);
      th.scope = "col";
      headRow.appendChild(th);
    });
    table.appendChild(el("thead")).appendChild(headRow);
    var body = table.appendChild(el("tbody"));
    shown.rows.forEach(function (row) {
      var tr = body.appendChild(el("tr"));
      row.forEach(function (cell) { tr.appendChild(el("td", "", cell)); });
    });
    scroll.appendChild(table);
    figure.appendChild(scroll);
    if (shown.dataset || shown.download) {
      var links = el("p", "jc-data-links");
      if (shown.dataset) links.appendChild(link(shown.dataset, w.dataset));
      if (shown.download) links.appendChild(link(shown.download, w.download));
      figure.appendChild(links);
    }
    item.insertBefore(figure, item.status && item.status.parentNode === item ? item.status : null);
    item.data = figure;
  }

  function fail(item, sentence) {
    if (item.status) item.status.remove();
    if (item.streaming) {
      item.streaming.remove();
      item.streaming = null;
    }
    item.classList.add("jc-error");
    var p = el("p", "", sentence);
    p.setAttribute("role", "alert");
    item.appendChild(p);
  }

  function handle(item, name, data) {
    if (name === "conversation") {
      conversation = data.id;
    } else if (name === "tool" && data.status === "started" && item.status) {
      item.status.textContent = w.reading + " " + (data.endpoint || data.name) + "…";
    } else if (name === "script") {
      var details = el("details", "jc-script");
      details.appendChild(el("summary", "", w.script));
      details.appendChild(el("pre", "", data.code));
      details.appendChild(el("pre", "", data.output !== undefined ? data.output : data.error));
      item.appendChild(details);
    } else if (name === "data") {
      showData(item, data);
    } else if (name === "delta" && typeof data.text === "string") {
      if (item.status) {
        item.status.remove();
        item.status = null;
      }
      if (!item.streaming) {
        item.streaming = el("div", "jc-answer jc-streaming");
        item.appendChild(item.streaming);
      }
      item.streaming.textContent += data.text;
      log.scrollTop = log.scrollHeight;
    } else if (name === "answer") {
      // Shown with its citations, which come next; `ask` shows it alone if they never do.
      if (item.status) {
        item.status.remove();
        item.status = null;
      }
      item.pending = String(data.text);
      history.push({ role: "assistant", text: item.pending.slice(0, 4000) });
    } else if (name === "citations") {
      show(item, item.pending || "", Array.isArray(data) ? data : []);
      item.pending = null;
    } else if (name === "error") {
      fail(item, data.detail || w.failed);
    }
  }

  function refusal(status, detail) {
    if (detail) return detail;
    if (status === 429) return w.busy;
    if (status === 503) return w.paused;
    return w.failed;
  }

  async function ask(question, item, signal) {
    var body = { message: question, history: history.slice(-6) };
    if (conversation) body.conversation = conversation;
    if (toggles.length > 0) {
      body.connectors = toggles.filter(function (b) { return b.checked; }).map(function (b) { return b.value; });
    }
    history.push({ role: "user", text: question });
    var response = await fetch("/api/v1/d/" + encodeURIComponent(config.publicId) + "/chat", {
      method: "POST",
      headers: { "Content-Type": "application/json", Accept: "text/event-stream" },
      credentials: "omit",
      body: JSON.stringify(body),
      signal: signal
    });
    if (!response.ok || !response.body) {
      var problem = await response.json().catch(function () { return {}; });
      fail(item, refusal(response.status, problem.detail));
      return;
    }
    var reader = response.body.getReader();
    var decoder = new TextDecoder();
    var buffer = "";
    for (;;) {
      var chunk = await reader.read();
      if (chunk.done) break;
      var read = JCRender.events(buffer + decoder.decode(chunk.value, { stream: true }));
      buffer = read.rest;
      read.events.forEach(function (event) { handle(item, event.name, event.data); });
    }
    if (item.pending !== null && item.pending !== undefined) {
      show(item, item.pending, []);
      item.pending = null;
    } else if (item.streaming) {
      show(item, item.streaming.textContent, []);
    }
    log.scrollTop = log.scrollHeight;
  }

  function settle() {
    busy = null;
    send.hidden = false;
    stop.hidden = true;
    input.focus();
  }

  function submit(question) {
    question = String(question || "").trim();
    if (!question || busy) return;
    if (intro) {
      var list = intro.querySelector(".jc-suggest");
      if (list) list.remove();
      if (intro.children.length === 0) intro.remove();
      intro = null;
    }
    turn("jc-you", w.you).appendChild(el("p", "", question));
    input.value = "";
    var item = turn("jc-bot", w.assistant);
    item.status = el("p", "jc-status", w.thinking);
    item.appendChild(item.status);
    busy = new AbortController();
    send.hidden = true;
    stop.hidden = false;
    var controller = busy;
    ask(question, item, controller.signal).catch(function (error) {
      if (controller.signal.aborted) {
        // What arrived stays; the visitor sees that they stopped it.
        if (item.status) item.status.remove();
        if (item.streaming) show(item, item.streaming.textContent, []);
        item.appendChild(el("p", "jc-status", w.stopped));
        return;
      }
      fail(item, error instanceof TypeError ? w.offline : w.failed);
    }).then(function () {
      if (busy === controller) settle();
    });
  }

  function closePanel() {
    // The launcher on the host page closes the panel and puts focus back on its button; the
    // message carries no data.
    if (embedded) window.parent.postMessage({ type: "jc-assistant-close" }, "*");
  }

  form.addEventListener("submit", function (event) {
    event.preventDefault();
    submit(input.value);
  });
  stop.addEventListener("click", function () {
    if (busy) busy.abort();
  });
  fresh.addEventListener("click", function () {
    if (busy) busy.abort();
    conversation = null;
    history = [];
    turns = 0;
    log.textContent = "";
    announce.textContent = "";
    busy = null;
    settle();
    welcome();
  });
  input.addEventListener("keydown", function (event) {
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      form.requestSubmit();
    }
  });
  document.addEventListener("keydown", function (event) {
    if (event.key === "Escape") closePanel();
  });

  relabel(lang);
  if (config.enabled === false) {
    // A paused deployment says so instead of offering a box that would only refuse.
    log.textContent = "";
    var paused = el("li", "jc-intro");
    paused.appendChild(words(el("p", "jc-greeting"), "paused"));
    log.appendChild(paused);
    form.hidden = true;
    fresh.hidden = true;
  }
})();
