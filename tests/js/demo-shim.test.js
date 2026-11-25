// Tests for the dashboard demo's stand-in for the dashboard unit
// (demo-site/shim.js): its answers have the shapes the unit's routes have.
// The shim is a classic browser script, so it runs here in a vm context
// with just enough of a window for it to install its fetch.
'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../../demo-site/shim.js'), 'utf8');

function loadShim() {
  const window = {
    HOMEOSTAT_DEMO: { model: { entities: [] }, snapshot: { state: {} } },
    fetch: () => Promise.reject(new Error('the shim passed a request through')),
  };
  const context = vm.createContext({
    window,
    location: { href: 'http://demo.test/homeostat/' },
    document: { addEventListener() {} },
    console,
    URL,
    Response,
    Date,
    // The shim's ticking house would keep the test process alive.
    setInterval() {},
    setTimeout() {},
  });
  vm.runInContext(source, context);
  return window;
}

test('/api/logs answers a bare list of the core log entries', async () => {
  const window = loadShim();
  const response = await window.fetch('/homeostat/api/logs?unit=weather&lines=100');
  const body = await response.json();
  assert.ok(Array.isArray(body), 'the unit answers a list, and the page ignores anything else');
  assert.ok(body.length > 0);
  for (const entry of body) {
    // The fields of the core's LogEntry (src/bus.rs), which the unit passes on.
    assert.deepEqual(Object.keys(entry).sort(), ['line', 'stream', 'ts_us']);
    assert.equal(typeof entry.ts_us, 'number');
    assert.ok(Math.abs(entry.ts_us / 1000 - Date.now()) < 60e3, 'ts_us is epoch microseconds');
    assert.ok(entry.stream === 'stdout' || entry.stream === 'stderr');
    assert.equal(typeof entry.line, 'string');
  }
});
