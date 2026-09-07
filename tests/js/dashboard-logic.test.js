// Tests for the dashboard's extracted decision logic
// (adapters/assets/dashboard-logic.js), run by `node --test tests/js` —
// Node's built-in runner, no packages. The DOM wiring in dashboard.html
// stays covered by the server-side suite (tests/dashboard.rs) plus hands.
'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');

const logic = require(path.join(__dirname, '../../adapters/assets/dashboard-logic.js'));

function model(overrides) {
  return Object.assign(
    {
      zones: {},
      units: [
        {
          name: 'evening_lights',
          label: 'Evening lights',
          params: {
            off_time: { type: 'time', default: '23:00', editable_by: 'family' },
            grace_minutes: { type: 'int', default: 5, editable_by: 'owner' },
          },
        },
        { name: 'zigbee', label: 'Zigbee', params: {} },
      ],
      entities: [
        { name: 'kitchen_lamp', room: 'kitchen', capability: 'light', label: 'Kitchen lamp' },
        { name: 'desk_lamp', room: 'office', capability: 'light', label: 'Desk lamp' },
        { name: 'front_door', room: 'hallway', capability: 'lock', label: 'Front door' },
        { name: 'gateway', room: 'hallway', capability: 'router', label: 'Gateway' },
        { name: 'site_tunnel', room: 'global', capability: 'vpn', label: 'Site tunnel' },
        { name: 'shed_temp', room: 'shed', capability: 'sensor', label: 'Shed temp' },
      ],
    },
    overrides || {}
  );
}

function deviations(state, health, config, m) {
  return logic.computeDeviations(m || model(), state || {}, health || {}, config || {});
}

test('an untouched house is in equilibrium', () => {
  const devs = deviations(
    { 'home/state/kitchen/kitchen_lamp/on': false },
    { 'home/health/zigbee': { status: 'running' } },
    { 'home/config/evening_lights/off_time': '23:00' } // equals the default
  );
  assert.deepEqual(devs, []);
});

test('a unit not running deviates with its restart detail', () => {
  const devs = deviations(null, {
    'home/health/zigbee': { status: 'backoff', restarts: 3, backoff_ms: 400 },
    'home/health/evening_lights': { status: 'running' },
  });
  assert.equal(devs.length, 1);
  assert.equal(devs[0].tag, 'supervision');
  assert.equal(devs[0].title, 'Zigbee');
  assert.equal(devs[0].detail, 'backoff — restarts: 3, backoff 400ms');
  assert.deepEqual(devs[0].target, { type: 'unit', unit: 'zigbee' });
});

test('lights on aggregate to one row with rooms and the corrective action', () => {
  const devs = deviations({
    'home/state/kitchen/kitchen_lamp/on': true,
    'home/state/office/desk_lamp/on': true,
  });
  assert.equal(devs.length, 1);
  assert.equal(devs[0].title, '2 lights on');
  assert.equal(devs[0].detail, 'Kitchen, Office');
  assert.deepEqual(devs[0].target, { type: 'rooms' });
  assert.deepEqual(devs[0].button, { action: 'lights-off', label: 'All off' });

  const one = deviations({ 'home/state/kitchen/kitchen_lamp/on': true });
  assert.equal(one[0].title, '1 light on', 'singular form');
});

test('an unlocked lock deviates; a locked or unknown one does not', () => {
  const devs = deviations({ 'home/state/hallway/front_door/locked': false });
  assert.equal(devs.length, 1);
  assert.equal(devs[0].title, 'Front door unlocked');
  assert.deepEqual(devs[0].target, { type: 'entity', room: 'hallway', entity: 'front_door' });

  assert.deepEqual(deviations({ 'home/state/hallway/front_door/locked': true }), []);
  assert.deepEqual(deviations({}), [], 'no state at all: stale, never false');
});

test('WAN and VPN down are connectivity deviations', () => {
  const devs = deviations({
    'home/state/hallway/gateway/wan': false,
    'home/state/global/site_tunnel/up': false,
  });
  assert.deepEqual(devs.map((d) => d.title), ['Gateway — WAN down', 'Site tunnel down']);
});

test('available === false deviates for any capability; stale is not false', () => {
  const devs = deviations({
    'home/state/shed/shed_temp/available': false,
    'home/state/kitchen/kitchen_lamp/available': true,
  });
  assert.equal(devs.length, 1);
  assert.equal(devs[0].title, 'Shed temp unresponsive');
  assert.deepEqual(devs[0].target, { type: 'entity', room: 'shed', entity: 'shed_temp' });
});

