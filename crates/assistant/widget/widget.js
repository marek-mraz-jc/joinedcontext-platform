// The knowledge assistant's widget (API/05 §1.6, T-3058, AG-114): one question at a time to the
// chat route of this host, the answer streamed as Server-Sent Events, the conversation and its
// last turns kept in this page alone. No cookie, no storage, no third party.
"use strict";
(function () {
  var root = document.getElementById("jc-chat");
  if (!root) return;
  var config = JSON.parse(root.getAttribute("data-config") || "{}");
  var WORDS = {
    en: { ask: "Your question", send: "Send", tools: "Live data the assistant may read", thinking: "Looking it up…",
      sources: "Sources", script: "Script the assistant ran", failed: "The answer could not be loaded. Try again.",
      you: "You", assistant: "Assistant", reading: "Reading" },
    sk: { ask: "Vaša otázka", send: "Odoslať", tools: "Živé dáta, ktoré smie asistent čítať", thinking: "Hľadám…",
      sources: "Zdroje", script: "Skript, ktorý asistent spustil", failed: "Odpoveď sa nepodarilo načítať. Skúste znova.",
      you: "Vy", assistant: "Asistent", reading: "Čítam" },
    cs: { ask: "Vaše otázka", send: "Odeslat", tools: "Živá data, která smí asistent číst", thinking: "Hledám…",
      sources: "Zdroje", script: "Skript, který asistent spustil", failed: "Odpověď se nepodařilo načíst. Zkuste znovu.",
      you: "Vy", assistant: "Asistent", reading: "Čtu" },
    de: { ask: "Ihre Frage", send: "Senden", tools: "Live-Daten, die der Assistent lesen darf", thinking: "Ich suche…",
      sources: "Quellen", script: "Skript, das der Assistent ausgeführt hat", failed: "Die Antwort konnte nicht geladen werden. Bitte erneut versuchen.",
      you: "Sie", assistant: "Assistent", reading: "Lese" },
    fi: { ask: "Kysymyksesi", send: "Lähetä", tools: "Live-data, jota avustaja saa lukea", thinking: "Etsin…",
      sources: "Lähteet", script: "Avustajan ajama skripti", failed: "Vastausta ei voitu ladata. Yritä uudelleen.",
      you: "Sinä", assistant: "Avustaja", reading: "Luen" }
  };
  var languages = (navigator.languages || [navigator.language || "en"]).map(function (l) { return String(l).slice(0, 2); });
  var lang = languages.filter(function (l) { return WORDS[l]; })[0] || "en";
  var w = WORDS[lang];
  document.documentElement.lang = lang;
  if (/^#[0-9a-fA-F]{6}$/.test(config.color || "")) {
    document.documentElement.style.setProperty("--accent", config.color);
  }

  function el(tag, cls, text) {
    var node = document.createElement(tag);
    if (cls) node.className = cls;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  var head = el("header", "jc-head");
  head.appendChild(el("h1", "", config.title || w.assistant));
  var toggles = [];
  if ((config.connectors || []).length > 0) {
    var tools = el("fieldset", "jc-tools");
    tools.appendChild(el("legend", "", w.tools));
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
    head.appendChild(tools);
  }
  var log = el("ol", "jc-log");
  log.setAttribute("aria-live", "polite");
  log.setAttribute("aria-label", config.title || w.assistant);
  var form = el("form", "jc-form");
  var input = el("textarea");
  input.id = "jc-question";
  input.maxLength = 4000;
  input.required = true;
  var label = el("label", "jc-sr", w.ask);
  label.htmlFor = "jc-question";
  input.placeholder = w.ask;
  var send = el("button", "", w.send);
  send.type = "submit";
  form.appendChild(label);
  form.appendChild(input);
  form.appendChild(send);
  root.appendChild(head);
  root.appendChild(log);
  root.appendChild(form);

  if (config.greeting) {
    var hello = el("li", "jc-bot");
    hello.appendChild(el("p", "", config.greeting));
    log.appendChild(hello);
  }

  var conversation = null;
  var history = [];

  function turn(cls, who) {
    var item = el("li", cls);
    item.appendChild(el("span", "jc-sr", who + ": "));
    log.appendChild(item);
    log.scrollTop = log.scrollHeight;
    return item;
  }

  function cite(item, citations) {
    if (!citations || citations.length === 0) return;
    var list = el("ol", "jc-cites");
    list.setAttribute("aria-label", w.sources);
    citations.forEach(function (c) {
      var li = el("li");
      li.value = c.n;
      if (c.url && /^https?:\/\//.test(c.url)) {
        var a = el("a", "", c.url);
        a.href = c.url;
        a.target = "_blank";
        a.rel = "noopener noreferrer";
        li.appendChild(a);
      } else {
        li.textContent = (c.tool || "") + (c.endpoint ? " · " + c.endpoint : "");
      }
      list.appendChild(li);
    });
    item.appendChild(list);
  }

  function handle(item, status, name, data) {
    if (name === "conversation") {
      conversation = data.id;
    } else if (name === "tool" && data.status === "started") {
      status.textContent = w.reading + " " + (data.endpoint || data.name) + "…";
    } else if (name === "script") {
      var details = el("details", "jc-script");
      details.appendChild(el("summary", "", w.script));
      details.appendChild(el("pre", "", data.code));
      details.appendChild(el("pre", "", data.output !== undefined ? data.output : data.error));
      item.appendChild(details);
    } else if (name === "answer") {
      status.remove();
      item.insertBefore(el("p", "", data.text), item.children[1] || null);
      history.push({ role: "assistant", text: String(data.text).slice(0, 4000) });
    } else if (name === "citations") {
      cite(item, data);
    } else if (name === "error") {
      status.remove();
      item.classList.add("jc-error");
      item.appendChild(el("p", "", data.detail || w.failed));
    }
  }

  async function ask(question) {
    var item = turn("jc-bot", w.assistant);
    var status = el("p", "jc-status", w.thinking);
    item.appendChild(status);
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
      body: JSON.stringify(body)
    });
    if (!response.ok || !response.body) {
      var problem = await response.json().catch(function () { return {}; });
      handle(item, status, "error", { detail: problem.detail || w.failed });
      return;
    }
    var reader = response.body.getReader();
    var decoder = new TextDecoder();
    var buffer = "";
    for (;;) {
      var chunk = await reader.read();
      if (chunk.done) break;
      buffer += decoder.decode(chunk.value, { stream: true });
      var at;
      while ((at = buffer.indexOf("\n\n")) >= 0) {
        var block = buffer.slice(0, at);
        buffer = buffer.slice(at + 2);
        var name = "";
        var data = "";
        block.split("\n").forEach(function (line) {
          if (line.indexOf("event:") === 0) name = line.slice(6).trim();
          else if (line.indexOf("data:") === 0) data += line.slice(5).trim();
        });
        if (name) {
          try { handle(item, status, name, JSON.parse(data)); } catch (e) { /* a malformed event is skipped, the stream goes on */ }
        }
      }
    }
    log.scrollTop = log.scrollHeight;
  }

  form.addEventListener("submit", function (event) {
    event.preventDefault();
    var question = input.value.trim();
    if (!question || send.disabled) return;
    turn("jc-you", w.you).appendChild(el("p", "", question));
    input.value = "";
    send.disabled = true;
    ask(question).catch(function () {
      var item = turn("jc-bot jc-error", w.assistant);
      item.appendChild(el("p", "", w.failed));
    }).then(function () {
      send.disabled = false;
      input.focus();
    });
  });
  input.addEventListener("keydown", function (event) {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      form.requestSubmit();
    }
  });
})();
