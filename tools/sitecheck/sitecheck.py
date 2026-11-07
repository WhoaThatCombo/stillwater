# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""sitecheck: sweep websites through Stillwater/Servo and explain why they don't work.

Each site is loaded headlessly in its own isolated Stillwater (own log, cache and profile).
A probe script injected into the page reports what it really shows; Stillwater's logs give
network failures, script errors and crashes. Every site gets a status and a diagnosis, and
the HTML report ranks the missing web features that break the most sites.

Examples (from the repo root, after `./mach build --release`):
    python tools/sitecheck/sitecheck.py                      # sweep tools/sitecheck/sites.json
    python tools/sitecheck/sitecheck.py --site https://example.com/
    python tools/sitecheck/sitecheck.py --category video --jobs 6
    python tools/sitecheck/sitecheck.py --label after --compare before
    python tools/sitecheck/sitecheck.py -- --pref=dom_webgl2_enabled   # extra Stillwater args
"""

from __future__ import annotations

import argparse
import concurrent.futures
import html
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path
from urllib.parse import urlparse

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
DEFAULT_EXE = REPO / "target" / "release" / ("servoshell.exe" if os.name == "nt" else "servoshell")

# Statuses, worst first.
CRASH, NETWORK, BLOCKED, BLANK, BROKEN, LAYOUT, PARTIAL, SLOW, OK = (
    "crash", "network", "blocked", "blank", "broken", "layout", "partial", "slow", "ok")
STATUS_ORDER = [CRASH, NETWORK, BLOCKED, BLANK, BROKEN, LAYOUT, PARTIAL, SLOW, OK]
STATUS_LABEL = {
    CRASH: "Crashed", NETWORK: "Network failure", BLOCKED: "Blocked by a browser check",
    BLANK: "Blank page", BROKEN: "Broken (script errors)", LAYOUT: "Layout broken",
    PARTIAL: "Partly works",
    SLOW: "Works, slowly", OK: "Works",
}

SCRIPT_ERROR = re.compile(r"Error at (\S+?):(\d+):(\d+) (.*)$")
API_PATTERNS = [
    re.compile(r"([\w$.]+) is not a function"),
    re.compile(r"([\w$.]+) is not a constructor"),
    re.compile(r"([\w$]+) is not defined"),
    re.compile(r"([\w$.]+) is undefined"),
    re.compile(r"can't access property \"?([\w$]+)\"?"),
    re.compile(r"undefined has no properties"),
]
# Stillwater logs each CSS declaration, rule or media query it rejects (see RUST_LOG below), like
# "  12:40 Unsupported property declaration: 'text-wrap:pretty', OtherInvalidValue".
CSS_ERROR = re.compile(r"^\s*\d+:\d+ ([A-Z][^:']+?): '?(.*?)'?, ([A-Za-z]+(?:\(.*\))?)\s*$")
# Pseudo-classes and elements Stillwater supports; the rest of an invalid selector is the culprit.
KNOWN_PSEUDOS = {
    "not", "is", "where", "hover", "focus", "active", "visited", "link", "any-link", "checked",
    "disabled", "enabled", "first-child", "last-child", "only-child", "nth-child", "nth-last-child",
    "nth-of-type", "nth-last-of-type", "first-of-type", "last-of-type", "only-of-type", "root",
    "empty", "before", "after", "placeholder", "selection", "focus-within", "focus-visible", "target",
    "required", "optional", "valid", "invalid", "read-only", "read-write", "indeterminate", "lang",
    "first-line", "first-letter", "marker", "default", "defined", "host", "slotted", "part",
    "placeholder-shown", "autofill", "fullscreen", "scope", "in-range", "out-of-range", "dir",
}
NOISE_API = {"e", "t", "n", "r", "i", "o", "a", "s", "this", "undefined", "null", "window", "document"}


@dataclass
class Result:
    site: str
    category: str
    status: str = OK
    diagnosis: str = ""
    head_parsed: float | None = None
    complete: float | None = None
    title: str = ""
    text_length: int = 0
    elements: int = 0
    blocked_phrases: list[str] = field(default_factory=list)
    network_errors: list[str] = field(default_factory=list)
    script_errors: list[str] = field(default_factory=list)
    missing_apis: list[str] = field(default_factory=list)
    absent_globals: list[str] = field(default_factory=list)
    panics: list[str] = field(default_factory=list)
    unsupported_css: list[str] = field(default_factory=list)
    oversized: list[str] = field(default_factory=list)
    screenshot: str = ""
    blank_screenshot: bool = False
    seconds: float = 0.0


def slug(url: str) -> str:
    parsed = urlparse(url)
    name = (parsed.netloc + parsed.path).rstrip("/").removeprefix("www.")
    return re.sub(r"[^A-Za-z0-9.]+", "_", name)[:60] or "site"


def api_name(message: str) -> str | None:
    """Guess which web API a script error is about ("x.adoptedStyleSheets is undefined")."""
    for pattern in API_PATTERNS:
        match = pattern.search(message)
        if match and match.groups():
            name = match.group(1).split(".")[-1].strip("$\"'")
            if len(name) > 2 and name not in NOISE_API:
                return name
    return None


def css_key(kind: str, body: str, reason: str) -> str | None:
    """Group a rejected-CSS log line under the feature it's about, or None for vendor noise."""
    body = body.strip().rstrip(";")
    lower = body.lower()
    if any(junk in lower for junk in ("-moz-", "-ms-", "-o-", "progid:", "expression(")) or body[:1] in "*_":
        return None
    if kind == "Unsupported property declaration":
        prop, _, value = body.partition(":")
        prop = prop.strip().lower()
        if reason == "UnknownProperty" or not value:
            return f"property {prop}"
        value = re.sub(r"\s*!important", "", value.strip().lower())
        value = re.sub(r"(?<![\w-])-?\d*\.?\d+", "0", value)
        return f"{prop}: {value[:48]}"
    if kind == "Invalid media rule":
        features = re.findall(r"\(\s*([a-z-]+)\s*[:)<>=]", lower)
        return "@media (" + ", ".join(dict.fromkeys(features)) + ")" if features else "@media " + lower[:40]
    if body.startswith("@"):
        return body.split()[0].split("(")[0].lower()
    if kind == "Invalid rule":
        pseudos = [name for name in re.findall(r"::?([a-z][a-z-]*)", lower)
                   if name not in KNOWN_PSEUDOS and not name.startswith("-")]
        return "selector :" + pseudos[0] if pseudos else None
    if "descriptor" in kind:
        return kind.split()[1] + " " + body.split(":")[0].strip().lower()
    return None


