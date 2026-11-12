# layoutfuzz

Differential layout fuzzer: generates small random pages, measures every element's
`getBoundingClientRect()` in Stillwater and in headless Microsoft Edge, and turns the pages that
disagree into minimal, grouped test cases.

- **Generator**: seeded, reproducible (`seed` → page). 2–12 elements, depth ≤ 4: block / flex /
  grid / inline-block / inline-flex / inline-grid / table (light) / `display:contents` containers;
  leaves are fixed-size boxes, fixed-size inline-blocks ("words"), `data:` SVG `<img>`s,
  `<canvas>`, `<textarea>`, `<input>`, `<button>`. Styles come from weighted pools: sizes
  (px, %, `min-content`/`max-content`/`fit-content`), min/max, `aspect-ratio`, padding, border,
  `box-sizing`, margins (auto, negative, %), positioning (relative/absolute/sticky/fixed + insets),
  flex and grid container/item properties, `gap`, alignment, `overflow`, floats, `vertical-align`,
  `writing-mode` (light).
- **Runner**: one Stillwater (via `../sitecheck/driver.py`: scratch profile, headless, muted) and
  one headless Edge (`../sitecheck/edge.py`, throwaway profile, DevTools on 127.0.0.1) stay alive
  for the whole run; pages are served from memory by a localhost HTTP server. Viewport 800×600,
  device scale factor 1, Edge scrollbars hidden. Runs at below-normal priority (inherited by both
  browsers). Stillwater is restarted every 1,500 pages and after a crash.
- **Comparator**: an element mismatches when x, y, width or height differ by more than `--tol`
  px (default 1). Non-atomic inlines (`display:inline` in Edge) are compared on x/width only.
- **Reducer**: delta-debugging over the element tree (drop subtrees, unwrap elements, drop each
  declaration and attribute) while one of the originally mismatching elements still disagrees.
- **Classifier**: groups reduced cases by element types + property names (with the values of
  `display`, `position`, `flex-direction`, `box-sizing`, `float`, `overflow`, `flex-wrap`,
  `grid-auto-flow`, `writing-mode`), ranked by how many random pages reduced into the group.

## Run a batch

```powershell
cd L:\servo\tools\layoutfuzz
$sw = "$env:TEMP\claude-sw-layoutfuzz\sw\Stillwater\Stillwater.exe"
python layoutfuzz.py run    --exe $sw --start 0 --count 3000 --out $env:TEMP\claude-sw-layoutfuzz\run
python layoutfuzz.py reduce --exe $sw --out $env:TEMP\claude-sw-layoutfuzz\run --limit 400
```

`run` writes `results.jsonl` (one line per page; re-running the same command resumes).
`reduce` writes `reduced.jsonl` next to it, then the groups: `cases/gNNN.html` (minimal page),
`cases/gNNN.json` (signature, seeds that hit it, both browsers' rects) and `groups.json` under
`--cases` (default `L:\servo-dev\perf\layoutfuzz`). `--limit N` reduces a random sample of N
mismatching pages (reduction costs a few seconds per page); `classify` regroups without re-running.
About 9 pages/s on the first run.

## Masks: steering around known bugs

A common bug swallows reduction: every page that contains it reduces to it. `run --mask a,b`
(or `--mask all`) makes the generator avoid known bugs, and the reducer is forbidden to undo a
mask. Masks: `writing-mode`, `aspect-ratio`, `svg-ratio` (viewBox-only SVG images),
`textarea-height` (textareas get a px height), `control-baseline` (controls get
`vertical-align:top`), `intrinsic-minmax` (no `min-content`/`max-content`/`fit-content` in
`min-*`/`max-*`), `empty-table` (the reducer keeps a cell in every table). Choose the masks for
the bugs that are known and not being fixed; `all` is every one of them. They are saved in the run folder's `config.json`, and `reduce`/`one --out`
reuse them, so seeds stay reproducible. `reduce --seeds FILE` re-reduces specific seeds.

## Re-run one seed

```powershell
python layoutfuzz.py one --exe $sw --seed 1234 --save seed1234.html   # prints the page and its mismatches
python layoutfuzz.py case --exe $sw seed1234.html other.html           # any hand-written pages
```

Hand-written pages should give each element to measure a `data-id` attribute. `case` also takes
folders, standing for the `.html` files in them.

## Regression suite

```powershell
python layoutfuzz.py recheck --exe path\to\new\Stillwater.exe          # vs the Edge rects stored with each case
python layoutfuzz.py recheck --exe path\to\new\Stillwater.exe --edge   # re-measure Edge too
```

`--save FILE` writes `{case: mismatches}` as JSON. Runs every `**/cases/*.html` under `--cases` plus `known_cases/*.html` (the two bugs fixed on 2026-11-11:
min-height column flex centring and textarea stretch) and prints which now match Edge.

## What it can't judge (by design)

- **Text**: pages have no text. The body is `font: 0/20px Arial`, so line boxes and the strut
  still exist but carry no font metrics (Chromium rounds ascent/descent to whole pixels, Servo
  doesn't: with 16px text every line box differed by 0.5–1.6px). Text shaping, line breaking of
  real text and font fallback are not tested.
- **Text-control intrinsic width** (`cols`/`size` × average character width, plus Chromium's
  scrollbar allowance): every `<textarea>`/`<input>` gets a px width and the reducer never
  removes it. Their heights (rows × 20px line-height + padding + border) and stretching in the
  block axis are tested. Controls keep 16px Arial, so their baselines carry font metrics (≤1px).
- **Inline box height**: `display:inline` boxes are compared on x/width only.
- Painting, overflow clipping, scroll positions, transforms: only box geometry is compared.
