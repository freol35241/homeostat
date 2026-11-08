// Day bars under a zone with daylight saving. The zone is set before
// anything reads the clock, so this file runs apart from the others,
// which `node --test` does by giving each file its own process.
'use strict';
process.env.TZ = 'Europe/Stockholm';
const test = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');

const logic = require(path.join(__dirname, '../../adapters/assets/dashboard-logic.js'));

const HOUR = 3600e3;
const iso = (ms) => new Date(ms).toISOString();

test('a day bar runs from local midnight to local midnight, 25 h on the night the clocks go back', () => {
  // Hourly 1.0 from 22:00Z on 23 October (local midnight) for three local
  // days; 25 October has 25 hours in Stockholm.
  const start = Date.UTC(2026, 9, 23, 22);
  const rows = Array.from({ length: 73 }, (_, i) => ({ ts: iso(start + i * HOUR), value: 1 }));
  const week = { from: start, to: start + 7 * 24 * HOUR };
  const bars = logic.intervalBars(rows, 3600, week, null);
  bars.forEach((b) => assert.equal(new Date(b.from).getHours(), 0));
  const long = bars.find((b) => b.to - b.from === 25 * HOUR);
  assert.ok(long, 'one 25 h day');
  assert.equal(long.observed, 25);
  assert.equal(bars.reduce((a, b) => a + b.observed, 0), 73, 'every hour lands in one day');
});

test('a daily total stamped at local midnight is one bar on its own day', () => {
  // Pellets per day, stamped at local midnight, which is 23:00Z in winter.
  const day = Date.UTC(2026, 10, 6, 23);
  const rows = [{ ts: iso(day), value: 12 }, { ts: iso(day + 24 * HOUR), value: 9 }];
  const bars = logic.intervalBars(rows, 86400, { from: day - 3 * 24 * HOUR, to: day + 2 * 24 * HOUR }, null);
  assert.deepEqual(bars.map((b) => [new Date(b.from).getDate(), b.observed]), [[7, 12], [8, 9]]);
});

test('the first day bar is the whole day, not the part inside the window', () => {
  const midnight = Date.UTC(2026, 10, 1, 23);
  const rows = Array.from({ length: 24 * 5 }, (_, i) => ({ ts: iso(midnight + i * HOUR), value: 0.5 }));
  const win = { from: midnight + 12 * HOUR, to: midnight + 4 * 24 * HOUR };
  const bars = logic.intervalBars(rows, 3600, win, null);
  assert.deepEqual([bars[0].from, bars[0].observed], [midnight, 12]);
});