def screenshot_is_blank(path: Path) -> bool:
    try:
        from PIL import Image, ImageStat
    except ImportError:
        return False
    try:
        with Image.open(path) as image:
            stat = ImageStat.Stat(image.convert("L").resize((160, 100)))
            return stat.stddev[0] < 4.0
    except OSError:
        return True


def check_site(url: str, category: str, args, run_dir: Path) -> Result:
    result = Result(site=url, category=category)
    name = slug(url)
    site_dir = run_dir / "sites" / name
    site_dir.mkdir(parents=True, exist_ok=True)
    png = site_dir / "screenshot.png"
    log = site_dir / "stillwater.log"
    profile = Path(tempfile.mkdtemp(prefix="sitecheck-"))
    env = dict(os.environ)
    env.update({
        "STILLWATER_LOG_FILE": str(log),
        "STILLWATER_CONFIG_DIR": str(profile),
        "STILLWATER_NO_UPDATE_CHECK": "1",
        # script::dom::window logs every CSS declaration/rule Stillwater rejects.
        "RUST_LOG": "warn,script::dom::window=info",
        # Screenshot a while after load: many pages fade their content in.
        "STILLWATER_SCREENSHOT_DELAY_MS": str(int(args.shot_delay * 1000)),
    })
    command = [
        str(args.exe), "--headless", "--window-size=1280x800", "-o", str(png),
        f"--userscripts={HERE / 'probe'}",
        f"--pref=network_http_disk_cache={profile / 'cache'}",
        *args.extra, url,
    ]
    started = time.monotonic()
    with open(site_dir / "output.txt", "wb") as output:
        process = subprocess.Popen(command, env=env, stdout=output, stderr=subprocess.STDOUT)
        try:
            process.wait(timeout=args.seconds)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
    result.seconds = round(time.monotonic() - started, 1)
    shutil.rmtree(profile, ignore_errors=True)

    text = (site_dir / "output.txt").read_text("utf-8", errors="replace").splitlines()
    diag = log.read_text("utf-8", errors="replace").splitlines() if log.exists() else []

    # Page load timing from Stillwater's diagnostic log.
    for line in diag:
        parts = line.split()
        if "page load HeadParsed" in line and result.head_parsed is None:
            result.head_parsed = float(parts[0])
        if "page load Complete" in line and result.complete is None:
            result.complete = float(parts[0])

    # What the probe saw (the last report wins: "settled" after "load").
    probe = {}
    for line in text:
        index = line.find("__sitecheck__")
        if index != -1:
            try:
                probe = json.loads(line[index + len("__sitecheck__"):])
            except json.JSONDecodeError:
                pass
    result.title = probe.get("title", "")
    result.text_length = int(probe.get("textLength", 0) or 0)
    result.elements = int(probe.get("elements", 0) or 0)
    result.blocked_phrases = probe.get("blocked", []) or []
    result.absent_globals = probe.get("missing", []) or []
    result.oversized = probe.get("oversized", []) or []

    # Errors from Stillwater's output.
    errors = []
    for line in text:
        if "network error" in line:
            kind = re.search(r"(dns error|Http2[^)]*\)|tls|certificate|Connect|SendRequest|timed out)", line, re.I)
            result.network_errors.append(kind.group(1) if kind else line[-160:])
        if "panicked" in line:
            result.panics.append(line[-200:])
        elif "servoshell::panic_hook" in line and "Panic hook called" not in line:
            # Stillwater's panic hook logs the message instead of Rust's "panicked at".
            result.panics.append(line[-200:])
        match = SCRIPT_ERROR.search(line)
        if match:
            errors.append(match.group(4).strip())
        css = CSS_ERROR.match(line)
        if css:
            key = css_key(*css.groups())
            if key and key not in result.unsupported_css and len(result.unsupported_css) < 80:
                result.unsupported_css.append(key)
    errors += probe.get("errors", []) or []
    seen = set()
    for message in errors:
        key = message[:160]
        if key not in seen:
            seen.add(key)
            result.script_errors.append(key)
        api = api_name(message)
        if api and api not in result.missing_apis:
            result.missing_apis.append(api)

    if png.exists():
        result.screenshot = str(png.relative_to(run_dir)).replace("\\", "/")
        result.blank_screenshot = screenshot_is_blank(png)

    classify(result, probe, text)
    return result


