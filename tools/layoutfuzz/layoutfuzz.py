# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Differential layout fuzzer: Stillwater vs headless Microsoft Edge.

Generates small random pages (seeded), measures getBoundingClientRect() of every element in
both browsers, reduces the pages that disagree to minimal cases and groups them by the CSS
left in them. See README.md.

    python layoutfuzz.py run --exe SW.exe --start 0 --count 3000 --out DIR
    python layoutfuzz.py one --exe SW.exe --seed 1234            # re-run one generated page
    python layoutfuzz.py case --exe SW.exe page.html [...]       # compare hand-written pages
    python layoutfuzz.py reduce --exe SW.exe --out DIR           # reduce + classify a run
    python layoutfuzz.py recheck --exe SW.exe [--cases DIR]      # saved cases as a regression suite
"""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import http.server
import json
import os
import random
import re
import sys
import tempfile
import threading
import time
from collections import Counter, defaultdict
from copy import deepcopy
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import quote

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "sitecheck"))

WIDTH, HEIGHT = 800, 600
DEFAULT_CASES = Path(r"L:\servo-dev\perf\layoutfuzz")
SCRATCH = Path(tempfile.gettempdir()) / "claude-sw-layoutfuzz"

BASE_CSS = ("html,body{margin:0;padding:0}"
            # font-size 0: line boxes and the strut stay (line-height 20px), but no font metrics
            # (Chromium rounds Arial's ascent/descent to whole pixels, Servo doesn't).
            "body{font:0/20px Arial}"
            "input,textarea,button{font:16px/20px Arial;margin:0;padding:2px;border:2px solid #333;"
            "vertical-align:baseline}"
            "button{font-size:0}"
            "textarea{resize:none}"
            "[data-id]{background:rgba(0,90,200,.12);outline:1px solid rgba(0,0,0,.25)}")

MEASURE_JS = """(() => {
  const rects = {};
  for (const el of document.querySelectorAll('[data-id]')) {
    const r = el.getBoundingClientRect();
    rects[el.dataset.id] = [r.x, r.y, r.width, r.height, getComputedStyle(el).display];
  }
  const d = document.documentElement;
  return JSON.stringify({rects, sw: d.scrollWidth, sh: d.scrollHeight, iw: innerWidth,
                         ih: innerHeight, cw: d.clientWidth, dpr: devicePixelRatio});
})()"""


# ---------------------------------------------------------------------------------------------
# Page model

VOID = {"img", "input"}


@dataclass
class Node:
    tag: str
    id: int
    style: dict = field(default_factory=dict)
    attrs: dict = field(default_factory=dict)
    children: list = field(default_factory=list)

    def walk(self):
        yield self
        for child in self.children:
            yield from child.walk()


def to_html(root: Node, title: str = "lf") -> str:
    def emit(node: Node) -> str:
        attrs = f' data-id="{node.id}"'
        for key, value in node.attrs.items():
            attrs += f' {key}="{value}"'
        if node.style:
            attrs += ' style="' + ";".join(f"{k}:{v}" for k, v in node.style.items()) + '"'
        if node.tag in VOID:
            return f"<{node.tag}{attrs}>"
        return f"<{node.tag}{attrs}>" + "".join(emit(c) for c in node.children) + f"</{node.tag}>"
    return (f"<!doctype html><html><head><meta charset=utf-8><title>{title}</title>"
            f"<style>{BASE_CSS}</style></head><body>{emit(root)}</body></html>")


def svg_uri(w: int | None, h: int | None, viewbox: bool = False) -> str:
    size = (f" width='{w}'" if w else "") + (f" height='{h}'" if h else "")
    vb = f" viewBox='0 0 {w or 40} {h or 30}'" if viewbox else ""
    svg = (f"<svg xmlns='http://www.w3.org/2000/svg'{size}{vb}>"
           f"<rect width='100%' height='100%' fill='teal'/></svg>")
    return "data:image/svg+xml," + quote(svg, safe="=:/ '")


# ---------------------------------------------------------------------------------------------
# Generator

def wchoice(rng: random.Random, table):
    """table: list of (value, weight)."""
    total = sum(w for _, w in table)
    pick = rng.random() * total
    for value, weight in table:
        pick -= weight
        if pick < 0:
            return value
    return table[-1][0]


# Known bugs the generator can steer around (--mask), so they don't swallow every page that
# contains them during reduction. Seeds are only reproducible with the same masks (saved in
# the run folder's config.json).
MASKS = {
    "writing-mode": "no writing-mode (Servo lays vertical modes out horizontally)",
    "aspect-ratio": "no aspect-ratio property",
    "svg-ratio": "no <img> whose SVG has only a viewBox (ratio, no size)",
    "textarea-height": "textareas always get a px height",
    "control-baseline": "inputs, textareas and buttons get vertical-align:top",
    "intrinsic-minmax": "no min-content/max-content/fit-content in min-* and max-* properties",
    "empty-table": "the reducer keeps at least one cell in every table",
}
INTRINSIC_KEYWORDS = ("min-content", "max-content", "fit-content")
ACTIVE_MASKS: set[str] = set()

PX = [0, 5, 10, 15, 20, 25, 30, 40, 50, 60, 75, 80, 100, 120, 150, 200, 250, 300, 400]
PCT = [10, 25, 33, 50, 75, 100]


def length(rng, kw=True, pct=True, small=False, neg=False):
    table = [("px", 6)] + ([("pct", 2)] if pct else []) + \
            ([("auto", 1.5), ("min-content", .6), ("max-content", .6), ("fit-content", .6)] if kw else [])
    kind = wchoice(rng, table)
    if kind == "px":
        value = rng.choice(PX[:9] if small else PX)
        if neg and rng.random() < .2:
            value = -value
        return f"{value}px"
    if kind == "pct":
        return f"{rng.choice(PCT[:3] if small else PCT)}%"
    return kind


def gen_size_props(rng, s, axis_weights=1.0):
    if rng.random() < .35 * axis_weights:
        s["width"] = length(rng)
    if rng.random() < .3 * axis_weights:
        s["height"] = length(rng)
    for prop in ("min-width", "min-height"):
        if rng.random() < .1:
            s[prop] = wchoice(rng, [(length(rng, kw=False), 4), ("min-content", 1), ("max-content", .5),
                                    ("0", 1), ("auto", .5)])
            if "intrinsic-minmax" in ACTIVE_MASKS and s[prop] in INTRINSIC_KEYWORDS:
                s[prop] = "0"
    for prop in ("max-width", "max-height"):
        if rng.random() < .1:
            s[prop] = wchoice(rng, [(length(rng, kw=False), 4), ("min-content", .7), ("max-content", .5),
                                    ("fit-content", .5), ("none", .3)])
            if "intrinsic-minmax" in ACTIVE_MASKS and s[prop] in INTRINSIC_KEYWORDS:
                s[prop] = "none"
    if rng.random() < .08 and "aspect-ratio" not in ACTIVE_MASKS:
        s["aspect-ratio"] = rng.choice(["1", "16 / 9", "2 / 1", "1 / 2", "4 / 3", "auto 1"])


def gen_box_props(rng, s):
    if rng.random() < .28:
        if rng.random() < .5:
            s["padding"] = length(rng, kw=False, small=True)
        else:
            s[rng.choice(["padding-top", "padding-left", "padding-right", "padding-bottom"])] = \
                length(rng, kw=False, small=True)
    if rng.random() < .22:
        width = rng.choice([1, 2, 3, 5, 8, 10])
        side = rng.choice(["border", "border", "border-top", "border-left", "border-right", "border-bottom"])
        s[side] = f"{width}px solid #000"
    if rng.random() < .25:
        s["box-sizing"] = "border-box"
    if rng.random() < .28:
        def m():
            return wchoice(rng, [(length(rng, kw=False, small=True, neg=True), 6), ("auto", 2),
                                 (f"{rng.choice(PCT[:3])}%", .5)])
        if rng.random() < .4:
            s["margin"] = m()
        else:
            s[rng.choice(["margin-top", "margin-left", "margin-right", "margin-bottom"])] = m()


def gen_position(rng, s):
    if rng.random() < .13:
        pos = wchoice(rng, [("relative", 3), ("absolute", 4), ("sticky", 1), ("fixed", .4)])
        s["position"] = pos
        for side in ("top", "left", "right", "bottom"):
            if rng.random() < (.45 if pos != "sticky" else .25):
                s[side] = wchoice(rng, [(f"{rng.choice(PX[:10])}px", 5), (f"{rng.choice(PCT[:4])}%", 2),
                                        ("auto", 1)])


def gen_container_props(rng, s, display):
    if "flex" in display:
        if rng.random() < .5:
            s["flex-direction"] = rng.choice(["row", "column", "column", "row-reverse", "column-reverse"])
        if rng.random() < .3:
            s["flex-wrap"] = rng.choice(["wrap", "wrap", "wrap-reverse", "nowrap"])
        if rng.random() < .4:
            s["justify-content"] = rng.choice(["center", "flex-end", "space-between", "space-around",
                                               "space-evenly", "flex-start", "end"])
        if rng.random() < .4:
            s["align-items"] = rng.choice(["center", "flex-start", "flex-end", "stretch", "baseline", "end"])
        if rng.random() < .15:
            s["align-content"] = rng.choice(["center", "flex-start", "space-between", "stretch", "flex-end"])
    elif "grid" in display:
        def track():
            return wchoice(rng, [(f"{rng.choice(PX[2:12])}px", 4), (f"{rng.choice([1, 1, 2, 3])}fr", 4),
                                 ("auto", 3), (f"minmax({rng.choice(PX[1:10])}px, 1fr)", 1.5),
                                 (f"{rng.choice(PCT[:4])}%", 1), ("min-content", 1), ("max-content", 1),
                                 (f"fit-content({rng.choice(PX[4:12])}px)", .7),
                                 ("minmax(0, 1fr)", 1), ("minmax(auto, 100px)", .5)])

        def tracks():
            if rng.random() < .12:
                return rng.choice(["repeat(auto-fill, minmax(60px, 1fr))", "repeat(auto-fit, minmax(50px, 1fr))",
                                   "repeat(3, 1fr)", "repeat(2, 100px)", "repeat(auto-fill, 80px)"])
            return " ".join(track() for _ in range(rng.choice([1, 2, 2, 3, 3, 4])))
        if rng.random() < .7:
            s["grid-template-columns"] = tracks()
        if rng.random() < .35:
            s["grid-template-rows"] = tracks()
        if rng.random() < .12:
            s["grid-auto-rows"] = rng.choice(["50px", "1fr", "auto", "minmax(30px, auto)", "min-content"])
        if rng.random() < .08:
            s["grid-auto-columns"] = rng.choice(["50px", "1fr", "auto", "min-content"])
        if rng.random() < .15:
            s["grid-auto-flow"] = rng.choice(["column", "row dense", "dense", "column dense"])
        for prop in ("justify-items", "align-items"):
            if rng.random() < .25:
                s[prop] = rng.choice(["start", "center", "end", "stretch", "baseline"])
        for prop in ("justify-content", "align-content"):
            if rng.random() < .15:
                s[prop] = rng.choice(["start", "center", "end", "space-between", "space-around", "stretch"])
    elif display in ("block", "flow-root", "inline-block") and rng.random() < .1:
        s["text-align"] = rng.choice(["center", "right"])
    if ("flex" in display or "grid" in display) and rng.random() < .3:
        prop = rng.choice(["gap", "row-gap", "column-gap"])
        s[prop] = rng.choice(["5px", "10px", "20px", "10px 20px" if prop == "gap" else "15px", "5%"])
    if rng.random() < .1:
        s["overflow"] = rng.choice(["hidden", "hidden", "auto", "clip"])
    if rng.random() < .03 and "writing-mode" not in ACTIVE_MASKS:
        s["writing-mode"] = rng.choice(["vertical-lr", "vertical-rl"])


def gen_item_props(rng, s, parent_display):
    if "flex" in parent_display:
        if rng.random() < .3:
            s["flex"] = rng.choice(["1", "auto", "none", "1 1 0", "2 0 50px", "0 0 auto", "1 0 auto",
                                    "0 1 100px", "1 1 0%"])
        else:
            if rng.random() < .25:
                s["flex-grow"] = rng.choice(["1", "1", "2", "0.5"])
            if rng.random() < .15:
                s["flex-shrink"] = rng.choice(["0", "0", "2", "1"])
            if rng.random() < .2:
                s["flex-basis"] = rng.choice(["0", "auto", "content", "50px", "100px", "50%", "min-content"])
        if rng.random() < .15:
            s["align-self"] = rng.choice(["center", "flex-start", "flex-end", "stretch", "baseline"])
        if rng.random() < .05:
            s["order"] = rng.choice(["-1", "1", "2"])
        if rng.random() < .07:
            s["margin-" + rng.choice(["left", "top", "right", "bottom"])] = "auto"
    elif "grid" in parent_display:
        if rng.random() < .2:
            s["grid-column"] = rng.choice(["span 2", "1 / 3", "2", "1 / -1", "span 3", "3"])
        if rng.random() < .15:
            s["grid-row"] = rng.choice(["span 2", "1 / 3", "2", "1 / -1"])
        if rng.random() < .15:
            s["justify-self"] = rng.choice(["start", "center", "end", "stretch"])
        if rng.random() < .15:
            s["align-self"] = rng.choice(["start", "center", "end", "stretch", "baseline"])
    elif parent_display in ("block", "flow-root", "inline-block", "table-cell") and rng.random() < .05:
        s["float"] = rng.choice(["left", "right"])
        if rng.random() < .3:
            s["clear"] = "both"


CONTAINERS = [("block", 3), ("flex", 4), ("grid", 3), ("inline-block", 1), ("inline-flex", .6),
              ("inline-grid", .4), ("flow-root", .3), ("inline", .4), ("table", .3), ("contents", .1)]
LEAVES = [("box", 4), ("word", 2), ("img", 1), ("textarea", .8), ("input", .6), ("button", .4),
          ("canvas", .3), ("empty", .5)]


class Gen:
    def __init__(self, seed: int, max_nodes: int | None = None):
        self.rng = random.Random(seed)
        self.next_id = 0
        self.target = max_nodes or self.rng.randint(2, 12)

    def node(self, tag) -> Node:
        node = Node(tag, self.next_id)
        self.next_id += 1
        return node

    def common(self, s, parent_display, is_root=False):
        gen_size_props(self.rng, s)
        gen_box_props(self.rng, s)
        if not is_root:
            gen_position(self.rng, s)
            gen_item_props(self.rng, s, parent_display)
        if parent_display in ("block", "flow-root", "inline-block", "table-cell", "inline") and \
                self.rng.random() < .08:
            s["vertical-align"] = self.rng.choice(["top", "middle", "bottom", "text-bottom", "10px"])

    def container(self, parent_display, is_root=False) -> tuple[Node, str]:
        rng = self.rng
        display = "block" if is_root and rng.random() < .3 else wchoice(rng, CONTAINERS)
        if is_root and display in ("contents", "inline"):
            display = "block"
        node = self.node("span" if display == "inline" else "div")
        s = node.style
        if display not in ("block",) or rng.random() < .2:
            if not (display == "inline" and node.tag == "span"):
                s["display"] = display
        if is_root and rng.random() < .5:
            s["width"] = f"{rng.choice([200, 300, 400, 500, 600])}px"
        self.common(s, parent_display, is_root)
        gen_container_props(rng, s, display)
        return node, display

    def leaf(self, parent_display) -> Node:
        rng = self.rng
        kind = wchoice(rng, LEAVES)
        if parent_display == "inline" and kind in ("box", "empty"):
            kind = "word"
        if kind in ("box", "empty"):
            node = self.node("div")
            if kind == "box":
                if rng.random() < .75:
                    node.style["width"] = f"{rng.choice(PX[2:15])}px"
                if rng.random() < .75:
                    node.style["height"] = f"{rng.choice(PX[2:13])}px"
            self.common(node.style, parent_display)
        elif kind == "word":
            node = self.node("span")
            node.style.update({"display": "inline-block", "width": f"{rng.choice(PX[2:11])}px",
                               "height": f"{rng.choice([10, 15, 20, 25, 30])}px"})
            if rng.random() < .3:
                self.common(node.style, parent_display)
        elif kind in ("img", "canvas"):
            node = self.node(kind)
            if kind == "img":
                w, h = rng.choice([(40, 30), (100, 50), (20, 60), (64, 64), (150, 100)])
                variant = rng.random()
                if variant < .75 or "svg-ratio" in ACTIVE_MASKS:
                    node.attrs["src"] = svg_uri(w, h)
                else:
                    node.attrs["src"] = svg_uri(None, None, viewbox=True)  # ratio only
                if rng.random() < .25:
                    node.attrs["width"] = str(rng.choice([30, 50, 80]))
            else:
                if rng.random() < .5:
                    node.attrs["width"] = str(rng.choice([50, 100, 200]))
                    node.attrs["height"] = str(rng.choice([30, 50, 100]))
            self.common(node.style, parent_display)
        else:  # form controls: font-independent unless width is auto where stretch may apply
            node = self.node(kind)
            if kind == "button":
                word = self.node("span")
                word.style.update({"display": "inline-block", "width": f"{rng.choice([20, 40, 60])}px",
                                   "height": "16px"})
                node.children.append(word)
            if rng.random() < .4:
                node.style["height"] = f"{rng.choice(PX[3:12])}px" if rng.random() < .8 else \
                    f"{rng.choice(PCT)}%"
            self.common(node.style, parent_display)
            if kind == "textarea" and "textarea-height" in ACTIVE_MASKS:
                node.style["height"] = f"{rng.choice(PX[3:12])}px"
            if "control-baseline" in ACTIVE_MASKS:
                node.style["vertical-align"] = "top"
            if kind != "button":  # see font_independent()
                node.style["width"] = f"{rng.choice(PX[5:16])}px"
                for prop in ("min-width", "max-width"):
                    if not node.style.get(prop, "0px").endswith("px"):
                        del node.style[prop]
        return node

    def table(self, parent: Node):
        rng = self.rng
        table = self.node("table")
        if rng.random() < .5:
            table.style["width"] = rng.choice(["100%", "300px", "200px"])
        if rng.random() < .3:
            table.style["border-collapse"] = "collapse"
        if rng.random() < .3:
            table.style["border-spacing"] = "0"
        parent.children.append(table)
        tbody = []
        for _ in range(rng.choice([1, 1, 2])):
            tr = self.node("tr")
            table.children.append(tr)
            for _ in range(rng.choice([1, 2, 3])):
                td = self.node("td")
                if rng.random() < .3:
                    td.style["width"] = rng.choice(["50px", "50%", "100px"])
                if rng.random() < .3:
                    td.style["padding"] = rng.choice(["0", "5px", "10px"])
                if rng.random() < .2:
                    td.style["vertical-align"] = rng.choice(["top", "middle", "bottom"])
                tr.children.append(td)
                tbody.append(td)
        return tbody

    def generate(self) -> Node:
        rng = self.rng
        root, display = self.container("block", is_root=True)
        containers = [(root, display, 1)]
        count = 1
        while count < self.target:
            parent, pdisplay, depth = rng.choice(containers)
            if rng.random() < .04 and depth <= 2 and pdisplay != "inline" and self.target - count >= 4:
                cells = self.table(parent)
                count += 1 + len(cells)
                for td in cells:
                    containers.append((td, "table-cell", depth + 2))
                    td.children.append(self.leaf("table-cell"))
                    count += 1
                continue
            if depth < 4 and rng.random() < .35:
                child, cdisplay = self.container(pdisplay)
                if pdisplay == "inline" and cdisplay not in ("inline", "inline-block", "inline-flex",
                                                             "inline-grid"):
                    child.style["display"] = cdisplay = "inline-block"
                containers.append((child, cdisplay, depth + 1))
            else:
                child = self.leaf(pdisplay)
            parent.children.append(child)
            count += 1 + len(child.children)
        return root


def generate(seed: int) -> Node:
    return Gen(seed).generate()


def font_independent(root: Node) -> bool:
    """A textarea's or input's intrinsic width is cols/size times the font's average character
    width, which Chromium and Servo compute differently (and Chromium adds scrollbar room), so
    every text control must keep a px width (and no content-sized or % min/max width)."""
    for node in root.walk():
        if node.tag in ("textarea", "input"):
            if not node.style.get("width", "").endswith("px"):
                return False
            if any(not node.style.get(p, "0px").endswith("px") for p in ("min-width", "max-width")):
                return False
    return respects_masks(root)


def respects_masks(root: Node) -> bool:
    """The reducer must not undo what a mask put in, or it slides back into the masked bug."""
    for node in root.walk():
        if node.tag == "table" and "empty-table" in ACTIVE_MASKS and                 not any(n.tag == "td" for n in node.walk()):
            return False
        if node.tag == "textarea" and "textarea-height" in ACTIVE_MASKS and \
                not node.style.get("height", "").endswith("px"):
            return False
        if node.tag in ("textarea", "input", "button") and "control-baseline" in ACTIVE_MASKS and \
                node.style.get("vertical-align") != "top":
            return False
    return True


# ---------------------------------------------------------------------------------------------
# Browsers and server

class PageServer:
    """Serves pages from memory on 127.0.0.1 (no-store, unique URL per load)."""

    def __init__(self):
        self.pages: dict[str, bytes] = {}
        self.counter = 0
        self.lock = threading.Lock()
        pages = self.pages

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                body = pages.get(self.path)
                if body is None:
                    self.send_response(404)
                    self.end_headers()
                    return
                self.send_response(200)
                self.send_header("Content-Type", "text/html; charset=utf-8")
                self.send_header("Cache-Control", "no-store")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.httpd.server_address[1]
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def put(self, html: str) -> str:
        with self.lock:
            self.counter += 1
            path = f"/p/{self.counter}.html"
            self.pages[path] = html.encode()
            # keep memory flat: only the last few pages need to stay
            old = f"/p/{self.counter - 8}.html"
            self.pages.pop(old, None)
        return f"http://127.0.0.1:{self.port}{path}"

    def close(self):
        self.httpd.shutdown()


def free_memory_gb() -> float:
    class MS(ctypes.Structure):
        _fields_ = [("dwLength", ctypes.c_ulong), ("dwMemoryLoad", ctypes.c_ulong),
                    ("ullTotalPhys", ctypes.c_ulonglong), ("ullAvailPhys", ctypes.c_ulonglong),
                    ("ullTotalPageFile", ctypes.c_ulonglong), ("ullAvailPageFile", ctypes.c_ulonglong),
                    ("ullTotalVirtual", ctypes.c_ulonglong), ("ullAvailVirtual", ctypes.c_ulonglong),
                    ("ullAvailExtendedVirtual", ctypes.c_ulonglong)]
    status = MS()
    status.dwLength = ctypes.sizeof(MS)
    ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(status))
    return status.ullAvailPhys / 2 ** 30


def wait_for_memory(minimum_gb: float = 3.0):
    while (free := free_memory_gb()) < minimum_gb:
        print(f"  only {free:.1f} GB free; waiting", flush=True)
        time.sleep(30)


def below_normal_priority():
    """Children (Stillwater, Edge and their helpers) inherit BELOW_NORMAL from this process."""
    if os.name == "nt":
        kernel32 = ctypes.windll.kernel32
        kernel32.SetPriorityClass(kernel32.GetCurrentProcess(), 0x00004000)


def make_edge():
    """edge.py's Edge, launched with --edge-skip-compat-layer-relaunch: without it Edge 157
    relaunches itself, the PID we started exits and edge.py reports "Edge didn't start"."""
    import subprocess
    import urllib.request
    from edge import Edge, WebSocket

    class HeadlessEdge(Edge):
        def start(self):
            args = [self.exe, "--headless=new", "--edge-skip-compat-layer-relaunch",
                    "--remote-debugging-port=0", "--remote-debugging-address=127.0.0.1",
                    f"--user-data-dir={self.profile}", "--mute-audio", "--hide-scrollbars", "--no-first-run",
                    "--no-default-browser-check", "--disable-extensions", "--disable-sync",
                    "--disable-background-networking", "--force-device-scale-factor=1",
                    f"--window-size={WIDTH},{HEIGHT}", "about:blank"]
            self.process = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                            creationflags=0x08000000)  # CREATE_NO_WINDOW
            port_file = self.profile / "DevToolsActivePort"
            deadline = time.time() + 30
            while not port_file.exists() or not port_file.read_text().strip():
                if time.time() > deadline or self.process.poll() is not None:
                    self.stop()
                    raise RuntimeError("Edge didn't start")
                time.sleep(0.1)
            port = port_file.read_text().split()[0]
            version = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version", timeout=10))
            self.ws = WebSocket(version["webSocketDebuggerUrl"])

    return HeadlessEdge()


class EdgeRunner:
    def __init__(self):
        SCRATCH.mkdir(parents=True, exist_ok=True)
        tempfile.tempdir = str(SCRATCH)  # edge.py's throwaway profile goes in our scratch folder
        wait_for_memory()
        self.edge = make_edge()
        self.edge.start()
        target = self.edge.send("Target.createTarget", {"url": "about:blank"})["targetId"]
        self.session = self.edge.send("Target.attachToTarget", {"targetId": target, "flatten": True})["sessionId"]
        self.edge.send("Emulation.setDeviceMetricsOverride",
                       {"width": WIDTH, "height": HEIGHT, "deviceScaleFactor": 1, "mobile": False}, self.session)
        self.edge.send("Page.enable", session=self.session)

    @property
    def pid(self):
        return self.edge.process.pid

    def measure(self, url: str) -> dict:
        self.edge.send("Page.navigate", {"url": url}, self.session, timeout=30,
                       wait_event=("Page.loadEventFired", self.session))
        result = self.edge.send("Runtime.evaluate", {"expression": MEASURE_JS, "returnByValue": True},
                                self.session, timeout=20)
        return json.loads(result["result"]["value"])

    def close(self):
        self.edge.stop()


class StillwaterRunner:
    def __init__(self, exe: Path):
        self.exe = Path(exe)
        self.sw = None
        self.loads = 0
        self.start()

    def start(self):
        from driver import Stillwater
        SCRATCH.mkdir(parents=True, exist_ok=True)
        wait_for_memory()
        self.profile = profile = Path(tempfile.mkdtemp(prefix="sw-profile-", dir=SCRATCH))
        self.log = open(profile / "out.txt", "wb")
        self.sw = Stillwater(self.exe, profile, profile / "log.txt", output=self.log,
                             window_size=f"{WIDTH}x{HEIGHT}", page_load_seconds=20, script_seconds=10)
        self.sw.start_session()
        self.loads = 0

    @property
    def pid(self):
        return self.sw.process.pid

    def restart(self):
        self.close()
        self.start()

    def measure(self, url: str) -> dict:
        if self.loads >= 1500:  # keep memory flat over long runs
            self.restart()
        self.loads += 1
        self.sw.navigate(url)
        return json.loads(self.sw.execute("return " + MEASURE_JS))

    def close(self):
        if self.sw:
            self.sw.quit()
            self.sw = None
            self.log.close()
            import shutil
            shutil.rmtree(self.profile, ignore_errors=True)


class Harness:
    def __init__(self, exe: Path, edge: bool = True):
        below_normal_priority()
        self.server = PageServer()
        self.sw = StillwaterRunner(exe)
        try:
            self.edge = EdgeRunner() if edge else None
        except BaseException:
            self.sw.close()
            self.server.close()
            raise
        self.edge_cache: dict[str, dict] = {}
        print(f"  Stillwater pid {self.sw.pid}" + (f", Edge pid {self.edge.pid}" if self.edge else ""),
              flush=True)

    def run_sw(self, html: str, retries: int = 1) -> dict:
        for attempt in range(retries + 1):
            try:
                return self.sw.measure(self.server.put(html))
            except Exception as error:  # crash or hang: restart and report
                crashed = not self.sw.sw.alive()
                print(f"  Stillwater {'crashed' if crashed else 'failed'}: {error}", flush=True)
                self.sw.restart()
                if attempt == retries:
                    return {"error": ("crash: " if crashed else "") + str(error)[:300]}

    def run_edge(self, html: str) -> dict:
        key = hashlib.sha1(html.encode()).hexdigest()
        if key not in self.edge_cache:
            if len(self.edge_cache) > 20000:
                self.edge_cache.clear()
            self.edge_cache[key] = self.edge.measure(self.server.put(html))
        return self.edge_cache[key]

    def both(self, html: str) -> tuple[dict, dict]:
        return self.run_sw(html), self.run_edge(html)

    def close(self):
        try:
            self.sw.close()
        finally:
            if self.edge:
                self.edge.close()
            self.server.close()


# ---------------------------------------------------------------------------------------------
# Comparison

def compare(sw: dict, edge: dict, tol: float = 1.0) -> list[dict]:
    """Elements whose rects differ by more than tol px. Non-atomic inline boxes (display:inline
    in Edge) are compared on x/width only: their height comes from font ascent/descent."""
    if "error" in sw:
        return [{"id": "crash", "error": sw["error"]}]
    out = []
    for key, er in edge["rects"].items():
        sr = sw["rects"].get(key)
        if sr is None:
            out.append({"id": key, "sw": None, "edge": er[:4]})
            continue
        axes = (0, 2) if er[4] == "inline" else (0, 1, 2, 3)
        diff = max(abs(sr[i] - er[i]) for i in axes)
        if diff > tol:
            out.append({"id": key, "sw": [round(v, 2) for v in sr[:4]], "edge": [round(v, 2) for v in er[:4]],
                        "diff": round(diff, 2), "display": er[4]})
    for key in ("sw", "sh"):
        if abs(sw[key] - edge[key]) > tol and not out:
            out.append({"id": "root-" + key, "sw": sw[key], "edge": edge[key], "diff": abs(sw[key] - edge[key])})
    return out


# ---------------------------------------------------------------------------------------------
# Reduction

def find_parent(root: Node, target: Node):
    for node in root.walk():
        for i, child in enumerate(node.children):
            if child is target:
                return node, i
    return None, None


def reduce_case(h: Harness, root: Node, anchor_ids: set[str], tol: float, budget_s: float = 240):
    """Delta-debug: drop subtrees, unwrap elements, drop declarations and attributes while
    the page still disagrees with Edge on one of the originally mismatching elements (or
    crashes, if it crashed)."""
    started = time.time()
    tests = 0

    def failing(candidate: Node) -> bool:
        nonlocal tests
        if not font_independent(candidate):
            return False
        tests += 1
        html = to_html(candidate)
        sw, edge = h.both(html)
        mism = compare(sw, edge, tol)
        if "crash" in anchor_ids:
            return any(m["id"] == "crash" for m in mism)
        return any(m["id"] in anchor_ids or m["id"].startswith("root-") and "root" in anchor_ids
                   for m in mism)

    current = deepcopy(root)
    changed = True
    while changed and time.time() - started < budget_s:
        changed = False
        # 1. remove subtrees (biggest first) and unwrap single elements
        nodes = sorted(list(current.walk())[1:], key=lambda n: -sum(1 for _ in n.walk()))
        for node in nodes:
            if time.time() - started > budget_s:
                break
            for op in ("remove", "unwrap"):
                trial = deepcopy(current)
                tnode = next(n for n in trial.walk() if n.id == node.id) if any(
                    n.id == node.id for n in trial.walk()) else None
                if tnode is None:
                    break
                parent, index = find_parent(trial, tnode)
                if parent is None:
                    break
                if op == "remove":
                    parent.children.pop(index)
                else:
                    if tnode.tag in VOID or not tnode.children:
                        continue
                    parent.children[index:index + 1] = tnode.children
                if failing(trial):
                    current = trial
                    changed = True
                    break
        # root unwrap: make its only child the root
        if len(current.children) == 1 and current.children[0].tag not in ("tr", "td"):
            trial = deepcopy(current.children[0])
            if failing(trial):
                current = trial
                changed = True
        # 2. remove declarations and attributes
        for node in list(current.walk()):
            for prop in list(node.style):
                if time.time() - started > budget_s:
                    break
                trial = deepcopy(current)
                tnode = next(n for n in trial.walk() if n.id == node.id)
                del tnode.style[prop]
                if failing(trial):
                    current = trial
                    changed = True
            for attr in list(node.attrs):
                if attr == "src":
                    continue
                trial = deepcopy(current)
                tnode = next(n for n in trial.walk() if n.id == node.id)
                del tnode.attrs[attr]
                if failing(trial):
                    current = trial
                    changed = True
        # 3. simplify values: compound shorthands to simpler ones, odd px to round numbers
        for node in list(current.walk()):
            for prop, value in list(node.style.items()):
                simpler = simplify_value(prop, value)
                if simpler and simpler != value:
                    trial = deepcopy(current)
                    tnode = next(n for n in trial.walk() if n.id == node.id)
                    tnode.style[prop] = simpler
                    if failing(trial):
                        current = trial
                        changed = True
    return current, tests, time.time() - started


def simplify_value(prop: str, value: str) -> str | None:
    if prop.startswith("border") and value.endswith("solid #000"):
        return None
    if " " in value and not value.startswith(("minmax", "repeat", "fit-content", "span")) and \
            prop.startswith("grid-template"):
        parts = value.split(" ")
        return " ".join(parts[:-1]) if len(parts) > 1 else None
    return None


# ---------------------------------------------------------------------------------------------
# Classification

CATEGORICAL = {"display", "position", "flex-direction", "writing-mode", "box-sizing", "float",
               "overflow", "flex-wrap", "grid-auto-flow"}


def value_kind(value: str) -> str:
    kinds = []
    value = value.replace(" solid #000", "")
    for token in re.findall(r"[a-z-]+\(|-?\d*\.?\d+(?:px|%|fr)?|[a-z-]+", value):
        if re.fullmatch(r"-?\d*\.?\d+px", token):
            kinds.append("px")
        elif token.endswith("%"):
            kinds.append("%")
        elif token.endswith("fr"):
            kinds.append("fr")
        elif re.fullmatch(r"-?\d*\.?\d+", token):
            kinds.append("n")
        elif token not in ("solid", "#000"):
            kinds.append(token)
    out = []
    for k in kinds:  # collapse runs
        if not out or out[-1] != k:
            out.append(k)
    return " ".join(out)


def signature(root: Node) -> tuple[str, str]:
    """(coarse, fine): coarse = element types + property names (+ categorical values);
    fine adds the kind of each value (px / % / keyword)."""
    tags, coarse, fine = set(), set(), set()
    for node in root.walk():
        tag = node.tag
        if tag == "span":
            tag = "span" if node.style.get("display") != "inline-block" else "inline-block"
        if tag != "div":
            tags.add(tag)
        for prop, value in node.style.items():
            if prop in CATEGORICAL:
                coarse.add(f"{prop}:{value}")
                fine.add(f"{prop}:{value}")
            else:
                name = re.sub(r"-(top|left|right|bottom)$", "", prop) if prop.startswith(("margin", "padding", "border")) else prop
                coarse.add(name)
                fine.add(f"{prop}:{value_kind(value)}")
        for attr in node.attrs:
            if attr != "src":
                coarse.add(f"@{attr}")
    head = ",".join(sorted(tags)) or "div"
    return f"{head} | " + "; ".join(sorted(coarse)), f"{head} | " + "; ".join(sorted(fine))


def node_to_json(node: Node) -> dict:
    return {"tag": node.tag, "id": node.id, "style": node.style, "attrs": node.attrs,
            "children": [node_to_json(c) for c in node.children]}


def node_from_json(data: dict) -> Node:
    return Node(data["tag"], data["id"], dict(data["style"]), dict(data["attrs"]),
                [node_from_json(c) for c in data["children"]])


def pretty_html(root: Node) -> str:
    """The minimal case as a short readable page (same rendering as to_html)."""
    return to_html(root)


# ---------------------------------------------------------------------------------------------
# Commands

def load_masks(args, out: Path | None = None, save: bool = False):
    """--mask for a new run; a run folder's config.json wins for an existing one."""
    masks = set(m for m in (getattr(args, "mask", "") or "").split(",") if m)
    if "all" in masks:
        masks = set(MASKS)
    unknown = masks - set(MASKS)
    if unknown:
        raise SystemExit(f"unknown mask(s) {unknown}; known: {', '.join(MASKS)}")
    config = out / "config.json" if out else None
    if config and config.exists():
        masks = set(json.loads(config.read_text())["masks"])
    elif config and save:
        config.write_text(json.dumps({"masks": sorted(masks)}))
    ACTIVE_MASKS.clear()
    ACTIVE_MASKS.update(masks)
    if masks:
        print(f"  masks: {', '.join(sorted(masks))}")


