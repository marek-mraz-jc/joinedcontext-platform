// What the widget shows of an answer (T-3325): a small, safe part of Markdown read into a plain
// tree, and the answer's citations grouped into its sources. Nothing here touches the page: the
// widget builds DOM nodes from the tree with createElement and textContent alone, so no HTML the
// model writes is ever parsed. Loaded before widget.js; `node --test` reads it as a module.
"use strict";
var JCRender = (function () {
  // Bold, italic, inline code, an http(s) link, and a citation marker `[1]` or `[1, 2]`.
  var INLINE = /\*\*(.+?)\*\*|__(.+?)__|`([^`]+)`|\[([^\]\n]+)\]\((https?:\/\/[^\s()]+)\)|\[(\d+(?:\s*,\s*\d+)*)\]|\*(?=\S)(.+?)\*|(^|[^\w])_(?=\S)(.+?)_(?!\w)/g;

  function inline(text, known) {
    var out = [];
    var last = 0;
    var match;
    var re = new RegExp(INLINE.source, "g");
    while ((match = re.exec(text)) !== null) {
      // `_em_` takes the character before it, so a snake_case name stays one word.
      out.push(text.slice(last, match.index) + (match[8] || ""));
      if (match[1] !== undefined || match[2] !== undefined) {
        out.push({ tag: "strong", children: inline(match[1] !== undefined ? match[1] : match[2], known) });
      } else if (match[3] !== undefined) {
        out.push({ tag: "code", children: [match[3]] });
      } else if (match[4] !== undefined) {
        out.push({ tag: "a", href: match[5], children: inline(match[4], known) });
      } else if (match[6] !== undefined) {
        var numbers = match[6].split(",").map(function (n) { return Number(n.trim()); })
          .filter(function (n) { return known(n); });
        // A marker no citation stands behind is dropped, never shown as typed.
        if (numbers.length > 0) out.push({ tag: "cite", numbers: numbers });
      } else {
        out.push({ tag: "em", children: inline(match[7] !== undefined ? match[7] : match[9], known) });
      }
      last = re.lastIndex;
    }
    out.push(text.slice(last));
    return out.filter(function (node) { return node !== ""; });
  }

  var BULLET = /^\s*[-*+]\s+(.*)$/;
  var NUMBERED = /^\s*\d{1,3}[.)]\s+(.*)$/;
  var HEADING = /^\s{0,3}#{1,6}\s+(.*?)\s*#*\s*$/;

  /**
   * The answer as blocks: paragraphs (lines joined by line breaks), bullet and numbered lists,
   * headings read as bold paragraphs. `known(n)` says whether citation n exists.
   */
  function markdown(text, known) {
    known = known || function () { return false; };
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
        list.children.push({ tag: "li", children: inline((item || number)[1], known) });
        paragraph = null;
      } else if (line.trim() === "") {
        paragraph = null;
        list = null;
      } else if (heading) {
        blocks.push({ tag: "p", children: [{ tag: "strong", children: inline(heading[1], known) }] });
        paragraph = null;
        list = null;
      } else if (list && /^\s{2,}\S/.test(line)) {
        // An indented line goes on with the list item above it.
        var last = list.children[list.children.length - 1];
        last.children = last.children.concat([{ tag: "br" }], inline(line.trim(), known));
      } else {
        list = null;
        if (!paragraph) {
          paragraph = { tag: "p", children: [] };
          blocks.push(paragraph);
        } else {
          paragraph.children.push({ tag: "br" });
        }
        paragraph.children = paragraph.children.concat(inline(line.trim(), known));
      }
    });
    return blocks;
  }

  function domain(url) {
    var match = /^https?:\/\/([^/?#:]+)/i.exec(url);
    return match ? match[1].replace(/^www\./, "") : "";
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
          numbers: []
        });
      }
      var entry = list[byKey[key]];
      if (!entry.title && typeof c.title === "string" && c.title.trim()) entry.title = c.title.trim();
      entry.numbers.push(c.n);
      position[c.n] = byKey[key] + 1;
    });
    return { list: list, position: position };
  }

  /** What a source's link says: its title, else the address without its scheme. */
  function label(source) {
    if (source.title) return source.title;
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
   * The language the chat speaks (T-4138): the one asked for (the host page's, or the visitor's
   * switch) when the widget has words for it, else Slovak when the deployment offers it or names
   * none, else the deployment's first language the widget knows, else English.
   */
  function language(requested, offered, known) {
    var has = function (l) { return Object.prototype.hasOwnProperty.call(known, l); };
    var asked = String(requested || "").slice(0, 2).toLowerCase();
    if (asked && has(asked)) return asked;
    offered = (offered || []).filter(has);
    if (offered.length === 0 || offered.indexOf("sk") >= 0) return has("sk") ? "sk" : "en";
    return offered[0];
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

  return {
    markdown: markdown, sources: sources, label: label,
    safeUrl: safeUrl, events: events, language: language, table: table
  };
})();
if (typeof module !== "undefined") module.exports = JCRender;