def classify(result: Result, probe: dict, text: list[str]) -> None:
    error_page = any("Could not load the requested page" in line for line in text) or \
        "Could not load" in (result.title or "")
    if result.panics:
        result.status, result.diagnosis = CRASH, "Stillwater crashed: " + result.panics[0]
    elif error_page or (not probe and result.network_errors and result.complete is None):
        kind = result.network_errors[0] if result.network_errors else "unknown"
        result.status = NETWORK
        result.diagnosis = f"The page itself failed to download ({kind})."
    elif result.blocked_phrases and result.text_length < 1500:
        result.status = BLOCKED
        result.diagnosis = ("The site showed a browser/bot check (\"" + result.blocked_phrases[0] +
                            "\"). Usually user-agent or fingerprint based.")
    elif not probe or (result.text_length < 40 and result.blank_screenshot):
        result.status = BLANK
        hint = f" Likely missing: {', '.join(result.missing_apis[:4])}." if result.missing_apis else ""
        result.diagnosis = "Nothing visible rendered." + hint
    elif result.text_length < 200 and result.script_errors and (result.blank_screenshot or result.elements < 300):
        result.status = BROKEN
        result.diagnosis = "Very little content and script errors: " + "; ".join(result.script_errors[:2])
    elif result.oversized:
        result.status = LAYOUT
        result.diagnosis = ("Parts of the page are far wider than the window (" +
                            ", ".join(result.oversized[:2]) + "), usually from unsupported CSS.")
    elif result.missing_apis or len(result.script_errors) >= 10:
        result.status = PARTIAL
        result.diagnosis = "Renders, but scripts fail" + (
            f" (missing: {', '.join(result.missing_apis[:4])})." if result.missing_apis else ".")
    elif result.complete is not None and result.complete > 10:
        result.status = SLOW
        result.diagnosis = f"Took {result.complete:.1f}s to finish loading."
    else:
        result.status, result.diagnosis = OK, "Loaded and rendered."


def load_sites(args) -> list[tuple[str, str]]:
    if args.site:
        return [(url, "requested") for url in args.site]
    catalog = json.loads(Path(args.sites).read_text("utf-8"))
    sites = []
    for category, urls in catalog.items():
        if args.category and category not in args.category:
            continue
        sites += [(url, category) for url in urls]
    return sites


