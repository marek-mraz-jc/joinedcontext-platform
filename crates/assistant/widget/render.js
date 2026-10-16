// What the widget shows of an answer (T-3325): a small, safe part of Markdown read into a plain
// tree, and the answer's citations grouped into its sources. Nothing here touches the page: the
// widget builds DOM nodes from the tree with createElement and textContent alone, so no HTML the
// model writes is ever parsed. Loaded before widget.js; `node --test` reads it as a module.
"use strict";
var JCRender = (function () {
  // Bold, italic, inline code, an http(s) link, and a citation marker `[1]` or `[1, 2]`.
  var INLINE = /\*\*(.+?)\*\*|__(.+?)__|`([^`]+)`|\[([^\]\n]+)\]\((https?:\/\/[^\s()]+)\)|\[(\d+(?:\s*,\s*\d+)*)\]|\*(?=\S)(.+?)\*|(^|[^\w])_(?=\S)(.+?)_(?!\w)/g;

  function inline(text, known, linkable) {
    var out = [];
    var last = 0;
    var match;
    var re = new RegExp(INLINE.source, "g");
    while ((match = re.exec(text)) !== null) {
      // `_em_` takes the character before it, so a snake_case name stays one word.
      out.push(text.slice(last, match.index) + (match[8] || ""));
      if (match[1] !== undefined || match[2] !== undefined) {
        out.push({ tag: "strong", children: inline(match[1] !== undefined ? match[1] : match[2], known, linkable) });
      } else if (match[3] !== undefined) {
        out.push({ tag: "code", children: [match[3]] });
      } else if (match[4] !== undefined) {
        var words = inline(match[4], known, linkable);
        // A link off the allowlist is its words alone (T-4149): an address a passage planted is
        // never one click away. `![alt](url)` is a "!" and the same link or words: never an image.
        if (linkable(match[5])) out.push({ tag: "a", href: match[5], children: words });
        else out = out.concat(words);
      } else if (match[6] !== undefined) {
        var numbers = match[6].split(",").map(function (n) { return Number(n.trim()); })
          .filter(function (n) { return known(n); });
        // A marker no citation stands behind is dropped, never shown as typed.
        if (numbers.length > 0) out.push({ tag: "cite", numbers: numbers });
      } else {
        out.push({ tag: "em", children: inline(match[7] !== undefined ? match[7] : match[9], known, linkable) });
      }
      last = re.lastIndex;
    }
    out.push(text.slice(last));
    return out.filter(function (node) { return node !== ""; });
  }

  var BULLET = /^\s*[-*+]\s+(.*)$/;
  var NUMBERED = /^\s*\d{1,3}[.)]\s+(.*)$/;
  var HEADING = /^\s{0,3}#{1,6}\s+(.*?)\s*#*\s*$/;

  // The citation numbers an item carries at its own level, as one key; "" when it cites nothing.
  function citeKey(item) {
    var numbers = [];
    item.children.forEach(function (node) {
      if (node && node.tag === "cite") numbers = numbers.concat(node.numbers);
    });
    return numbers.filter(function (n, i) { return numbers.indexOf(n) === i; }).sort(function (a, b) { return a - b; }).join(",");
  }

  // An item without its markers, and without the commas and spaces that only joined them.
  function uncited(item) {
    var kept = item.children.filter(function (node) { return !(node && node.tag === "cite"); });
    var at = kept.length - 1;
    while (at >= 0 && typeof kept[at] === "string" && /^[\s,;]*$/.test(kept[at])) at -= 1;
    kept = kept.slice(0, at + 1);
    if (typeof kept[at] === "string") kept[at] = kept[at].replace(/[\s,;]+$/, "");
    item.children = kept.filter(function (node) { return node !== ""; });
  }

  /**
   * Consecutive items of a list that cite the same sources keep the markers on the last of them
   * only (T-4278): "[1],[2]" after every weekday of the opening hours was noise on a phone.
   */
  function collapseCites(list) {
    var items = list.children;
    for (var i = 0; i < items.length - 1; i += 1) {
      var key = citeKey(items[i]);
      if (key !== "" && key === citeKey(items[i + 1])) uncited(items[i]);
    }
  }

  /**
   * The answer as blocks: paragraphs (lines joined by line breaks), bullet and numbered lists,
   * headings read as bold paragraphs. `known(n)` says whether citation n exists, `linkable(url)`
   * whether a link to `url` stays a link (none does without it).
   */
  function markdown(text, known, linkable) {
    known = known || function () { return false; };
    linkable = linkable || function () { return false; };
    var blocks = [];
    var paragraph = null;
    var list = null;
    String(text || "").replace(/\r\n?/g, "\n").split("\n").forEach(function (line) {
      var item = BULLET.exec(line);
      var number = item ? null : NUMBERED.exec(line);
      var heading = HEADING.exec(line);
      if (item || number) {
        var tag = item ? "ul" : "ol";
        if (!list || list.tag !== tag) {
          list = { tag: tag, children: [] };
          blocks.push(list);
        }
        list.children.push({ tag: "li", children: inline((item || number)[1], known, linkable) });
        paragraph = null;
      } else if (line.trim() === "") {
        paragraph = null;
        list = null;
      } else if (heading) {
        blocks.push({ tag: "p", children: [{ tag: "strong", children: inline(heading[1], known, linkable) }] });
        paragraph = null;
        list = null;
      } else if (list && /^\s{2,}\S/.test(line)) {
        // An indented line goes on with the list item above it.
        var last = list.children[list.children.length - 1];
        last.children = last.children.concat([{ tag: "br" }], inline(line.trim(), known, linkable));
      } else {
        list = null;
        if (!paragraph) {
          paragraph = { tag: "p", children: [] };
          blocks.push(paragraph);
        } else {
          paragraph.children.push({ tag: "br" });
        }
        paragraph.children = paragraph.children.concat(inline(line.trim(), known, linkable));
      }
    });
    blocks.forEach(function (block) {
      if (block.tag === "ul" || block.tag === "ol") collapseCites(block);
    });
    return blocks;
  }

  function domain(url) {
    var match = /^https?:\/\/([^/?#:]+)/i.exec(url);
    return match ? match[1].replace(/^www\./, "") : "";
  }

  /**
   * Whether `url` goes to one of `hosts` or a host under one, `www.` aside (T-4149). A host with
   * anything but letters, digits, dots and dashes (a `user@host`, a backslash) never matches.
   */
  function linkHost(url, hosts) {
    var match = /^https?:\/\/([^/?#]+)(?:[/?#]|$)/i.exec(String(url || ""));
    if (!match) return false;
    var host = match[1].toLowerCase();
    if (!/^[a-z0-9.-]+$/.test(host)) return false;
    host = host.replace(/^www\./, "");
    return (hosts || []).some(function (h) {
      h = String(h || "").toLowerCase().replace(/^www\./, "");
      return h !== "" && (host === h || host.slice(-(h.length + 1)) === "." + h);
    });
  }

  /**
   * The citations as the sources a visitor reads: one per address, the numbers that share it
   * merged, each with its title and domain. A citation of live data reads as live data and never
   * by its tool's name. `position[n]` is the 1-based place of citation n in `sources`.
   */
  function sources(citations) {
    var list = [];
    var byKey = {};
    var position = {};
    (citations || []).forEach(function (c) {
      if (!c || typeof c.n !== "number") return;
      var url = typeof c.url === "string" && /^https?:\/\//i.test(c.url) ? c.url : null;
      var live = Boolean(c.tool);
      var key = url || (live ? "live:" + (c.endpoint || "") : null);
      if (!key) return;
      if (!(key in byKey)) {
        byKey[key] = list.length;
        list.push({
          url: url,
          title: typeof c.title === "string" && c.title.trim() ? c.title.trim() : null,
          live: live,
          endpoint: typeof c.endpoint === "string" ? c.endpoint : null,
          domain: url ? domain(url) : "",
          numbers: [],
          cards: []
        });
      }
      var entry = list[byKey[key]];
      cards(c.cards).forEach(function (card) {
        var same = JSON.stringify(card);
        if (entry.cards.length < 3 && !entry.cards.some(function (k) { return JSON.stringify(k) === same; })) {
          entry.cards.push(card);
        }
      });
      if (!entry.title && typeof c.title === "string" && c.title.trim()) entry.title = c.title.trim();
      entry.numbers.push(c.n);
      position[c.n] = byKey[key] + 1;
    });
    return { list: list, position: position };
  }

  /** What a source's link says: its title, else the address without its scheme. */
  function label(source) {
    if (source.title) return source.title;
    // An uploaded file is named by its file name (T-4184).
    if (source.url && /^upload:/.test(source.url)) return source.url.replace(/^upload:[^/]*\//, "").replace(/#page=\d+$/, "");
    if (source.url) return source.url.replace(/^https?:\/\//i, "").replace(/\/$/, "");
    return source.endpoint || "";
  }

  /** An `http(s)` address as given, anything else (a `javascript:` link, a relative path) null. */
  function safeUrl(url) {
    return typeof url === "string" && /^https?:\/\/[^\s]+$/i.test(url) ? url : null;
  }

  /**
   * The complete Server-Sent Events in `buffer` (T-4138): each `{name, data}` with its data parsed
   * as JSON, a malformed one skipped, and the unfinished tail returned as `rest` for the next read.
   */
  function events(buffer) {
    var out = [];
    var text = String(buffer).replace(/\r\n?/g, "\n");
    var at;
    while ((at = text.indexOf("\n\n")) >= 0) {
      var block = text.slice(0, at);
      text = text.slice(at + 2);
      var name = "";
      var data = [];
      block.split("\n").forEach(function (line) {
        if (line.indexOf("event:") === 0) name = line.slice(6).trim();
        else if (line.indexOf("data:") === 0) data.push(line.slice(5).replace(/^ /, ""));
      });
      if (!name) continue;
      try {
        out.push({ name: name, data: JSON.parse(data.join("\n")) });
      } catch (e) {
        // A malformed event is skipped and the stream goes on.
      }
    }
    return { events: out, rest: text };
  }

  /**
   * The language the chat speaks (T-4138, T-4280): the one the visitor chose in the widget before
   * (`chosen`), else the one the embedding site names on purpose (`requested`, its `data-lang`),
   * when the widget has words for it; else Slovak when the deployment offers it or names none,
   * else the deployment's first language the widget knows, else English. The host page's own
   * language is never asked: an English catalogue page still opens a Slovak chat.
   */
  function language(requested, offered, known, chosen) {
    var has = function (l) { return Object.prototype.hasOwnProperty.call(known, l); };
    var pick = function (l) { return String(l || "").slice(0, 2).toLowerCase(); };
    if (has(pick(chosen))) return pick(chosen);
    var asked = pick(requested);
    if (asked && has(asked)) return asked;
    offered = (offered || []).filter(has);
    // The deployment's first language is its default (MF-52, T-4281); Slovak when it lists none.
    if (offered.length > 0) return offered[0];
    return has("sk") ? "sk" : "en";
  }

  /**
   * A `data` event as a table to show (T-4138, T-4140): its columns and at most 50 rows of
   * text, and its dataset and download links when they are http(s); null when nothing to show.
   */
  function table(data) {
    if (!data || !Array.isArray(data.columns) || data.columns.length === 0) return null;
    var cell = function (v) {
      if (v === null || v === undefined) return "";
      return typeof v === "object" ? JSON.stringify(v) : String(v);
    };
    var width = data.columns.length;
    var rows = (Array.isArray(data.rows) ? data.rows : []).filter(Array.isArray).slice(0, 50)
      .map(function (row) {
        var out = row.slice(0, width).map(cell);
        while (out.length < width) out.push("");
        return out;
      });
    return {
      caption: typeof data.type === "string" ? data.type : "",
      columns: data.columns.map(cell),
      rows: rows,
      dataset: safeUrl(data.dataset),
      download: safeUrl(data.download)
    };
  }

  /**
   * The greeting the empty chat shows (T-4199): the deployment's own, which its author writes in
   * its first language, when the chat speaks that language or the deployment names none; else the
   * widget's greeting in the visitor's language.
   */
  function greeting(own, languages, lang, fallback) {
    if (!own) return "";
    return !Array.isArray(languages) || languages.length === 0 || languages[0] === lang ? own : fallback;
  }

  var TIME = /^\d{4}-\d{2}-\d{2}(?:[T ][\d:.]+(?:Z|[+-]\d{2}:?\d{2})?)?$/;

  /**
   * A `data` event as a small chart (T-4198): when exactly one column holds numbers and another
   * names them, its points: a line over time when every name is a date, else bars, at most 50,
   * a value that is no finite number skipped. Null in any other case, and under two points.
   */
  function chart(data) {
    if (!data || !Array.isArray(data.columns) || !Array.isArray(data.rows)) return null;
    var rows = data.rows.filter(Array.isArray);
    var finite = function (v) { return typeof v === "number" && isFinite(v); };
    var numeric = data.columns.map(function (_, i) {
      var numbers = 0, words = 0;
      rows.forEach(function (row) {
        if (finite(row[i])) numbers += 1;
        else if (typeof row[i] === "string" && row[i].trim()) words += 1;
      });
      return numbers > 0 && numbers > words;
    });
    var values = numeric.reduce(function (found, yes, i) { return yes ? found.concat([i]) : found; }, []);
    if (values.length !== 1) return null;
    var named = data.columns.map(function (_, i) { return i; }).filter(function (i) { return !numeric[i]; });
    var isTime = function (i) {
      return rows.every(function (row) { return typeof row[i] === "string" && TIME.test(row[i]) && isFinite(Date.parse(row[i])); });
    };
    var time = named.filter(isTime)[0];
    var by = time !== undefined ? time : named[0];
    if (by === undefined) return null;
    var v = values[0];
    var points = rows.filter(function (row) { return finite(row[v]); }).slice(0, 50).map(function (row) {
      var name = row[by] === null || row[by] === undefined ? "" : String(row[by]);
      return { label: name.slice(0, 40), value: row[v] };
    });
    if (time !== undefined) points.sort(function (a, b) { return Date.parse(a.label) - Date.parse(b.label); });
    if (points.length < 2) return null;
    return { kind: time !== undefined ? "line" : "bar", value: String(data.columns[v]), by: String(data.columns[by]), points: points };
  }

  var EMAIL = /^[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}$/;

  /** A phone as a `tel:` link: 9 to 15 digits, a leading `+` and spaces, `/`, `-` aside; else null. */
  function tel(phone) {
    var compact = String(phone || "").replace(/[\s\/-]/g, "");
    return /^\+?\d{9,15}$/.test(compact) ? "tel:" + compact : null;
  }

  /** An e-mail address as a `mailto:` link, null unless it is one address and nothing else. */
  function mailto(address) {
    return EMAIL.test(String(address || "")) ? "mailto:" + address : null;
  }

  function text(value, max) {
    return typeof value === "string" ? value.replace(/\s+/g, " ").trim().slice(0, max) : "";
  }

  /**
   * A citation's cards (T-4155) as the chat shows them: each field text, each link checked
   * (`tel:`, `mailto:`, an OpenStreetMap search), an empty field left out and an empty card dropped.
   */
  function cards(raw) {
    return (Array.isArray(raw) ? raw : []).map(function (card) {
      if (!card || typeof card !== "object") return null;
      var list = function (values, link) {
        return (Array.isArray(values) ? values : []).map(function (v) { return text(v, 300); })
          .filter(Boolean).slice(0, 5).map(function (v) { return { text: v, href: link(v) }; });
      };
      if (card.kind === "contact") {
        var phones = list(card.phones, tel);
        var emails = list(card.emails, mailto);
        if (phones.length + emails.length === 0) return null;
        return { kind: "contact", name: text(card.name, 300), phones: phones, emails: emails };
      }
      if (card.kind === "place") {
        var address = text(card.address, 300);
        return address ? { kind: "place", address: address,
          href: "https://www.openstreetmap.org/search?query=" + encodeURIComponent(address) } : null;
      }
      if (card.kind === "procedure") {
        var steps = (Array.isArray(card.steps) ? card.steps : []).map(function (v) { return text(v, 300); })
          .filter(Boolean).slice(0, 10);
        return steps.length > 0 ? { kind: "procedure", steps: steps } : null;
      }
      return null;
    }).filter(Boolean);
  }

  var STAGES = {
    sk: { understanding: "Rozmýšľam nad otázkou…", searching: "Hľadám v zdrojoch", reading: "Čítam živé údaje",
      answering: "Pripravujem odpoveď…" },
    en: { understanding: "Reading your question…", searching: "Searching the sources", reading: "Reading live data",
      answering: "Preparing the answer…" },
    cs: { understanding: "Přemýšlím nad otázkou…", searching: "Hledám ve zdrojích", reading: "Čtu živá data",
      answering: "Připravuji odpověď…" }
  };

  /**
   * The progress line of a `stage` event (T-4155): fixed words per step, a search or a read
   * naming its topic (`Hľadám v zdrojoch: „…“…`); null for a step the chat does not know.
   */
  function stage(data, lang) {
    var words = STAGES[lang] || STAGES.en;
    var name = data && data.stage;
    if (!Object.prototype.hasOwnProperty.call(words, name)) return null;
    var topic = text(data.topic, 60);
    if (name === "searching" || name === "reading") {
      return words[name] + (topic ? ": „" + topic + "“…" : "…");
    }
    return words[name];
  }


  /** A `followups` event's questions (T-4174): strings, trimmed, 1 to 120 characters, at most 3. */
  function followups(data) {
    var out = [];
    ((data && Array.isArray(data.questions)) ? data.questions : []).forEach(function (q) {
      if (typeof q !== "string") return;
      q = q.replace(/\s+/g, " ").trim();
      if (q && q.length <= 120 && out.indexOf(q) < 0 && out.length < 3) out.push(q);
    });
    return out;
  }

  // What the panel keeps in its sessionStorage (T-4174): the conversation's id, how many questions
  // it asked, and the visible turns, at most MAX_KEPT, each question and answer with its citations.
  var MAX_KEPT = 20;
  var UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

  /** The kept conversation read back from `raw`, or null when it is missing or not of its shape. */
  function restore(raw) {
    var kept;
    try { kept = JSON.parse(raw); } catch (e) { return null; }
    if (!kept || typeof kept !== "object" || typeof kept.conversation !== "string" ||
        !UUID.test(kept.conversation) || !Array.isArray(kept.turns)) return null;
    var turns = kept.turns.filter(function (t) {
      return t && typeof t.question === "string" && typeof t.text === "string" &&
        Number.isInteger(t.turn) && t.turn >= 1 && Array.isArray(t.citations);
    }).slice(-MAX_KEPT).map(function (t) {
      return { question: t.question.slice(0, 4000), text: t.text.slice(0, 4000), turn: t.turn, citations: t.citations };
    });
    var asked = Number.isInteger(kept.asked) && kept.asked >= 0 ? kept.asked : 0;
    turns.forEach(function (t) { if (t.turn > asked) asked = t.turn; });
    return { conversation: kept.conversation, asked: asked, turns: turns };
  }

  /** `kept` with one more answered turn, the oldest dropped past MAX_KEPT. */
  function remember(kept, conversation, asked, entry) {
    var turns = (kept && kept.conversation === conversation ? kept.turns : []).concat([entry]);
    return { conversation: conversation, asked: asked, turns: turns.slice(-MAX_KEPT) };
  }

  /**
   * The questions an empty chat offers (T-4253): the deployment's own in the language the chat
   * speaks, else the widget's own, never another language's; at most four distinct ones.
   */
  function suggestions(own, lang, fallback) {
    var list = own && Array.isArray(own[lang]) && own[lang].length > 0 ? own[lang] : fallback || [];
    var seen = [];
    list.forEach(function (q) {
      var text = typeof q === "string" ? q.trim() : "";
      if (text && seen.indexOf(text) < 0 && seen.length < 4) seen.push(text);
    });
    return seen;
  }

  // The service's error titles (API/05 §1.6), English and stable, to the widget's own words.
  var TROUBLE = {
    "Budget Spent": "spent", "Conversation Budget Spent": "full", "Model Unavailable": "unreachable",
    "No Answer": "unanswered", "Answer Took Too Long": "slow"
  };

  /**
   * The widget's own word for a failed answer (T-4253), so the visitor reads it in the chat's
   * language and never an English detail: by the service's title, else by the status, a lost
   * connection (no status) as offline.
   */
  function trouble(status, title) {
    if (Object.prototype.hasOwnProperty.call(TROUBLE, title)) return TROUBLE[title];
    if (status === 429) return "busy";
    if (status === 503) return "paused";
    if (!status) return "offline";
    return "failed";
  }

  /** Whether asking the same again can help: not once the day's or the conversation's share is spent. */
  function retryable(word) {
    return word !== "spent" && word !== "full";
  }

  /**
   * What Retry sends again (T-4253): the last question and the history without it, which the
   * failed request already carried; null when the history does not end on a question.
   */
  function retry(history) {
    var last = Array.isArray(history) ? history[history.length - 1] : null;
    if (!last || last.role !== "user" || typeof last.text !== "string" || !last.text.trim()) return null;
    return { question: last.text, history: history.slice(0, -1) };
  }

  /** The header's monogram (T-4283): the first letters of the first two words, upper case. */
  function monogram(title) {
    var words = String(title || "").trim().split(/[\s\-–]+/).filter(function (word) { return /^\p{L}/u.test(word); });
    return words.slice(0, 2).map(function (word) { return word.charAt(0).toLocaleUpperCase(); }).join("");
  }

  /** The language this browser chose before, when the deployment still offers it; else null. */
  function chosen(stored, offered) {
    return typeof stored === "string" && (offered || []).indexOf(stored) >= 0 ? stored : null;
  }

  /**
   * Whether the panel asks for the language first (T-4283 item 13): on a first open where there
   * is a choice to make, never on the full page that names `?lang=`, never over a conversation
   * already open or a paused deployment.
   */
  function asksLanguage(at) {
    if (!at || at.stored || at.kept || at.enabled === false) return false;
    if ((at.offered || []).length < 2) return false;
    return !(at.page && at.requested);
  }

  /** Slovak first, then the deployment's order: the chooser's and the toggle's buttons. */
  function ordered(offered) {
    var list = (offered || []).slice();
    var sk = list.indexOf("sk");
    if (sk > 0) list.unshift(list.splice(sk, 1)[0]);
    return list;
  }

  return {
    monogram: monogram, chosen: chosen, asksLanguage: asksLanguage, ordered: ordered,
    followups: followups, restore: restore, remember: remember,
    cards: cards, stage: stage, tel: tel, mailto: mailto,
    markdown: markdown, sources: sources, label: label, linkHost: linkHost,
    safeUrl: safeUrl, events: events, language: language, table: table, chart: chart, greeting: greeting,
    suggestions: suggestions, trouble: trouble, retryable: retryable, retry: retry
  };
})();
if (typeof module !== "undefined") module.exports = JCRender;
