# The dashboard demo

A static copy of the family dashboard that runs entirely in the browser,
published on GitHub Pages: https://freol35241.github.io/homeostat/

The page is the real `adapters/dashboard.html` with its real assets. What
the dashboard unit would answer — the model, the WebSocket's snapshot and
deltas, history, forecasts, logs, commands — is answered in the page by
`shim.js`, from the browser tests' fixture house (`tests/browser/fixtures`).
Commands are "obeyed" locally: the device's readback arrives a moment
later, as it would from a real one. Nothing reaches a house; there is none.

Reusing the browser tests' fixtures is deliberate: they are already kept
in step with what the dashboard unit emits (the drift canary in
`tests/browser/run.py`), so the demo cannot quietly describe a house the
dashboard no longer draws.

## Building it

```
scripts/build_demo_site.py _site      # then serve _site/ with any static server
```

The build makes asset paths relative (Pages serves the site under
`/homeostat/`), loads `demo/data.js` and `shim.js` ahead of the page's own
scripts, and drops what a static page cannot show — the camera, which
needs a stream — and refuses to build if the page has changed in a way
those edits no longer fit. CI builds it on every change; the Dashboard
demo workflow publishes it with each release, or when run by hand.

The live counterpart — the whole house, adapters and all, against
simulated devices — is `examples/starter-house/demo`.
