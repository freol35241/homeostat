# Browser tests

```sh
uv run --script tests/browser/run.py            # all of them
uv run --script tests/browser/run.py Smoke      # one case
```

The page under test is the real `adapters/dashboard.html` with its real
assets. Behind it is `server.py`. It serves a canned model, snapshot,
history, forecasts and holds, and a WebSocket that a test can push deltas
down. There is no supervisor, bus or clock. A test can therefore force a
re-render at a chosen moment. A state that takes minutes to set up on a
live house is a few lines of JSON here.

## What this suite covers

This suite does not find most rendering bugs. People find them by opening
the page in a browser. Examples are the legend pin lost on a re-render,
`&MIDDOT;` in an upper-cased label, a spent forecast blanking a tile's
caption, and a key rebuild that drops a segment. Check your dashboard
change in a browser.

The suite covers two things:

1. A broad check. There are no page errors, every view renders, and every
   widget kind draws, at desktop and phone width. This catches bugs that
   no hand-picked assertion anticipates.
2. One regression test per written-down rule, rather than one per past
   bug. For example, "a choice the reader made survives a re-render only
   if it is held outside the markup" has five instances, and each has a
   test.

## Rules for assertions

Assert through the DOM and the network only. The page's scripts are ES
modules that put nothing on `window`, so there are no internals to reach.
Everything a test asserts is something a person or another process could
observe.

- Prefer the attributes the event delegation already needs: `data-action`,
  `data-entity`, `data-aspect`, `data-layer`, `data-source`, and stable ids
  like `#paramrow-{unit}-{param}`.
- Match derived text by its pattern, not its wording: `/issued \d{2}:\d{2}/`
  rather than `"issued 09:00"`. Wording changes more often than the facts
  it reports.
- Assert a tap by the request it makes, not by what the page draws next.

Do not assert pixels or screenshots, whole-HTML snapshots, or chart path
coordinates. Do not assert CSS, apart from the few properties that are
behaviour (`touch-action`). `tests/js` checks chart geometry numerically.

## Fixtures

`fixtures/model.json` and `fixtures/snapshot.json` are a real supervised
house's model and snapshot, extended to cover every widget kind.
`server.py` re-stamps their time-bearing parts on each request, because a
forecast stored in a file would be spent by the next day. A test that
wants a spent forecast or a lapsed hold pushes its own document with
explicit timestamps.

Fixtures can drift from the real unit. The canary test boots a real house
once and checks that `model.json` has the same fields as the unit's
`/api/model`. Nothing checks `snapshot.json` that way, so keep it to
shapes the unit emits.

## Checking that the tests can fail

Break the page on purpose and confirm the suite reports it:

```sh
cp adapters/dashboard.html /tmp/broken.html   # then break something in it
HOMEOSTAT_TEST_PAGE=/tmp/broken.html uv run --script tests/browser/run.py Smoke
```
