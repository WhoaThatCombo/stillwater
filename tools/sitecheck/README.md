# sitecheck

Sweeps websites through Stillwater (Servo) and explains why the ones that don't work fail.

Each site loads headlessly in its own isolated Stillwater, with its own log, cache and profile.
A probe script injected into every page (`probe/probe.js`) reports what the page actually shows,
and Stillwater's output supplies network failures, script errors and crashes.

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

## Broken-site reports

Stillwater's **Report a broken site** menu item opens a pre-filled GitHub issue with the site's
address, the Stillwater version and the page's recent script errors. To investigate one, run
`sitecheck --site <address>` and open its report.
