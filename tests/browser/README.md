# Browser tests

```sh
uv run --script tests/browser/run.py            # all of them
uv run --script tests/browser/run.py Smoke      # one case
```

The page under test is the real `adapters/dashboard.html` with its real
assets. Behind it is `server.py`: canned model, snapshot, history,
forecasts and holds, plus a WebSocket a test can push deltas down. No
supervisor, no bus, no clock — so a test can force a re-render at a chosen
moment, and states that take minutes to stage on a live house are a few
lines of JSON here.

## What this suite is, and is not

It is **not** the discovery mechanism. Every rendering bug this project has
had — the legend pin dying on a re-render, `&MIDDOT;` in an upper-cased
label, a spent forecast blanking a tile's caption, three key rebuilds that
dropped a segment — was found by a person opening a browser. That habit is
what finds the next one. Browser-verify your dashboard change.

What this carries is the boring half:

1. **A broad net.** No page errors, every view renders, every widget kind
   draws, at desktop and phone width. Better odds against a bug nobody has
   thought of than any hand-picked assertion.
2. **One regression per rule we have written down** — not per past bug.
   "What a reader chose survives a re-render only if it is held outside the
   markup" has five instances today; a test each turns the insight into a
   checklist item that stays checked.

## House rules for assertions

Assert through the **DOM** and the **network** only. The page's script is an
IIFE, so there are no internals to reach — which is the right discipline
anyway: everything asserted is something a person or another process could
observe.

- Prefer the attributes the event delegation already needs: `data-action`,
  `data-entity`, `data-aspect`, `data-layer`, `data-source`, and stable ids
  like `#paramrow-{unit}-{param}`.
- Match derived text by shape, never by wording: `/issued \d{2}:\d{2}/`,
  not `"issued 09:00"`. Copy changes; facts do not.
- Assert a tap by the request it makes, not by what the page draws next.

Do not assert: pixels or screenshots, whole-HTML snapshots, CSS beyond the
handful that is behaviour (`touch-action`), or chart path coordinates —
chart geometry is asserted numerically in `tests/js`.

## Fixtures

`fixtures/model.json` and `fixtures/snapshot.json` were captured from a
real supervised house and then extended to cover every widget kind. Their
time-bearing parts are re-stamped per request by `server.py`, because a
forecast frozen into a file is a spent one by tomorrow. A test that wants a
spent forecast or a lapsed hold pushes its own document with explicit
timestamps.

Fixtures drift. The canary test — a real house, booted once — is what
catches them ceasing to resemble what the unit actually emits.

## Checking the net itself

A net that cannot fail is indistinguishable from one that passes:

```sh
cp adapters/dashboard.html /tmp/broken.html   # then break something in it
HOMEOSTAT_TEST_PAGE=/tmp/broken.html uv run --script tests/browser/run.py Smoke
```
