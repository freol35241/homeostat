# The dashboard demo

A static copy of the family dashboard that runs entirely in the browser,
published on GitHub Pages: https://freol35241.github.io/homeostat/

The page is the real `adapters/dashboard.html` with its real assets.
`shim.js` answers in the page for the dashboard unit, using the browser
tests' fixture house (`tests/browser/fixtures`). It serves the model, the
WebSocket's snapshot and deltas, history, forecasts, logs and commands.
Commands are carried out locally, and the device's readback arrives a
moment later, as it would from a real device. There is no house behind
the page.

The demo uses the browser tests' fixtures because they are already kept
in step with what the dashboard unit emits (the drift canary in
`tests/browser/run.py`). The demo therefore cannot drift into describing
a house the dashboard no longer draws.

## Building it

```
scripts/build_demo_site.py _site      # then serve _site/ with any static server
```

The build makes these edits:

- Asset paths become relative, because Pages serves the site under
  `/homeostat/`.
- `demo/data.js` and `shim.js` load before the page's own scripts.
- The camera is dropped, because it needs a stream that a static page
  cannot provide.

The build fails if the page has changed in a way these edits no longer
fit. The Dashboard demo workflow publishes the site at each release, and
it can also be run by hand.

## Keeping the shim in step

`shim.js` is a second stand-in for the dashboard unit's endpoints, beside
`tests/browser/server.py`, so it can fall behind the page. Two checks keep
that out of the published demo. The shim logs a console error for any
request it has no answer for. The browser suite's `PagesDemo` case builds
the site and serves it under `/homeostat/` as Pages does. It then walks
every view and sends a command, and fails on any console error, page
error or failed request. If a change makes the page request something
new, CI fails on that change.

`examples/starter-house/demo` runs the whole house, adapters included,
against simulated devices.
