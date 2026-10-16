// The knowledge assistant's public chat (API/05 §1.6, T-3058, AG-114, T-4138): the full page at
// /d/{publicId} and the panel the launcher (embed.js) opens. One question at a time to the chat
// route of this host, the answer streamed as Server-Sent Events, the conversation and its last
// turns kept in this panel's own sessionStorage, so they survive a page change on the host site
// (T-4174). No cookie, no third party, no HTML from the model.
"use strict";
(function () {
  var root = document.getElementById("jc-chat");
  if (!root) return;
  var config = JSON.parse(root.getAttribute("data-config") || "{}");
  var WORDS = {
    sk: { ask: "Napíšte otázku", send: "Odoslať", stop: "Zastaviť", stopped: "Odpoveď ste zastavili.",
      tools: "Živé dáta, ktoré smie asistent čítať", thinking: "Hľadám…",
      script: "Skript, ktorý asistent spustil", failed: "Odpoveď sa nepodarilo načítať. Skúste to znova.",
      offline: "Nepodarilo sa spojiť so službou. Skontrolujte pripojenie a skúste to znova.",
      busy: "Položili ste veľa otázok naraz. Počkajte chvíľu a skúste to znova.",
      paused: "Asistent teraz neodpovedá. Skúste to neskôr.", you: "Vy", assistant: "Asistent", bot: "AI asistent",
      hello: "Dobrý deň! Na čo sa chcete opýtať?",
      reading: "Čítam", live: "Živé dáta", source: "Zdroj", fresh: "Nová konverzácia", copy: "Kopírovať",
      copied: "Odpoveď je skopírovaná.", close: "Zavrieť", suggested: "Môžete sa opýtať",
      dataset: "Otvoriť dataset", download: "Stiahnuť údaje", language: "Jazyk",
      table: "Údaje, z ktorých asistent odpovedá", up: "Užitočná odpoveď", down: "Neužitočná odpoveď",
      missing: "Čo chýbalo? (nepovinné)", sendNote: "Odoslať", skip: "Preskočiť",
      thanks: "Ďakujeme za hodnotenie.", more: "Ďalšie otázky", contact: "Kontakt", map: "Zobraziť na mape", steps: "Postup", retry: "Skúsiť znova",
      full: "Tento rozhovor už minul svoj podiel asistenta. Ak sa chcete pýtať ďalej, začnite nový rozhovor.",
      unreachable: "Asistent sa teraz nevie spojiť so svojím modelom. Skúste to o minútu znova.",
      unanswered: "Asistent nevedel zostaviť odpoveď. Skúste sa opýtať inými slovami.",
      slow: "Odpoveď trvala príliš dlho. Skúste otázku položiť kratšie alebo inak.",
      choose: "Vyberte jazyk", moreInfo: "Viac", lessInfo: "Menej" },
    en: { ask: "Type your question", send: "Send", stop: "Stop", stopped: "You stopped the answer.",
      tools: "Live data the assistant may read", thinking: "Looking it up…",
      script: "Script the assistant ran", failed: "The answer could not be loaded. Try again.",
      offline: "The service could not be reached. Check your connection and try again.",
      busy: "You asked many questions at once. Wait a moment and try again.",
      paused: "The assistant is not answering right now. Try again later.", you: "You", assistant: "Assistant", bot: "AI assistant",
      hello: "Hello! What would you like to ask?",
      reading: "Reading", live: "Live data", source: "Source", fresh: "New conversation", copy: "Copy",
      copied: "The answer is copied.", close: "Close", suggested: "You can ask",
      dataset: "Open the dataset", download: "Download the data", language: "Language",
      table: "The data the assistant answers from", up: "Helpful answer", down: "Not helpful",
      missing: "What was missing? (optional)", sendNote: "Send", skip: "Skip",
      thanks: "Thank you for the feedback.", more: "More questions", contact: "Contact", map: "Show on the map", steps: "Steps", retry: "Try again",
      full: "This conversation has used up its share of the assistant. To ask more, start a new conversation.",
      unreachable: "The assistant cannot reach its model right now. Try again in a minute.",
      unanswered: "The assistant could not put an answer together. Try asking in other words.",
      slow: "The answer took too long. Try a shorter or different question.",
      choose: "Choose your language", moreInfo: "More", lessInfo: "Less" },
    cs: { ask: "Napište otázku", send: "Odeslat", stop: "Zastavit", stopped: "Odpověď jste zastavili.",
      tools: "Živá data, která smí asistent číst", thinking: "Hledám…",
      script: "Skript, který asistent spustil", failed: "Odpověď se nepodařilo načíst. Zkuste to znovu.",
      offline: "Službu se nepodařilo zastihnout. Zkontrolujte připojení a zkuste to znovu.",
      busy: "Položili jste mnoho otázek najednou. Chvíli počkejte a zkuste to znovu.",
      paused: "Asistent teď neodpovídá. Zkuste to později.", you: "Vy", assistant: "Asistent", bot: "AI asistent",
      hello: "Dobrý den! Na co se chcete zeptat?",
      reading: "Čtu", live: "Živá data", source: "Zdroj", fresh: "Nová konverzace", copy: "Kopírovat",
      copied: "Odpověď je zkopírovaná.", close: "Zavřít", suggested: "Můžete se zeptat",
      dataset: "Otevřít dataset", download: "Stáhnout data", language: "Jazyk",
      table: "Data, ze kterých asistent odpovídá", up: "Užitečná odpověď", down: "Neužitečná odpověď",
      missing: "Co chybělo? (nepovinné)", sendNote: "Odeslat", skip: "Přeskočit",
      thanks: "Děkujeme za hodnocení.", more: "Další otázky", contact: "Kontakt", map: "Zobrazit na mapě", steps: "Postup", retry: "Zkusit znovu",
      full: "Tento rozhovor už vyčerpal svůj podíl asistenta. Chcete-li se ptát dál, začněte novou konverzaci.",
      unreachable: "Asistent se teď nemůže spojit se svým modelem. Zkuste to za minutu znovu.",
      unanswered: "Asistent nedokázal sestavit odpověď. Zkuste se zeptat jinými slovy.",
      slow: "Odpověď trvala příliš dlouho. Zkuste otázku položit kratší nebo jinak.",
      choose: "Vyberte jazyk", moreInfo: "Více", lessInfo: "Méně" }
  };
  // Each language's name in itself, for the toggle's labels and the `ask` mode's chooser (T-4283).
  var NATIVE = { sk: "Slovensky", en: "English", cs: "Česky" };
  var params = new URLSearchParams(location.search);
  var embedded = params.get("embed") === "1" && window.parent !== window;
  var framed = window.parent !== window;
  var page = document.body.classList.contains("jc-page");
  var offered = JCRender.ordered((config.languages || []).filter(function (l) { return WORDS[l]; }));
  // How the chat picks its language and whether it shows the toggle, the deployment's
  // `widget` (T-4343): `browser` by default, `ask` keeps the first-open chooser, `fixed` its first.
  var mode = (config.widget && config.widget.language) || "browser";
  var toggled = !(config.widget && config.widget.languageToggle === false);
  // The visitor's own choice, kept in this browser's storage; storage may be off, then the
  // chat opens by the browser's languages again (or the chooser asks again, in `ask` mode).
  var LANG_KEY = "jc-chat-lang";
  function storedLanguage() {
    try { return JCRender.chosen(localStorage.getItem(LANG_KEY), offered); } catch (e) { return null; }
  }
  function storeLanguage(code) {
    try { localStorage.setItem(LANG_KEY, code); } catch (e) { /* asked again next time */ }
  }
  // Without the toggle there is nothing to pick, so an old pick is not read.
  var stored = toggled ? storedLanguage() : null;
  // The browser's languages are read here and never sent: only the chosen code is used.
  var browser = navigator.languages && navigator.languages.length ? navigator.languages : [navigator.language];
  var lang = JCRender.language(params.get("lang"), config.languages, WORDS, stored, browser, mode);
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
    node[prop || "textContent"] = typeof key === "function" ? key() : w[key];
    return node;
  }
  function button(cls, key) {
    var b = el("button", cls);
    b.type = "button";
    return words(b, key);
  }

  var SVG = "http://www.w3.org/2000/svg";

  function svg(tag, attrs, text) {
    var node = document.createElementNS(SVG, tag);
    Object.keys(attrs).forEach(function (k) { node.setAttribute(k, String(attrs[k])); });
    if (text) node.textContent = text;
    return node;
  }

  // The widget's few icons, drawn inline (no font, no image request): 24-unit paths, stroked.
  var ICONS = {
    fresh: "M12 5v14M5 12h14",
    close: "M6 6l12 12M18 6L6 18",
    chat: "M4 5h16v11H9l-5 4z",
    send: "M5 12h13M12 5l7 7-7 7",
    stop: "M7 7h10v10H7z",
    copy: "M9 9h10v10H9zM5 15V5h10",
    up: "M7 11v9H4v-9zM7 11l4-7c1.5 0 2.5 1 2 3l-1 3h6c1 0 2 1 1.7 2.2l-1.5 6C18 19.3 17.2 20 16 20H7",
    down: "M7 13V4H4v9zM7 13l4 7c1.5 0 2.5-1 2-3l-1-3h6c1 0 2-1 1.7-2.2l-1.5-6C18 4.7 17.2 4 16 4H7"
  };
  function icon(name) {
    var drawing = svg("svg", { viewBox: "0 0 24 24", width: 20, height: 20, "aria-hidden": "true", focusable: "false", class: "jc-icon" });
    drawing.appendChild(svg("path", { d: ICONS[name] }));
    return drawing;
  }
  // An icon button: its name is the word, read out and shown as a tooltip.
  function iconButton(cls, key, name) {
    var b = el("button", "jc-icon-button " + cls);
    b.type = "button";
    b.appendChild(icon(name));
    words(b, key, "ariaLabel");
    words(b, key, "title");
    return b;
  }

  var head = el("header", "jc-head");
  var brand = el("div", "jc-brand");
  var named = typeof config.title === "string" && config.title.trim() !== "";
  var avatar = el("span", "jc-avatar", named ? JCRender.monogram(config.title) : "");
  avatar.setAttribute("aria-hidden", "true");
  if (!named) avatar.appendChild(icon("chat"));
  var title = el("h1", "jc-title");
  if (named) {
    var name = el("span", "jc-name", config.title.trim());
    name.title = config.title.trim();
    title.appendChild(name);
    title.appendChild(words(el("span", "jc-role"), "assistant"));
  } else {
    title.appendChild(words(el("span", "jc-name"), "assistant"));
  }
  brand.appendChild(avatar);
  brand.appendChild(title);
  var actions = el("div", "jc-actions");
  // The languages as a segmented toggle, SK | EN: nothing drops down over the conversation
  // (T-4283 item 8), and the visitor's choice is kept for the next open.
  var picker = null;
  if (offered.length > 1 && toggled) {
    picker = el("div", "jc-langs");
    picker.setAttribute("role", "group");
    words(picker, "language", "ariaLabel");
    offered.forEach(function (code) {
      var option = el("button", "", code.toUpperCase());
      option.type = "button";
      option.lang = code;
      option.value = code;
      option.setAttribute("aria-label", NATIVE[code] || code);
      option.addEventListener("click", function () {
        storeLanguage(code);
        relabel(code);
      });
      picker.appendChild(option);
    });
    actions.appendChild(picker);
  }
  var fresh = iconButton("jc-fresh", "fresh", "fresh");
  actions.appendChild(fresh);
  if (embedded) {
    var close = iconButton("jc-close", "close", "close");
    close.addEventListener("click", closePanel);
    actions.appendChild(close);
  }
  head.appendChild(brand);
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
  // One rounded field with its send button inside (T-4283); the box grows with the question.
  var send = el("button", "jc-icon-button jc-send");
  send.type = "submit";
  send.appendChild(icon("send"));
  words(send, "send", "ariaLabel");
  words(send, "send", "title");
  var stop = iconButton("jc-stop", "stop", "stop");
  stop.hidden = true;
  input.addEventListener("input", grow);
  function grow() {
    input.style.height = "auto";
    input.style.height = Math.min(input.scrollHeight, 160) + "px";
  }
  form.appendChild(label);
  form.appendChild(input);
  form.appendChild(send);
  form.appendChild(stop);
  var foot = el("footer", "jc-foot");
  // The disclaimer as one line, the rest behind "More" (T-4283).
  var notice = el("div", "jc-notice");
  var disclaimer = el("p", "jc-disclaimer");
  disclaimer.id = "jc-disclaimer";
  var more = el("button", "jc-more");
  more.type = "button";
  more.setAttribute("aria-controls", "jc-disclaimer");
  more.setAttribute("aria-expanded", "false");
  more.addEventListener("click", function () {
    var open = more.getAttribute("aria-expanded") !== "true";
    more.setAttribute("aria-expanded", String(open));
    notice.classList.toggle("jc-open", open);
    more.textContent = open ? w.lessInfo : w.moreInfo;
  });
  notice.appendChild(disclaimer);
  notice.appendChild(more);
  foot.appendChild(form);
  foot.appendChild(notice);

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
  // Questions the server counted in this conversation: an answer's `turn` for its feedback.
  var asked = 0;
  var kept = null;
  var KEY = "jc-chat:" + config.publicId;

  // sessionStorage may be off (a blocked third-party frame, a private window): the chat works
  // without it, it only forgets on a page change then.
  function keep() {
    try {
      if (kept) sessionStorage.setItem(KEY, JSON.stringify(kept));
      else sessionStorage.removeItem(KEY);
    } catch (e) { /* not kept */ }
  }
  function kept0() {
    try { return JCRender.restore(sessionStorage.getItem(KEY)); } catch (e) { return null; }
  }

  function perLanguage(map) {
    if (!map) return null;
    return map[lang] || map[config.languages && config.languages[0]] || null;
  }

  // The empty chat: the greeting and the suggested questions, nothing else above the fold.
  function welcome() {
    intro = el("li", "jc-intro");
    var hello = JCRender.greeting(config.greeting, config.languages, lang, w.hello);
    if (hello) intro.appendChild(el("p", "jc-greeting", hello));
    // The deployment's own questions in this language, and none it did not set (T-4283).
    var asks = JCRender.suggestions(config.suggestions, lang, []);
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
    bound.forEach(function (b) { b[0][b[2]] = typeof b[1] === "function" ? b[1]() : w[b[1]]; });
    if (picker) {
      Array.prototype.forEach.call(picker.children, function (b) { b.setAttribute("aria-pressed", String(b.value === code)); });
    }
    disclaimer.textContent = perLanguage(config.disclaimer) || "";
    notice.hidden = !disclaimer.textContent;
    more.textContent = more.getAttribute("aria-expanded") === "true" ? w.lessInfo : w.moreInfo;
    if (turns === 0 && !busy && config.enabled !== false) {
      log.textContent = "";
      welcome();
    }
  }

  // `who` is the word's key, bound like every other word, so a language switch renames the
  // speakers of the turns already shown (T-4278: "Vy" stayed on an English page).
  function turn(cls, who) {
    var item = el("li", cls);
    var speaker = el("span", "jc-sr");
    speaker.appendChild(words(el("span"), who));
    speaker.appendChild(document.createTextNode(": "));
    item.appendChild(speaker);
    log.appendChild(item);
    log.scrollTop = log.scrollHeight;
    return item;
  }

  // When a message was written (T-4339): the visitor's HH:MM, the UTC minute on hover and to a
  // screen reader's time element; nothing when the moment is not known.
  function when(at) {
    var s = JCRender.stamp(at, lang);
    if (!s) return null;
    var node = el("time", "jc-time", s.text);
    node.setAttribute("datetime", s.datetime);
    node.title = s.title;
    return node;
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
          // A marker opens the closed source list first (T-4364).
          to.addEventListener("click", function () {
            var card = document.getElementById(ids + "-" + p);
            if (card && card.parentNode.hidden) reveal(card.parentNode, true);
          });
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
  function show(item, text, citations, turn, at) {
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
    // One row under the answer (T-4364): who and when, copy, thumbs, then the sources' badges and
    // count, which open the cards under the row.
    var tools = el("div", "jc-answer-tools");
    var by = el("span", "jc-by");
    by.appendChild(words(el("span"), "bot"));
    var time = when(at === undefined ? Date.now() : at);
    if (time) {
      by.appendChild(document.createTextNode(" • "));
      by.appendChild(time);
    }
    tools.appendChild(by);
    var copy = iconButton("jc-copy", "copy", "copy");
    copy.addEventListener("click", function () {
      if (!navigator.clipboard) return;
      navigator.clipboard.writeText(text).then(function () { announce.textContent = w.copied; }, function () {});
    });
    tools.appendChild(copy);
    if (turn && config.feedback !== false) thumbs(tools, turn);
    item.appendChild(tools);
    if (grouped.list.length === 0) return;
    var open = el("button", "jc-sources");
    open.type = "button";
    open.id = ids + "-toggle";
    var stack = el("span", "jc-badges");
    stack.setAttribute("aria-hidden", "true");
    JCRender.badges(grouped.list).forEach(function (badge) {
      var b = el("span", "jc-badge", badge.letter);
      b.style.setProperty("--badge", "hsl(" + badge.hue + " 45% 38%)");
      stack.appendChild(b);
    });
    open.appendChild(stack);
    var n = grouped.list.length;
    open.appendChild(words(el("span", "jc-count"), function () { return JCRender.sourceCount(n, lang); }));
    open.appendChild(el("span", "jc-chevron")).setAttribute("aria-hidden", "true");
    tools.appendChild(open);
    var list = el("ul", "jc-cards");
    list.id = ids;
    list.setAttribute("aria-labelledby", open.id);
    open.setAttribute("aria-controls", list.id);
    grouped.list.forEach(function (source, i) {
      var li = el("li", "jc-card");
      li.id = ids + "-" + (i + 1);
      var name = (source.live ? w.live + ": " : "") + JCRender.label(source);
      // Only an http(s) address is a link: an uploaded file cites `upload:{source}/{name}` (T-4184).
      var href = JCRender.safeUrl(source.url);
      var heading = href ? link(href, name) : el("span", "", name);
      li.appendChild(el("span", "jc-card-n", String(i + 1)));
      li.appendChild(heading);
      if (source.domain && !source.live) li.appendChild(el("span", "jc-domain", source.domain));
      source.cards.forEach(function (card) { li.appendChild(showCard(card)); });
      list.appendChild(li);
    });
    list.toggle = open;
    open.addEventListener("click", function () { reveal(list, list.hidden); });
    reveal(list, false);
    tools.parentNode.insertBefore(list, tools.nextSibling);
  }

  // The source cards stay closed under the footer row until its count opens them (T-4364).
  function reveal(list, open) {
    list.hidden = !open;
    list.toggle.setAttribute("aria-expanded", String(open));
  }

  // What the cited page publishes (T-4155): text, and a link only where render.js checked one.
  function showCard(card) {
    var box = el("div", "jc-part jc-part-" + card.kind);
    var item = function (entry) {
      return entry.href ? link(entry.href, entry.text) : el("span", "", entry.text);
    };
    if (card.kind === "contact") {
      box.appendChild(el("strong", "", card.name || w.contact));
      card.phones.concat(card.emails).forEach(function (entry) {
        box.appendChild(el("p")).appendChild(item(entry));
      });
    } else if (card.kind === "place") {
      box.appendChild(el("p", "", card.address));
      box.appendChild(el("p")).appendChild(link(card.href, w.map));
    } else {
      box.appendChild(el("strong", "", w.steps));
      var steps = box.appendChild(el("ol"));
      card.steps.forEach(function (step) { steps.appendChild(el("li", "", step)); });
    }
    return box;
  }

  // Thumbs up or down on one answer, a thumbs down asking one optional note (API/05 §1.8).
  function thumbs(tools, turn) {
    var of = conversation;
    var up = iconButton("jc-thumb", "up", "up");
    var down = iconButton("jc-thumb", "down", "down");
    function said(sentence) {
      var p = el("p", "jc-thanks", sentence);
      p.setAttribute("role", "status");
      tools.parentNode.insertBefore(p, tools.nextSibling);
    }
    async function vote(verdict, note) {
      up.disabled = true;
      down.disabled = true;
      var body = { conversation: of, turn: turn, verdict: verdict };
      if (note) body.note = note;
      try {
        var response = await fetch("/api/v1/d/" + encodeURIComponent(config.publicId) + "/feedback", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          credentials: "omit",
          body: JSON.stringify(body)
        });
        if (response.ok) return said(w.thanks);
        said(w[refusal(response.status)]);
        // A refused note may be sent again without it.
        up.disabled = false;
        down.disabled = false;
      } catch (e) {
        said(w.offline);
        up.disabled = false;
        down.disabled = false;
      }
    }
    up.addEventListener("click", function () { vote("up"); });
    var asking = null;
    down.addEventListener("click", function () {
      if (asking) return asking.querySelector("textarea").focus();
      var box = asking = el("form", "jc-note");
      var area = el("textarea");
      area.maxLength = 500;
      area.rows = 2;
      area.setAttribute("aria-label", w.missing);
      area.placeholder = w.missing;
      var go = el("button", "", w.sendNote);
      go.type = "submit";
      var skip = el("button", "", w.skip);
      skip.type = "button";
      box.appendChild(area);
      box.appendChild(go);
      box.appendChild(skip);
      box.addEventListener("submit", function (event) {
        event.preventDefault();
        box.remove();
        asking = null;
        vote("down", area.value.trim());
      });
      skip.addEventListener("click", function () {
        box.remove();
        asking = null;
        vote("down");
      });
      tools.parentNode.insertBefore(box, tools.nextSibling);
      area.focus();
    });
    tools.appendChild(up);
    tools.appendChild(down);
  }

  // Up to three follow-up questions as chips under the answer; one click asks it (T-4174).
  function chips(item, questions) {
    if (questions.length === 0) return;
    var list = el("ul", "jc-suggest jc-followups");
    words(list, "more", "ariaLabel");
    questions.forEach(function (question) {
      var li = el("li");
      var b = el("button", "", question);
      b.type = "button";
      b.addEventListener("click", function () { submit(question); });
      li.appendChild(b);
      list.appendChild(li);
    });
    item.appendChild(list);
  }

  // A data round's one numeric column as a small line or bar chart under its table (T-4198). The
  // table beside it is the text alternative, so the drawing is hidden from screen readers; every
  // label goes in as text, never as markup.
  function plot(shown) {
    if (!shown) return null;
    var W = 300, H = 90, top = 14, base = top + H;
    var values = shown.points.map(function (p) { return p.value; });
    var low = Math.min(0, Math.min.apply(null, values)), high = Math.max(0, Math.max.apply(null, values));
    var span = high - low || 1;
    var y = function (v) { return top + H - ((v - low) / span) * H; };
    var step = W / shown.points.length;
    var drawing = svg("svg", { viewBox: "0 0 " + W + " " + (base + 14), class: "jc-data-chart", "aria-hidden": "true", focusable: "false" });
    drawing.appendChild(svg("line", { x1: 0, x2: W, y1: y(0), y2: y(0), class: "jc-chart-axis" }));
    if (shown.kind === "line") {
      var at = shown.points.map(function (p, i) { return (i * step + step / 2).toFixed(1) + "," + y(p.value).toFixed(1); });
      drawing.appendChild(svg("polyline", { points: at.join(" "), class: "jc-chart-line" }));
    } else {
      shown.points.forEach(function (p, i) {
        var a = y(Math.max(p.value, 0)), b = y(Math.min(p.value, 0));
        drawing.appendChild(svg("rect", { x: (i * step + step * 0.1).toFixed(1), width: (step * 0.8).toFixed(1), y: a.toFixed(1), height: Math.max(b - a, 1).toFixed(1), class: "jc-chart-bar" }));
      });
    }
    drawing.appendChild(svg("text", { x: 0, y: 10, class: "jc-chart-text" }, shown.value + " " + Number(high.toPrecision(4))));
    drawing.appendChild(svg("text", { x: 0, y: base + 12, class: "jc-chart-text" }, shown.points[0].label));
    drawing.appendChild(svg("text", { x: W, y: base + 12, "text-anchor": "end", class: "jc-chart-text" }, shown.points[shown.points.length - 1].label));
    return drawing;
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
    var drawn = plot(JCRender.chart(data));
    if (drawn) figure.appendChild(drawn);
    if (shown.dataset || shown.download) {
      var links = el("p", "jc-data-links");
      if (shown.dataset) links.appendChild(link(shown.dataset, w.dataset));
      if (shown.download) links.appendChild(link(shown.download, w.download));
      figure.appendChild(links);
    }
    item.insertBefore(figure, item.status && item.status.parentNode === item ? item.status : null);
    item.data = figure;
  }

  // A failed answer as one quiet line in the visitor's language and a Retry that asks the same
  // question again, through the same route and its per-visitor limit (T-4253).
  function fail(item, word) {
    if (item.status) item.status.remove();
    if (item.streaming) {
      item.streaming.remove();
      item.streaming = null;
    }
    item.classList.add("jc-error");
    var p = el("p", "", w[word]);
    p.setAttribute("role", "alert");
    item.appendChild(p);
    if (!JCRender.retryable(word)) return;
    var again = button("jc-retry", "retry");
    again.addEventListener("click", function () {
      var resend = JCRender.retry(history);
      if (!resend || busy) return;
      history = resend.history;
      var question = item.previousSibling;
      if (question && question.classList.contains("jc-you")) question.remove();
      item.remove();
      submit(resend.question);
    });
    item.appendChild(again);
  }

  function handle(item, name, data) {
    if (name === "conversation") {
      // The server's moment of the question, the one the answer is told, replaces this
      // browser's guess, so the time beside the question is the time the answer states (T-4339).
      var at = JCRender.stamp(data.at, lang);
      if (at && item.askedTime) {
        item.askedTime.textContent = at.text;
        item.askedTime.setAttribute("datetime", at.datetime);
        item.askedTime.title = at.title;
        item.askedAt = data.at;
      }
      if (data.id !== conversation) asked = 0;
      conversation = data.id;
      asked += 1;
      item.turn = asked;
    } else if (name === "stage" && item.status) {
      (item.stages = item.stages || []).push(data);
      var line = JCRender.stage(data, lang);
      if (line) {
        item.status.textContent = line;
        item.staged = true;
      }
    } else if (name === "tool" && data.status === "started" && item.status && !item.staged) {
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
      var cited = Array.isArray(data) ? data : [];
      show(item, item.pending || "", cited, item.turn);
      var took = JCRender.steps(item.stages, lang);
      if (took) {
        var list = el("details", "jc-steps");
        list.appendChild(el("summary", "", took.summary));
        var ol = el("ol");
        took.lines.forEach(function (line) { ol.appendChild(el("li", "", line)); });
        list.appendChild(ol);
        item.appendChild(list);
      }
      saved(item, item.pending || "", cited);
      item.pending = null;
    } else if (name === "followups") {
      chips(item, JCRender.followups(data));
    } else if (name === "error") {
      fail(item, JCRender.trouble(data.status, data.title));
    }
  }

  // The answered turn into the panel's sessionStorage, with the question it answered.
  function saved(item, text, citations) {
    if (!conversation || !item.turn) return;
    kept = JCRender.remember(kept, conversation, asked,
      { question: item.question || "", text: text, turn: item.turn, citations: citations,
        askedAt: item.askedAt, at: new Date().toISOString() });
    keep();
  }

  // The server's detail is English and for the logs; the visitor reads the widget's words.
  function refusal(status, title) {
    return JCRender.trouble(status, title);
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
      fail(item, refusal(response.status, problem.title));
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
      show(item, item.pending, [], item.turn);
      saved(item, item.pending, []);
      item.pending = null;
    } else if (item.streaming) {
      show(item, item.streaming.textContent, [], item.turn);
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
    Array.prototype.forEach.call(log.querySelectorAll(".jc-followups"), function (l) { l.remove(); });
    var mine = turn("jc-you", "you");
    mine.appendChild(el("p", "", question));
    var askedAt = new Date().toISOString();
    var time = when(askedAt);
    if (time) mine.appendChild(time);
    input.value = "";
    var item = turn("jc-bot", "assistant");
    item.question = question;
    item.askedAt = askedAt;
    item.askedTime = time;
    item.status = el("p", "jc-status", w.thinking);
    item.status.setAttribute("role", "status");
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
      fail(item, error instanceof TypeError ? "offline" : "failed");
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
    asked = 0;
    kept = null;
    keep();
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
  // The conversation open before a page change on the host site, shown again as it was.
  kept = config.enabled === false ? null : kept0();
  if (kept && kept.turns.length > 0) {
    conversation = kept.conversation;
    asked = kept.asked;
    log.textContent = "";
    intro = null;
    kept.turns.forEach(function (t) {
      var mine = turn("jc-you", "you");
      mine.appendChild(el("p", "", t.question));
      var time = when(t.askedAt);
      if (time) mine.appendChild(time);
      show(turn("jc-bot", "assistant"), t.text, t.citations, t.turn, t.at || null);
      history.push({ role: "user", text: t.question });
      history.push({ role: "assistant", text: t.text });
    });
    announce.textContent = "";
  }
  if (config.enabled === false) {
    // A paused deployment says so instead of offering a box that would only refuse.
    log.textContent = "";
    var paused = el("li", "jc-intro");
    paused.appendChild(words(el("p", "jc-greeting"), "paused"));
    log.appendChild(paused);
    form.hidden = true;
    fresh.hidden = true;
  }
  if (JCRender.asksLanguage({ mode: mode, offered: offered, stored: stored, kept: Boolean(kept && kept.turns.length > 0),
    enabled: config.enabled, page: page, requested: params.get("lang") })) {
    choose();
  }

  // T-4283 item 13: on the first open the panel asks for the language before anything else,
  // Slovak first and focused; the answer is kept, so later opens go straight to the chat.
  function choose() {
    log.textContent = "";
    intro = null;
    root.classList.add("jc-choosing");
    var box = el("li", "jc-intro jc-choose");
    var heading = el("p", "jc-choose-title");
    heading.id = "jc-choose-title";
    heading.textContent = offered.map(function (code) { return WORDS[code].choose; })
      .filter(function (text, i, all) { return all.indexOf(text) === i; }).join(" / ");
    var group = el("div", "jc-choose-list");
    group.setAttribute("role", "group");
    group.setAttribute("aria-labelledby", heading.id);
    offered.forEach(function (code) {
      var b = el("button", "", NATIVE[code] || code.toUpperCase());
      b.type = "button";
      b.lang = code;
      b.addEventListener("click", function () {
        storeLanguage(code);
        root.classList.remove("jc-choosing");
        relabel(code);
        input.focus();
      });
      group.appendChild(b);
    });
    box.appendChild(heading);
    box.appendChild(group);
    log.appendChild(box);
    group.firstChild.focus();
  }

  // T-4283: the page that framed the panel sizes it to this content (Architecture/22 §4), so a
  // short conversation leaves no empty panel. The message carries one number and nothing else.
  var reported = 0;
  var measuring = false;
  // The conversation's own height, its items and their margins, not the space the panel gives
  // it (the first item's automatic top margin is that space).
  function measure() {
    var style = getComputedStyle(log);
    var list = parseFloat(style.paddingTop) + parseFloat(style.paddingBottom);
    Array.prototype.forEach.call(log.children, function (item) {
      list += item.offsetHeight + parseFloat(getComputedStyle(item).marginBottom);
    });
    return Math.ceil(head.offsetHeight + (tools ? tools.offsetHeight : 0) + list + foot.offsetHeight);
  }
  function report() {
    measuring = false;
    var height = measure();
    if (height === reported) return;
    reported = height;
    window.parent.postMessage({ type: "jc-assistant-height", height: height }, "*");
  }
  function schedule() {
    if (measuring) return;
    measuring = true;
    requestAnimationFrame(report);
  }
  if (framed) {
    new MutationObserver(schedule).observe(root, { childList: true, subtree: true, characterData: true, attributes: true });
    window.addEventListener("resize", schedule);
    schedule();
  }
})();
