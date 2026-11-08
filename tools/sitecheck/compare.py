# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""compare: how differently do sites look in Stillwater than in Microsoft Edge?

For each site, a hidden Edge loads the page, then Stillwater (headless), then Edge again. All
three are measured with the same script (ports/servoshell/desktop/compare-collect.js: every
visible element's box, text size, weight and colours) and screenshotted at 1280x800.
Whatever differs between the two Edge loads (ads, carousels, clocks, A/B tests) is noise and
ignored; what's left is how Stillwater differs. The report ranks sites by that difference and
shows each with a slider between the two browsers, a heat map, and the differences in words
("the nav bar is 12 px taller", "this heading's text is smaller", "this image is missing").

Run from the repo root, after `./mach build --release`:
    python tools/sitecheck/compare.py --label c1                  # every site in sites.json
    python tools/sitecheck/compare.py --category news --jobs 3
    python tools/sitecheck/compare.py --site https://example.com/

Results: tools/sitecheck/out/compare-<label>/report.html and results.json. Stillwater runs
headless and muted, Edge headless and muted, both on 127.0.0.1 only; nothing is shown on screen.
Ad blocking is off in Stillwater, since Edge has none.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import html
import json
import os
import re
import shutil
import sys
import tempfile
import threading
import time
from pathlib import Path

import numpy as np
from PIL import Image, ImageFilter

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from driver import Stillwater, WebDriverError  # noqa: E402
from edge import COLLECT_JS, Edge  # noqa: E402

WIDTH, HEIGHT = 1280, 800


def windows_color_scheme() -> str:
    """Windows' own theme. Both browsers use it: some sites (Google) choose light or dark on the
    server from a header Edge sends that follows Windows, whatever the page is told."""
    try:
        import winreg
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER,
                            r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize") as key:
            return "light" if winreg.QueryValueEx(key, "AppsUseLightTheme")[0] else "dark"
    except OSError:
        return "light"


SCHEME = windows_color_scheme()
# The measuring script as one expression (its leading comment lines removed), so WebDriver can
# `return` it.
COLLECT_EXPRESSION = "\n".join(line for line in COLLECT_JS.splitlines()
                               if not line.lstrip().startswith("//")).strip()


# ---- Loading the page in both browsers ----

_edges: list[Edge] = []
_edges_lock = threading.Lock()
_local = threading.local()


def worker_edge() -> Edge:
    edge = getattr(_local, "edge", None)
    if edge is None:
        edge = Edge(dark=SCHEME == "dark")
        edge.start()
        _local.edge = edge
        with _edges_lock:
            _edges.append(edge)
    return edge


def restart_worker_edge() -> None:
    edge = getattr(_local, "edge", None)
    if edge is not None:
        edge.stop()
        with _edges_lock:
            if edge in _edges:
                _edges.remove(edge)
        _local.edge = None


def edge_load(url: str, settle: float) -> dict:
    for attempt in range(2):
        try:
            return worker_edge().load(url, WIDTH, HEIGHT, settle, color_scheme=SCHEME)
        except (OSError, ConnectionError, RuntimeError, TimeoutError, ValueError) as error:
            restart_worker_edge()
            if attempt:
                return {"url": url, "shot": None, "measure": None, "loaded": False, "error": str(error)}
    return {}


def stillwater_load(exe: Path, url: str, settle: float, work: Path) -> dict:
    profile = work / "profile"
    profile.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, STILLWATER_NO_ADBLOCK="1", STILLWATER_HEADLESS_THEME=SCHEME)
    page = {"url": url, "shot": None, "measure": None, "loaded": False}
    try:
        with Stillwater(exe, profile, work / "log.txt", env=env, output=open(work / "out.txt", "wb"),
                        window_size=f"{WIDTH}x{HEIGHT}", page_load_seconds=30, script_seconds=20) as browser:
            try:
                browser.navigate(url)
                page["loaded"] = True
            except WebDriverError:
                pass
            time.sleep(settle)
            try:
                # Not eval(): pages with a strict Content-Security-Policy block it.
                value = browser.execute("return " + COLLECT_EXPRESSION)
                if isinstance(value, str):
                    page["measure"] = json.loads(value)
            except (WebDriverError, ValueError) as error:
                page["error"] = f"measure: {error}"
            shot = work / "shot.png"
            browser.screenshot(shot)
            page["shot"] = shot.read_bytes()
    except (WebDriverError, OSError) as error:
        page["error"] = str(error)
    return page