def cmd_run(args):
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    load_masks(args, out, save=True)
    results = out / "results.jsonl"
    done = set()
    if results.exists():
        for line in results.read_text(encoding="utf-8").splitlines():
            done.add(json.loads(line)["seed"])
    h = Harness(Path(args.exe))
    started = time.time()
    count = mism_pages = 0
    try:
        with open(results, "a", encoding="utf-8") as log:
            for seed in range(args.start, args.start + args.count):
                if seed in done:
                    continue
                if count and count % 250 == 0:
                    wait_for_memory()
                html = to_html(generate(seed))
                sw, edge = h.both(html)
                mism = compare(sw, edge, args.tol)
                flaky = False
                if mism and mism[0]["id"] != "crash":
                    again = compare(h.run_sw(html), edge, args.tol)
                    flaky = {m["id"] for m in again} != {m["id"] for m in mism}
                record = {"seed": seed, "n": html.count("data-id="), "mismatch": mism, "flaky": flaky}
                if edge["iw"] != WIDTH or ("iw" in sw and sw["iw"] != WIDTH):
                    record["viewport"] = [sw.get("iw"), sw.get("ih"), edge["iw"], edge["ih"]]
                log.write(json.dumps(record) + "\n")
                log.flush()
                count += 1
                mism_pages += bool(mism)
                if count % 100 == 0:
                    rate = count / (time.time() - started)
                    print(f"  {count} pages, {mism_pages} mismatching ({100 * mism_pages / count:.1f}%), "
                          f"{rate:.1f} pages/s", flush=True)
    finally:
        h.close()
    print(f"done: {count} pages, {mism_pages} mismatching")