test('a live setpoint off its manifest default deviates; matching or unserved does not', () => {
  const off = deviations(null, null, { 'home/config/evening_lights/off_time': '21:30' });
  assert.equal(off.length, 1);
  assert.equal(off[0].tag, 'setpoint');
  assert.equal(off[0].title, 'Evening lights · off_time');
  assert.equal(off[0].detail, '21:30 (default 23:00)');
  assert.deepEqual(off[0].target, { type: 'setpoint', unit: 'evening_lights', param: 'off_time' });

  assert.deepEqual(deviations(null, null, { 'home/config/evening_lights/off_time': '23:00' }), []);
  assert.deepEqual(deviations(null, null, {}), [], 'no served value: no verdict');
});

test('an owner param off its default deviates too, tapping to the unit rather than an editor', () => {
  const off = deviations(null, null, { 'home/config/evening_lights/grace_minutes': 10 });
  assert.equal(off.length, 1);
  assert.equal(off[0].tag, 'setpoint');
  assert.equal(off[0].detail, '10 (default 5)');
  assert.deepEqual(off[0].target, { type: 'unit', unit: 'evening_lights' });
});

test('deviations render in a stable order: supervision, state, setpoints', () => {
  const devs = deviations(
    {
      'home/state/kitchen/kitchen_lamp/on': true,
      'home/state/hallway/front_door/locked': false,
      'home/state/shed/shed_temp/available': false,
    },
    { 'home/health/zigbee': { status: 'open' } },
    { 'home/config/evening_lights/off_time': '21:30' }
  );
  assert.deepEqual(
    devs.map((d) => d.tag),
    ['supervision', 'state', 'state', 'state', 'setpoint']
  );
});

// ---- applyMessage ----

function freshStore() {
  return { state: {}, health: {}, config: {}, events: [] };
}

test('a snapshot replaces the maps wholesale', () => {
  const store = freshStore();
  store.state['home/state/a/b/c'] = 1;
  const applied = logic.applyMessage(store, {
    type: 'snapshot',
    state: { 'home/state/x/y/z': 2 },
    health: { 'home/health/u': { status: 'running' } },
  });
  assert.equal(applied, 'snapshot');
  assert.deepEqual(store.state, { 'home/state/x/y/z': 2 });
  assert.deepEqual(store.config, {}, 'a snapshot without config clears it');
});

test('deltas set single keys and report their type', () => {
  const store = freshStore();
  assert.equal(logic.applyMessage(store, { type: 'state', key: 'k', value: 7 }), 'state');
  assert.equal(logic.applyMessage(store, { type: 'config', key: 'c', value: 'v' }), 'config');
  assert.equal(logic.applyMessage(store, { type: 'health', key: 'h', value: { status: 'open' } }), 'health');
  assert.equal(store.state.k, 7);
  assert.equal(store.config.c, 'v');
  assert.deepEqual(store.health.h, { status: 'open' });
});

test('events prepend, stamp a missing ts, and cap at 200', () => {
  const store = freshStore();
  logic.applyMessage(store, { type: 'event', key: 'e1', value: {}, ts: 111 }, 999);
  logic.applyMessage(store, { type: 'event', key: 'e2', value: {} }, 999);
  assert.equal(store.events[0].key, 'e2', 'newest first');
  assert.equal(store.events[0].ts, 999, 'stamped from the clock argument');
  assert.equal(store.events[1].ts, 111, 'a carried ts wins');
  for (let i = 0; i < 250; i++) {
    logic.applyMessage(store, { type: 'event', key: 'e' + i, value: {} }, 999);
  }
  assert.equal(store.events.length, 200);
});

test('unrecognized messages apply nothing', () => {
  const store = freshStore();
  assert.equal(logic.applyMessage(store, null), null);
  assert.equal(logic.applyMessage(store, {}), null);
  assert.equal(logic.applyMessage(store, { type: 'mystery', key: 'k' }), null);
  assert.deepEqual(store, freshStore());
});

// ---- presence helpers ----

test('presence keys parse for both aspect spellings, off-schema keys do not', () => {
  assert.equal(logic.presenceEntityFromKey('home/state/hall/sensor1/occupancy'), 'hall/sensor1');
  assert.equal(logic.presenceEntityFromKey('home/state/global/phone/presence'), 'global/phone');
  assert.equal(logic.presenceEntityFromKey('home/state/hall/sensor1/temperature'), null);
  assert.equal(logic.presenceEntityFromKey('home/cmd/hall/sensor1/occupancy'), null);
  assert.equal(logic.presenceEntityFromKey('home/state/short'), null);
});

test('presenceValue prefers occupancy, falls back to presence', () => {
  const entity = { room: 'hall', name: 'sensor1' };
  assert.equal(
    logic.presenceValue({ 'home/state/hall/sensor1/occupancy': true }, entity),
    true
  );
  assert.equal(
    logic.presenceValue({ 'home/state/hall/sensor1/presence': false }, entity),
    false
  );
  assert.equal(logic.presenceValue({}, entity), undefined);
});