# ---- Comparing ----

def number(value: str) -> float:
    match = re.match(r"-?[\d.]+", value or "")
    return float(match.group()) if match else 0.0


def colour(value: str) -> tuple[float, ...]:
    parts = [float(x) for x in re.findall(r"[\d.]+", value or "")]
    if len(parts) == 3:
        parts.append(1.0)
    return tuple(parts[:4]) if len(parts) >= 4 else (0, 0, 0, 0)


def colours_differ(a: str, b: str) -> bool:
    ca, cb = colour(a), colour(b)
    if ca[3] < 0.05 and cb[3] < 0.05:
        return False
    return any(abs(x - y) > 10 for x, y in zip(ca[:3], cb[:3])) or abs(ca[3] - cb[3]) > 0.15


def geometry_differs(a: dict, b: dict, tolerance: int = 3) -> bool:
    return any(abs(a[key] - b[key]) > tolerance for key in ("x", "y", "w", "h"))


def label(element: dict) -> str:
    text = element.get("txt") or ""
    if text:
        return f"{element['t']} “{text[:40]}{'…' if len(text) > 40 else ''}”"
    tail = element["k"].split(">")[-3:]
    return f"{element['t']} (…{'>'.join(tail)})"


def compare_elements(a: dict | None, b: dict | None, s: dict | None) -> tuple[list[dict], int]:
    """Differences between Edge (a, checked against its second load b) and Stillwater (s)."""
    if not a or not s:
        return [], 0
    A = {e["k"]: e for e in a["elements"]}
    B = {e["k"]: e for e in b["elements"]} if b else A
    S = {e["k"]: e for e in s["elements"]}
    noisy = {k for k, e in A.items() if k not in B or geometry_differs(e, B[k], 6) or
             e.get("txt") != B[k].get("txt")}
    issues = []
    # The last move seen among each parent's children, in document order.
    sibling_moves: dict[str | None, tuple[int, int]] = {}
    for key, ea in A.items():
        if key in noisy:
            continue
        area = ea["w"] * ea["h"]
        es = S.get(key)
        if es is None:
            if area >= 600 and ea["y"] < HEIGHT * 2:
                issues.append({"kind": "missing", "key": key, "what": label(ea),
                               "detail": f"not found in Stillwater ({ea['w']}×{ea['h']} px in Edge)",
                               "weight": min(area, 60000) / 600, "rect": [ea["x"], ea["y"], ea["w"], ea["h"]]})
            continue
        dw, dh = es["w"] - ea["w"], es["h"] - ea["h"]
        # A size change only counts once: not again for a child that grew by the same amount.
        parent_key = key.rsplit(">", 1)[0] if ">" in key else None
        pa_, ps_ = A.get(parent_key), S.get(parent_key)
        same_as_parent = pa_ and ps_ and abs((ps_["w"] - pa_["w"]) - dw) <= 3 and abs((ps_["h"] - pa_["h"]) - dh) <= 3
        if not same_as_parent and (
                (abs(dw) > 4 and abs(dw) > 0.05 * ea["w"]) or (abs(dh) > 4 and abs(dh) > 0.05 * ea["h"])):
            parts = []
            if abs(dw) > 4:
                parts.append(f"{abs(dw)} px {'wider' if dw > 0 else 'narrower'} ({ea['w']} → {es['w']})")
            if abs(dh) > 4:
                parts.append(f"{abs(dh)} px {'taller' if dh > 0 else 'shorter'} ({ea['h']} → {es['h']})")
            issues.append({"kind": "size", "key": key, "what": label(ea), "detail": ", ".join(parts),
                           "weight": 1 + min(abs(dw) * ea["h"] + abs(dh) * ea["w"], 60000) / 1500,
                           "rect": [es["x"], es["y"], es["w"], es["h"]]})
        # A move only counts if the element moved differently from its parent.
        dx, dy = es["x"] - ea["x"], es["y"] - ea["y"]
        parent = key.rsplit(">", 1)[0] if ">" in key else None
        previous = sibling_moves.get(parent)
        sibling_moves[parent] = (dx, dy)
        if abs(dx) > 4 or abs(dy) > 4:
            pa, ps = A.get(parent), S.get(parent)
            moved_with_parent = pa and ps and abs((ps["x"] - pa["x"]) - dx) <= 3 and abs((ps["y"] - pa["y"]) - dy) <= 3
            # ...or by as much as the sibling before it (a list pushed down by an earlier item).
            moved_with_sibling = previous and abs(previous[0] - dx) <= 3 and abs(previous[1] - dy) <= 3
            if not moved_with_parent and not moved_with_sibling:
                where = []
                if abs(dy) > 4:
                    where.append(f"{abs(dy)} px {'lower' if dy > 0 else 'higher'}")
                if abs(dx) > 4:
                    where.append(f"{abs(dx)} px {'right' if dx > 0 else 'left'}")
                issues.append({"kind": "moved", "key": key, "what": label(ea), "detail": "sits " + ", ".join(where),
                               "weight": 1 + min(area, 60000) / 6000, "rect": [es["x"], es["y"], es["w"], es["h"]]})
        if ea.get("txt"):
            if abs(number(ea["fs"]) - number(es["fs"])) > 0.5:
                issues.append({"kind": "text size", "key": key, "what": label(ea),
                               "detail": f"text size {ea['fs']} → {es['fs']}", "weight": 3,
                               "rect": [es["x"], es["y"], es["w"], es["h"]]})
            if ea["fw"] != es["fw"]:
                issues.append({"kind": "text weight", "key": key, "what": label(ea),
                               "detail": f"weight {ea['fw']} → {es['fw']}", "weight": 2,
                               "rect": [es["x"], es["y"], es["w"], es["h"]]})
            if colours_differ(ea["c"], es["c"]):
                issues.append({"kind": "text colour", "key": key, "what": label(ea),
                               "detail": f"text colour {ea['c']} → {es['c']}", "weight": 2,
                               "rect": [es["x"], es["y"], es["w"], es["h"]]})
        if colours_differ(ea["bg"], es["bg"]) and area >= 400:
            issues.append({"kind": "background", "key": key, "what": label(ea),
                           "detail": f"background {ea['bg']} → {es['bg']}", "weight": 1 + min(area, 60000) / 6000,
                           "rect": [es["x"], es["y"], es["w"], es["h"]]})
    issues.sort(key=lambda issue: -issue["weight"])
    return issues, len(noisy)