def cmd_one(args):
    load_masks(args, Path(args.out) if args.out else None)
    root = generate(args.seed)
    html = to_html(root, f"seed {args.seed}")
    if args.save:
        Path(args.save).write_text(html, encoding="utf-8")
    h = Harness(Path(args.exe))
    try:
        sw, edge = h.both(html)
        print(html)
        for m in compare(sw, edge, args.tol):
            print(json.dumps(m))
    finally:
        h.close()


def cmd_case(args):
    h = Harness(Path(args.exe), edge=not args.no_edge)
    pages = []
    for path in map(Path, args.pages):  # a folder stands for the .html files in it
        pages += sorted(path.glob("*.html")) if path.is_dir() else [path]
    try:
        for path in pages:
            html = Path(path).read_text(encoding="utf-8")
            sw = h.run_sw(html)
            if args.no_edge:
                print(path, json.dumps(sw))
                continue
            edge = h.run_edge(html)
            mism = compare(sw, edge, args.tol)
            print(f"{path}: {'MATCH' if not mism else 'MISMATCH'}  (viewport sw {sw.get('iw')}x{sw.get('ih')} "
                  f"edge {edge['iw']}x{edge['ih']})")
            for m in mism:
                print("   ", json.dumps(m))
    finally:
        h.close()


