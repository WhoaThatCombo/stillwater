// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Turn uBlock Origin's scriptlets (src/js/resources/scriptlets.js) and redirect resources
// (src/web_accessible_resources, indexed by src/js/redirect-resources.js) into the resource list
// adblock-rust's `Engine::use_resources` reads, so `##+js(...)` and `$redirect=` filters work.
//
// Usage: node build-scriptlets.mjs <uBlock checkout> <out.json>
//
// The output contains uBlock Origin's scriptlet code, which is GPLv3: it is published next to
// a notice pointing at the uBlock Origin commit it came from, never compiled into Stillwater.

import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const [uboDir, outPath] = process.argv.slice(2);
if (!uboDir || !outPath) {
  console.error("usage: node build-scriptlets.mjs <uBlock checkout> <out.json>");
  process.exit(2);
}

const moduleUrl = pathToFileURL(join(uboDir, "src", "js", "resources", "scriptlets.js"));
const { builtinScriptlets } = await import(moduleUrl.href);

// adblock-rust: a scriptlet that needs trust (uBO's `requiresTrust`) is only injected for
// filters parsed with this permission bit; Stillwater grants it to uBlock Origin's own lists.
const TRUSTED = 1;

const resources = builtinScriptlets
  // uBO's ISOLATED-world scriptlets need extension APIs a page script doesn't have.
  .filter((details) => details.world !== "ISOLATED")
  .map((details) => {
    const isDependency = details.name.endsWith(".fn");
    return {
      name: details.name,
      aliases: details.aliases || [],
      kind: { mime: isDependency ? "fn/javascript" : "application/javascript" },
      content: Buffer.from(details.fn.toString()).toString("base64"),
      dependencies: (details.dependencies || []).filter((name) => typeof name === "string"),
      ...(details.requiresTrust ? { permission: TRUSTED } : {}),
    };
  });

// Redirect resources: harmless stand-ins served instead of blocked ads (e.g. a no-op Google IMA
// SDK, so video players keep working). adblock-rust serves them as data: URLs.
const MIME = {
  gif: "image/gif", png: "image/png", js: "application/javascript", json: "application/json",
  css: "text/css", html: "text/html", txt: "text/plain", xml: "text/xml", mp3: "audio/mp3",
  mp4: "video/mp4",
};
const redirectUrl = pathToFileURL(join(uboDir, "src", "js", "redirect-resources.js"));
const { default: redirects } = await import(redirectUrl.href);
const names = new Set(resources.map((resource) => resource.name));
let redirectCount = 0;
for (const [name, details] of redirects) {
  if (names.has(name)) continue;
  const extension = name.slice(name.lastIndexOf(".") + 1);
  const mime = MIME[extension];
  if (!mime) continue;
  let bytes;
  try {
    bytes = readFileSync(join(uboDir, "src", "web_accessible_resources", name));
  } catch {
    continue;
  }
  const alias = details.alias === undefined ? [] : [].concat(details.alias);
  resources.push({ name, aliases: alias, kind: { mime }, content: bytes.toString("base64"), dependencies: [] });
  redirectCount++;
}

let commit = "unknown";
try {
  commit = execFileSync("git", ["-C", uboDir, "rev-parse", "HEAD"]).toString().trim();
} catch {}

writeFileSync(outPath, JSON.stringify(resources));
console.log(`${resources.length - redirectCount} scriptlets + ${redirectCount} redirect resources from uBlock Origin ${commit} -> ${outPath}`);