def compare_pixels(a: bytes | None, b: bytes | None, s: bytes | None, out: Path) -> dict:
    """Share of the (non-noisy) viewport that looks different, and a heat map image."""
    if not a or not s:
        return {"ratio": None, "noise": None}
    load = lambda data: Image.open(__import__("io").BytesIO(data)).convert("RGB")
    ia, isw = load(a), load(s)
    ib = load(b) if b else ia
    w = min(ia.width, ib.width, isw.width, WIDTH)
    h = min(ia.height, ib.height, isw.height, HEIGHT)
    A, B, S = (np.asarray(image.crop((0, 0, w, h)), dtype=np.int16) for image in (ia, ib, isw))
    noise = np.abs(A - B).max(axis=2) > 32
    noise = np.asarray(Image.fromarray((noise * 255).astype(np.uint8)).filter(ImageFilter.MaxFilter(17))) > 0
    different = (np.abs(A - S).max(axis=2) > 40) & ~noise
    clean = max(1, int((~noise).sum()))
    # Heat map: Stillwater's page faded, differences in red, ignored (noisy) areas in blue.
    heat = (S * 0.35 + 255 * 0.65).astype(np.uint8)
    heat[noise] = (heat[noise] * 0.6 + np.array([90, 140, 255]) * 0.4).astype(np.uint8)
    heat[different] = np.array([230, 40, 40], dtype=np.uint8)
    Image.fromarray(heat).save(out)
    stillwater_blank = float(S.std()) < 3 and float(A.std()) > 10
    return {"ratio": round(float(different.sum()) / clean, 4), "noise": round(float(noise.mean()), 4),
            "blank": stillwater_blank}


