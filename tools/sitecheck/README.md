# sitecheck

Sweeps websites through Stillwater (Servo) and explains why the ones that don't work fail.

Each site loads headlessly in its own isolated Stillwater, with its own log, cache and profile,
driven over WebDriver. A probe script injected into every page (`probe/probe.js`) collects
script errors from the start, and once the page has loaded sitecheck asks it what the page
shows and takes a screenshot. Stillwater's output supplies network failures, rejected CSS and
crashes. A site takes 6 to 10 seconds unless it's slow to load.

A page that keeps Stillwater busy makes the probe wait: some real sites run one script for
40 s. Stillwater stops scripts that run for over 60 s without a break
(`STILLWATER_SLOW_SCRIPT_SECONDS`), so a page stuck in an endless loop can't stall a run. `--legacy` uses the old method instead: Stillwater's `-o`
screenshot and the probe's console output, with every site taking the full `--seconds`.

## Run it

Build Stillwater first (`./mach build --release`), then from the repo root:

```powershell
python tools/sitecheck/sitecheck.py                         # the whole catalog (sites.json)
python tools/sitecheck/sitecheck.py --category video        # one category
python tools/sitecheck/sitecheck.py --site https://example.com/   # one site (e.g. from a report)
python tools/sitecheck/sitecheck.py --label after --compare before  # diff two runs
python tools/sitecheck/sitecheck.py --jobs 8 -- --pref=dom_webgl2_enabled  # pass Stillwater flags
```

Results go to `tools/sitecheck/out/<label>/`: `report.html` (screenshots and diagnoses) and
`results.json`.

## What the statuses mean

| Status | Meaning |
|---|---|
| Works | Loaded and rendered |
| Works, slowly | Took more than 10 s to finish loading |
| Partly works | Renders, but scripts fail (often a missing web feature) |
| Broken | Very little content plus script errors |
| Layout broken | Something on the page is far wider than the window, usually from unsupported CSS |
| Blank page | Nothing visible rendered |
| Blocked by a browser check | An anti-bot or "unsupported browser" page |
| Network failure | The page itself failed to download (DNS, TLS, HTTP/2…) |
| Crashed | Stillwater panicked |

The report also ranks, across all sites:

- the **missing web features** guessed from script errors, and
- the **unsupported CSS**: every declaration, selector and media query Stillwater rejected
  (vendor-prefixed hacks left out). Rejected CSS is silently ignored, so it explains pages that
  load fine but look wrong.

Fixing the top entries in Servo helps the most sites.

Screenshots are taken 6 s after the page loads (`--shot-delay`), since many pages fade their
content in.

## Driving Stillwater from your own tests

`driver.py` is the small WebDriver client sitecheck uses, with no dependencies:

```python
from driver import Stillwater

with Stillwater(exe, profile_dir, log_file) as browser:
    browser.navigate("https://example.com/")
    print(browser.execute("return document.title"))
    browser.screenshot("shot.png")
```

Stillwater speaks standard W3C WebDriver (Servo's implementation), so other clients such as
Selenium or WebdriverIO should work too, though only `driver.py` has been tried: start it with `--webdriver=4444` (add `--headless` for no window) and connect to
`http://127.0.0.1:4444`. Set `STILLWATER_CONFIG_DIR` to a scratch folder so tests don't touch
your own profile.

## Comparing with Edge

`compare.py` checks whether sites look the same in Stillwater as in Microsoft Edge, to find what's
"slightly off" without having to describe it:

```powershell
python tools/sitecheck/compare.py --label c1                 # every site in sites.json
python tools/sitecheck/compare.py --category news --jobs 3
python tools/sitecheck/compare.py --site https://example.com/
```

For each site a hidden Edge (`edge.py`: headless, muted, 127.0.0.1 only, a throwaway profile) loads
the page, then Stillwater, then Edge again, all at 1280×800 in light mode. The same script
(`ports/servoshell/desktop/compare-collect.js`) measures every visible element in both browsers:
box, text size, weight and colours. Whatever differs between the two Edge loads is treated as
changing content and ignored. The report (`out/compare-<label>/report.html`) ranks sites by how
different they look and shows each with a slider between the two browsers, a heat map and the
differences in words. A move or size change is reported once, not again for every element inside
or after it. Ad blocking is off in Stillwater for these runs, since Edge has none.

Stillwater has the same comparison for the page you're looking at: **Compare with Edge**
(<kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>E</kbd>).

## Broken-site reports

Stillwater's **Report a broken site** menu item opens a pre-filled GitHub issue with the site's
address, the Stillwater version and the page's recent script errors. To investigate one, run
`sitecheck --site <address>` and open its report.