def write_report(run_dir: Path, results: list[Result], label: str, previous: dict | None) -> Path:
    counts = {status: sum(1 for r in results if r.status == status) for status in STATUS_ORDER}
    api_sites: dict[str, list[str]] = {}
    for r in results:
        for api in r.missing_apis:
            api_sites.setdefault(api, []).append(slug(r.site))
    ranked = sorted(api_sites.items(), key=lambda item: -len(item[1]))
    css_sites: dict[str, list[str]] = {}
    for r in results:
        for key in r.unsupported_css:
            css_sites.setdefault(key, []).append(slug(r.site))
    css_ranked = sorted(css_sites.items(), key=lambda item: -len(item[1]))

    def esc(value) -> str:
        return html.escape(str(value))

    changes = []
    if previous:
        for r in results:
            before = previous.get(r.site)
            if before and before != r.status:
                better = STATUS_ORDER.index(r.status) > STATUS_ORDER.index(before)
                changes.append((r.site, before, r.status, better))

    cards = []
    for r in sorted(results, key=lambda r: (STATUS_ORDER.index(r.status), r.site)):
        timing = f"loaded in {r.complete:.1f}s" if r.complete is not None else "never finished loading"
        details = "".join(f"<li>{esc(e)}</li>" for e in r.script_errors[:8])
        net = "".join(f"<li>{esc(e)}</li>" for e in r.network_errors[:5])
        image = f'<a href="{esc(r.screenshot)}"><img src="{esc(r.screenshot)}" loading="lazy" alt=""></a>' if r.screenshot else '<div class="noshot">No screenshot</div>'
        cards.append(f"""
<article class="card {r.status}">
  {image}
  <div class="body">
    <h3><a href="{esc(r.site)}">{esc(slug(r.site))}</a> <span class="badge">{esc(STATUS_LABEL[r.status])}</span></h3>
    <p class="diag">{esc(r.diagnosis)}</p>
    <p class="meta">{esc(r.category)} · {timing} · {r.text_length} chars of text · {len(r.script_errors)} script errors</p>
    {f'<details><summary>Script errors</summary><ul>{details}</ul></details>' if details else ''}
    {f'<details><summary>Network errors</summary><ul>{net}</ul></details>' if net else ''}
  </div>
</article>""")

    rows = "".join(f"<tr><td>{esc(api)}</td><td>{len(sites)}</td><td>{esc(', '.join(sites[:8]))}</td></tr>" for api, sites in ranked[:25])
    css_rows = "".join(f"<tr><td><code>{esc(key)}</code></td><td>{len(sites)}</td><td>{esc(', '.join(sites[:8]))}</td></tr>" for key, sites in css_ranked[:60])
    summary = "".join(f'<li class="{s}"><b>{counts[s]}</b> {esc(STATUS_LABEL[s])}</li>' for s in STATUS_ORDER if counts[s])
    change_rows = "".join(
        f"<li class=\"{'better' if better else 'worse'}\">{esc(slug(site))}: {esc(STATUS_LABEL[a])} → {esc(STATUS_LABEL[b])}</li>"
        for site, a, b, better in changes)

    page = f"""<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>sitecheck · {esc(label)}</title>
<style>
:root {{ color-scheme: light dark; --bg:#f4f6f6; --fg:#1d2a30; --card:#fff; --muted:#5d7276; --edge:#0001; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg:#13202a; --fg:#dce6e4; --card:#1c2f3b; --muted:#8ea5a7; --edge:#fff2; }} }}
body {{ margin:0; padding:24px; background:var(--bg); color:var(--fg); font:15px/1.5 "Segoe UI",system-ui,sans-serif; }}
h1 {{ margin:0 0 4px; font-weight:600; }} .muted {{ color:var(--muted); }}
.summary {{ display:flex; flex-wrap:wrap; gap:8px; padding:0; list-style:none; }}
.summary li {{ padding:6px 12px; border-radius:999px; background:var(--card); border:1px solid var(--edge); }}
.grid {{ display:grid; grid-template-columns:repeat(auto-fill,minmax(300px,1fr)); gap:14px; margin-top:16px; }}
.card {{ background:var(--card); border-radius:12px; overflow:hidden; border:1px solid var(--edge); border-top:4px solid var(--c); }}
.card img, .noshot {{ display:block; width:100%; aspect-ratio:16/10; object-fit:cover; object-position:top; background:#0002; }}
.noshot {{ display:grid; place-items:center; color:var(--muted); }}
.body {{ padding:10px 14px 14px; }} .body h3 {{ margin:0; font-size:16px; }} .body a {{ color:inherit; }}
.badge {{ font-size:12px; font-weight:600; padding:2px 8px; border-radius:999px; background:var(--c); color:#fff; margin-left:6px; }}
.diag {{ margin:6px 0 2px; }} .meta {{ margin:0; font-size:13px; color:var(--muted); }}
details {{ font-size:13px; margin-top:6px; }} details ul {{ margin:4px 0; padding-left:18px; overflow-wrap:anywhere; }}
.crash{{--c:#8e2bd4}} .network{{--c:#c43b2c}} .blocked{{--c:#d8731b}} .blank{{--c:#b8414f}} .broken{{--c:#c95a3a}}
.layout{{--c:#c77d2b}} .partial{{--c:#c9a227}} .slow{{--c:#5b8bd6}} .ok{{--c:#2f9a62}}
table {{ border-collapse:collapse; width:100%; background:var(--card); border-radius:10px; overflow:hidden; }}
td, th {{ text-align:left; padding:6px 10px; border-bottom:1px solid var(--edge); font-size:14px; vertical-align:top; }}
.better {{ color:#2f9a62; }} .worse {{ color:#c43b2c; }}
</style></head><body>
<h1>sitecheck: {esc(label)}</h1>
<p class="muted">{len(results)} sites · {time.strftime('%Y-%m-%d %H:%M')} · Stillwater on Servo</p>
<ul class="summary">{summary}</ul>
{f'<h2>Changes since previous run</h2><ul>{change_rows}</ul>' if change_rows else ''}
<h2>Most common missing web features</h2>
<p class="muted">Guessed from script errors. Fixing the top entries in Servo should help the most sites.</p>
{f'<table><tr><th>Feature</th><th>Sites</th><th>Where</th></tr>{rows}</table>' if rows else '<p>None detected.</p>'}
<h2>Most common unsupported CSS</h2>
<p class="muted">Declarations, selectors and media queries Stillwater rejected, by number of sites using them
(vendor-prefixed hacks left out). Rejected CSS is silently ignored, so popular entries here are likely
causes of pages that look wrong.</p>
{f'<table><tr><th>CSS</th><th>Sites</th><th>Where</th></tr>{css_rows}</table>' if css_rows else '<p>None detected.</p>'}
<h2>Sites</h2>
<div class="grid">{''.join(cards)}</div>
</body></html>"""
    report = run_dir / "report.html"
    report.write_text(page, "utf-8")
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--site", action="append", help="check this URL (repeatable) instead of the catalog")
    parser.add_argument("--sites", default=str(HERE / "sites.json"), help="site catalog (JSON: category -> URLs)")
    parser.add_argument("--category", action="append", help="only these catalog categories")
    parser.add_argument("--jobs", type=int, default=4, help="sites checked at once (default 4)")
    parser.add_argument("--seconds", type=int, default=25, help="time given to each site (default 25)")
    parser.add_argument("--shot-delay", type=float, default=6,
                        help="seconds after load to take the screenshot (default 6)")
    parser.add_argument("--label", default=time.strftime("run-%Y%m%d-%H%M%S"), help="name of this run")
    parser.add_argument("--compare", help="label of a previous run to diff against")
    parser.add_argument("--exe", type=Path, default=DEFAULT_EXE, help="servoshell executable")
    parser.add_argument("--out", type=Path, default=HERE / "out", help="output folder")
    parser.add_argument("extra", nargs="*", help="extra Stillwater arguments (after --)")
    args = parser.parse_args()

    if not args.exe.exists():
        print(f"Can't find {args.exe}; build Stillwater first (./mach build --release).", file=sys.stderr)
        return 2
    sites = load_sites(args)
    run_dir = args.out / args.label
    run_dir.mkdir(parents=True, exist_ok=True)
    print(f"Checking {len(sites)} sites, {args.jobs} at a time, {args.seconds}s each -> {run_dir}")

    results: list[Result] = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        futures = {pool.submit(check_site, url, category, args, run_dir): url for url, category in sites}
        for future in concurrent.futures.as_completed(futures):
            result = future.result()
            results.append(result)
            print(f"  {STATUS_LABEL[result.status]:<28} {slug(result.site):<40} {result.diagnosis[:90]}")

    (run_dir / "results.json").write_text(json.dumps([asdict(r) for r in results], indent=2), "utf-8")
    previous = None
    if args.compare:
        previous_file = args.out / args.compare / "results.json"
        if previous_file.exists():
            previous = {r["site"]: r["status"] for r in json.loads(previous_file.read_text("utf-8"))}
    report = write_report(run_dir, results, args.label, previous)
    working = sum(1 for r in results if r.status in (OK, SLOW, PARTIAL))
    css_count: dict[str, int] = {}
    for r in results:
        for key in r.unsupported_css:
            css_count[key] = css_count.get(key, 0) + 1
    if css_count:
        print("\nMost common unsupported CSS:")
        for key, count in sorted(css_count.items(), key=lambda item: -item[1])[:15]:
            print(f"  {count:4}  {key}")
    print(f"\n{working}/{len(results)} sites usable. Report: {report}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