def check_site(url: str, index: int, args, out: Path) -> dict:
    shots = out / "shots"
    work = Path(tempfile.mkdtemp(prefix="compare-", dir=out))
    started = time.time()
    try:
        first = edge_load(url, args.settle)
        mine = stillwater_load(args.exe, url, args.settle, work)
        second = edge_load(url, args.settle)
    finally:
        pass
    name = f"{index:04d}"
    for suffix, page in (("edge", first), ("stillwater", mine)):
        if page.get("shot"):
            (shots / f"{name}-{suffix}.png").write_bytes(page["shot"])
    pixels = compare_pixels(first.get("shot"), second.get("shot"), mine.get("shot"), shots / f"{name}-diff.png")
    issues, noisy = compare_elements(first.get("measure"), second.get("measure"), mine.get("measure"))
    weight = sum(issue["weight"] for issue in issues)
    ratio = pixels["ratio"]
    score = None if ratio is None else round(ratio * 100 + min(weight, 400) / 4, 1)
    shutil.rmtree(work, ignore_errors=True)
    return {
        "site": url, "index": name, "score": score, "pixels": pixels, "issues": issues[:40],
        "issue_count": len(issues), "noisy_elements": noisy,
        "edge_elements": len(first["measure"]["elements"]) if first.get("measure") else 0,
        "stillwater_elements": len(mine["measure"]["elements"]) if mine.get("measure") else 0,
        "title": (first.get("measure") or {}).get("title", ""),
        "errors": [e for e in (first.get("error"), mine.get("error"), second.get("error")) if e],
        "seconds": round(time.time() - started, 1),
    }


# ---- Report ----