def cmd_reduce(args):
    out = Path(args.out)
    cases_dir = Path(args.cases) / "cases"
    cases_dir.mkdir(parents=True, exist_ok=True)
    reduced_log = out / "reduced.jsonl"
    done = set()
    if reduced_log.exists():
        for line in reduced_log.read_text(encoding="utf-8").splitlines():
            done.add(json.loads(line)["seed"])
    load_masks(args, out)
    records = [json.loads(l) for l in (out / "results.jsonl").read_text(encoding="utf-8").splitlines()]
    todo = [r for r in records if r["mismatch"] and r["seed"] not in done]
    if args.seeds:
        wanted = {int(x) for x in Path(args.seeds).read_text().split()}
        todo = [r for r in todo if r["seed"] in wanted]
    elif args.limit:
        random.Random(1).shuffle(todo)
        todo = todo[:args.limit]
    print(f"reducing {len(todo)} pages", flush=True)
    h = Harness(Path(args.exe))
    try:
        with open(reduced_log, "a", encoding="utf-8") as log:
            for i, record in enumerate(todo):
                seed = record["seed"]
                root = generate(seed)
                html = to_html(root)
                mism = compare(*h.both(html), args.tol)
                if not mism:
                    log.write(json.dumps({"seed": seed, "status": "not-reproducible"}) + "\n")
                    continue
                anchors = {m["id"] if not m["id"].startswith("root-") else "root" for m in mism}
                small, tests, secs = reduce_case(h, root, anchors, args.tol, args.budget)
                small_html = to_html(small, f"seed {seed}")
                sw, edge = h.both(small_html)
                final = compare(sw, edge, args.tol)
                coarse, fine = signature(small)
                entry = {"seed": seed, "status": "reduced" if final else "lost", "tests": tests,
                         "seconds": round(secs, 1), "elements": sum(1 for _ in small.walk()),
                         "coarse": coarse, "fine": fine, "tree": node_to_json(small),
                         "mismatch": final, "edge": edge, "sw": sw, "original": mism}
                log.write(json.dumps(entry) + "\n")
                log.flush()
                print(f"  [{i + 1}/{len(todo)}] seed {seed}: {entry['elements']} elements, {tests} tests, "
                      f"{secs:.0f}s  {coarse}", flush=True)
    finally:
        h.close()
    cmd_classify(args)


