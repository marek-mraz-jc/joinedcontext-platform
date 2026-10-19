// Builds the standalone assistant (T-4694) into one directory any static host serves:
//   node crates/assistant/widget/app/build.mjs --api https://assistant.{domain} [--viz https://viz.{domain}] --out <dir>
// The host answers every `/{publicId}` with index.html and serves the rest as files. Node's
// standard library only; the bundle is the widget the chat's own host serves, byte for byte.
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { gzipSync } from "node:zlib";

const here = dirname(fileURLToPath(import.meta.url));
const widget = join(here, "..");
/** The bundle's budget, gzip, every file together. */
export const BUDGET = 200 * 1024;
const ORIGIN = /^https:\/\/[a-z0-9.-]+(:\d{1,5})?$/;

/** The files of the bundle, by name, for chat host `api` and chart renderer `viz` (or none). */
export function bundle(api, viz) {
  if (!ORIGIN.test(api || "")) throw new Error(`--api must be an https origin, got ${api}`);
  if (viz && !ORIGIN.test(viz)) throw new Error(`--viz must be an https origin, got ${viz}`);
  const read = (path) => readFileSync(join(widget, path), "utf8");
  const fields = readFileSync(join(widget, "../../jc-core/data/field-order.json"), "utf8");
  const files = {
    "index.html": read("app/index.html")
      .replaceAll("__API__", api)
      .replace("__FRAMES__", viz ? `frame-src ${viz}; ` : ""),
    "app.js": read("app/app.js"),
    // As the chat's host concatenates it (chat/widget.rs SCRIPT).
    "widget.js": `var JC_FIELDS = ${fields};\n${read("render.js")}${read("widget.js")}`,
    "widget.css": read("widget.css"),
    "visitor.js": read("visitor.js"),
    "embed.js": read("embed.js"),
  };
  const size = Object.values(files).reduce((sum, text) => sum + gzipSync(text).length, 0);
  if (size > BUDGET) throw new Error(`the bundle is ${size} bytes gzip, over its ${BUDGET}`);
  return { files, size };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const args = process.argv.slice(2);
  const arg = (name) => { const i = args.indexOf(name); return i >= 0 ? args[i + 1] : undefined; };
  const out = arg("--out");
  if (!out) { console.error("usage: build.mjs --api https://assistant.{domain} [--viz https://viz.{domain}] --out <dir>"); process.exit(2); }
  const { files, size } = bundle(arg("--api"), arg("--viz"));
  mkdirSync(out, { recursive: true });
  for (const [name, text] of Object.entries(files)) writeFileSync(join(out, name), text);
  console.log(`${Object.keys(files).length} files, ${size} bytes gzip, in ${out}`);
}