def write_report(results: list[dict], out: Path, label_: str) -> None:
    ranked = sorted(results, key=lambda r: -(r["score"] if r["score"] is not None else 1e9))
    kinds: dict[str, int] = {}
    for result in results:
        for kind in {issue["kind"] for issue in result["issues"]}:
            kinds[kind] = kinds.get(kind, 0) + 1
    rows = []
    for r in ranked:
        px = r["pixels"]
        ratio = "—" if px.get("ratio") is None else f"{px['ratio'] * 100:.1f}%"
        issues = "".join(
            f"<li><b>{html.escape(i['kind'])}</b> {html.escape(i['what'])}: {html.escape(i['detail'])}</li>"
            for i in r["issues"][:14])
        notes = []
        if px.get("blank"):
            notes.append("Stillwater's page is blank")
        if px.get("noise") and px["noise"] > 0.4:
            notes.append(f"noisy page ({px['noise'] * 100:.0f}% changes between two Edge loads), less reliable")
        notes += [f"error: {e}" for e in r["errors"]]
        note_html = "".join(f"<p class=note>{html.escape(n)}</p>" for n in notes)
        rows.append(f"""
<section class=site id="s{r['index']}">
  <h2><span class=score>{'—' if r['score'] is None else r['score']}</span> {html.escape(r['title'] or r['site'])}</h2>
  <p class=meta><a href="{html.escape(r['site'])}">{html.escape(r['site'])}</a> · {ratio} of the page looks different · {r['issue_count']} element differences</p>
  {note_html}
  <div class=pair>
    <div class=slider style="--split: 50%">
      <img src="shots/{r['index']}-edge.png" alt="Edge">
      <img class=top src="shots/{r['index']}-stillwater.png" alt="Stillwater">
      <input type=range min=0 max=100 value=50 aria-label="Edge (left) or Stillwater (right)" oninput="this.parentNode.style.setProperty('--split', this.value + '%')">
      <span class=tag-left>Edge</span><span class=tag-right>Stillwater</span>
    </div>
    <img class=heat src="shots/{r['index']}-diff.png" alt="Differences in red, ignored areas in blue">
  </div>
  <ul class=issues>{issues or '<li>No element differences found.</li>'}</ul>
</section>""")
    kind_html = "".join(f"<li>{html.escape(k)}: {v} sites</li>" for k, v in sorted(kinds.items(), key=lambda kv: -kv[1]))
    page = f"""<!doctype html><meta charset=utf-8><title>Stillwater vs Edge: {html.escape(label_)}</title>
<style>
body {{ font: 14px/1.5 system-ui, sans-serif; margin: 0 auto; max-width: 1400px; padding: 16px; background: #f4f4f6; color: #1a1a1a; }}
h1 {{ font-size: 22px; }} h2 {{ font-size: 16px; margin: 0 0 4px; }}
.site {{ background: white; border-radius: 10px; padding: 14px; margin: 14px 0; }}
.score {{ display: inline-block; min-width: 44px; padding: 2px 6px; border-radius: 6px; background: #ffe2e0; color: #a01d14; text-align: center; }}
.meta {{ margin: 0 0 8px; color: #555; }} .note {{ margin: 0 0 6px; color: #8a5a00; }}
.pair {{ display: grid; grid-template-columns: 1fr 1fr; gap: 10px; }}
.pair img {{ width: 100%; display: block; border: 1px solid #ddd; }}
.slider {{ position: relative; }}
.slider .top {{ position: absolute; inset: 0; clip-path: inset(0 0 0 var(--split)); }}
.slider input {{ position: absolute; left: 0; right: 0; bottom: 6px; width: 100%; }}
.tag-left, .tag-right {{ position: absolute; top: 6px; padding: 1px 6px; border-radius: 4px; background: rgba(0,0,0,.6); color: white; font-size: 12px; }}
.tag-left {{ left: 6px; }} .tag-right {{ right: 6px; }}
.issues {{ margin: 10px 0 0; padding-left: 18px; columns: 2; }}
@media (max-width: 900px) {{ .pair {{ grid-template-columns: 1fr; }} .issues {{ columns: 1; }} }}
</style>
<h1>Stillwater vs Edge: {len(results)} sites</h1>
<p>Sites are ranked by how different they look: the share of the page whose pixels differ (after ignoring what also changes between two loads in Edge), plus weighted element differences. Drag the slider to compare; in the heat map red is different, blue was ignored as changing content.</p>
<h2>Most common differences</h2><ul>{kind_html or '<li>None</li>'}</ul>
{''.join(rows)}
"""
    (out / "report.html").write_text(page, encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--site", action="append", help="compare this URL (repeatable)")
    parser.add_argument("--sites", type=Path, default=HERE / "sites.json")
    parser.add_argument("--category", action="append")
    parser.add_argument("--jobs", type=int, default=3)
    parser.add_argument("--settle", type=float, default=4, help="seconds to wait after load before measuring")
    parser.add_argument("--label", default=time.strftime("%Y%m%d-%H%M"))
    parser.add_argument("--exe", type=Path, default=HERE.parents[1] / "target" / "release" / "servoshell.exe")
    parser.add_argument("--limit", type=int, default=0, help="only the first N sites")
    args = parser.parse_args()
    if args.site:
        sites = args.site
    else:
        catalog = json.loads(args.sites.read_text(encoding="utf-8"))
        sites = [url for category, urls in catalog.items()
                 if not args.category or category in args.category for url in urls]
    if args.limit:
        sites = sites[:args.limit]
    out = HERE / "out" / f"compare-{args.label}"
    (out / "shots").mkdir(parents=True, exist_ok=True)
    print(f"Comparing {len(sites)} sites with {args.jobs} jobs, {SCHEME} mode (Windows' theme) -> {out}")
    results = []
    try:
        with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
            futures = {pool.submit(check_site, url, i, args, out): url for i, url in enumerate(sites)}
            for done, future in enumerate(concurrent.futures.as_completed(futures), 1):
                url = futures[future]
                try:
                    result = future.result()
                except Exception as error:  # keep going: one broken site mustn't stop the run
                    result = {"site": url, "index": "x", "score": None, "pixels": {}, "issues": [],
                              "issue_count": 0, "noisy_elements": 0, "title": "", "errors": [repr(error)]}
                results.append(result)
                ratio = result["pixels"].get("ratio")
                print(f"[{done}/{len(sites)}] {result['score']!s:>6}  {'' if ratio is None else f'{ratio * 100:5.1f}%'}  {url}", flush=True)
                if done % 25 == 0:
                    (out / "results.json").write_text(json.dumps(results, indent=1), encoding="utf-8")
    finally:
        with _edges_lock:
            for edge in list(_edges):
                edge.stop()
            _edges.clear()
    (out / "results.json").write_text(json.dumps(results, indent=1), encoding="utf-8")
    write_report(results, out, args.label)
    print(f"Report: {out / 'report.html'}")


if __name__ == "__main__":
    main()