def cmd_classify(args):
    out = Path(args.out)
    cases_root = Path(args.cases)
    cases_dir = cases_root / "cases"
    cases_dir.mkdir(parents=True, exist_ok=True)
    entries = [json.loads(l) for l in (out / "reduced.jsonl").read_text(encoding="utf-8").splitlines()]
    entries = [e for e in entries if e.get("status") == "reduced"]
    groups = defaultdict(list)
    for e in entries:
        groups[e["coarse"]].append(e)
    ranked = sorted(groups.items(), key=lambda kv: -len(kv[1]))
    summary = []
    for rank, (coarse, members) in enumerate(ranked, 1):
        best = min(members, key=lambda e: (e["elements"], len(to_html(node_from_json(e["tree"])))))
        name = f"g{rank:03d}"
        tree = node_from_json(best["tree"])
        html = to_html(tree, f"{name} seed {best['seed']}")
        (cases_dir / f"{name}.html").write_text(html, encoding="utf-8")
        (cases_dir / f"{name}.json").write_text(json.dumps({
            "group": name, "signature": coarse, "seed": best["seed"], "hits": len(members),
            "seeds": [m["seed"] for m in members], "mismatch": best["mismatch"],
            "edge": best["edge"], "sw": best["sw"],
            "fine_variants": Counter(m["fine"] for m in members).most_common()}, indent=1), encoding="utf-8")
        summary.append({"group": name, "hits": len(members), "signature": coarse, "seed": best["seed"]})
    (cases_root / "groups.json").write_text(json.dumps(summary, indent=1), encoding="utf-8")
    print(f"{len(entries)} reduced cases -> {len(ranked)} groups (written to {cases_dir})")
    for s in summary:
        print(f"  {s['group']}  {s['hits']:4d}  {s['signature']}")


def cmd_recheck(args):
    files = sorted(Path(args.cases).glob("**/cases/*.html")) + sorted((HERE / "known_cases").glob("*.html"))
    h = Harness(Path(args.exe), edge=args.edge)
    fixed = failing = 0
    saved = {}
    try:
        for path in files:
            name = path.relative_to(Path(args.cases)).as_posix() if path.is_relative_to(Path(args.cases))                 else path.name
            html = path.read_text(encoding="utf-8")
            meta_path = path.with_suffix(".json")
            sw = h.run_sw(html)
            if args.edge or not meta_path.exists():
                if not h.edge:
                    print(f"  {path.name}: no stored Edge result (use --edge)")
                    continue
                edge = h.run_edge(html)
            else:
                edge = json.loads(meta_path.read_text(encoding="utf-8"))["edge"]
            mism = compare(sw, edge, args.tol)
            saved[name] = mism
            if mism:
                failing += 1
                print(f"  STILL FAILING {name}: " + "; ".join(
                    f"#{m['id']} sw={m.get('sw')} edge={m.get('edge')}" for m in mism[:3]))
            else:
                fixed += 1
                print(f"  ok            {name}")
    finally:
        h.close()
        if args.save:
            Path(args.save).write_text(json.dumps(saved, indent=1), encoding="utf-8")
    print(f"{fixed} matching Edge, {failing} still different")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)

    def common(p, out=True):
        p.add_argument("--exe", required=True, help="Stillwater.exe to test")
        p.add_argument("--tol", type=float, default=1.0, help="px tolerance (default 1)")
        if out:
            p.add_argument("--out", default=str(SCRATCH / "run"), help="run folder (results.jsonl)")
            p.add_argument("--cases", default=str(DEFAULT_CASES), help="where minimal cases are saved")

    p = sub.add_parser("run", help="generate and compare a batch of pages")
    common(p)
    p.add_argument("--start", type=int, default=0)
    p.add_argument("--count", type=int, default=3000)
    p.add_argument("--mask", default="", help="comma-separated known bugs to avoid: " + ", ".join(MASKS)
                   + " (or all)")
    p.set_defaults(func=cmd_run)

    p = sub.add_parser("one", help="generate one page by seed, print it and its mismatches")
    common(p, out=False)
    p.add_argument("--seed", type=int, required=True)
    p.add_argument("--mask", default="", help="same masks as the run the seed came from")
    p.add_argument("--out", default="", help="or: the run folder (reads its config.json)")
    p.add_argument("--save", help="also write the page to this file")
    p.set_defaults(func=cmd_one)

    p = sub.add_parser("case", help="compare hand-written HTML files")
    common(p, out=False)
    p.add_argument("pages", nargs="+", help="HTML files, or folders of them")
    p.add_argument("--no-edge", action="store_true", help="only measure in Stillwater")
    p.set_defaults(func=cmd_case)

    p = sub.add_parser("reduce", help="reduce the mismatching pages of a run and group them")
    common(p)
    p.add_argument("--limit", type=int, default=0, help="reduce at most this many (random sample)")
    p.add_argument("--budget", type=float, default=180, help="seconds per page")
    p.add_argument("--seeds", help="file of seeds to reduce (whitespace-separated) instead of a sample")
    p.set_defaults(func=cmd_reduce)

    p = sub.add_parser("classify", help="(re)group already reduced cases")
    p.add_argument("--out", default=str(SCRATCH / "run"))
    p.add_argument("--cases", default=str(DEFAULT_CASES))
    p.set_defaults(func=cmd_classify)

    p = sub.add_parser("recheck", help="re-run saved minimal cases against a build")
    common(p, out=False)
    p.add_argument("--cases", default=str(DEFAULT_CASES))
    p.add_argument("--edge", action="store_true", help="re-measure Edge too instead of using stored rects")
    p.add_argument("--save", help="write {case: mismatches} as JSON here")
    p.set_defaults(func=cmd_recheck)

    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
