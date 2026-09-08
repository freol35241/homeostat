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

// ---- aspect descriptors ----

const HEAT_PUMP = { name: 'heat_pump', room: 'utility', capability: 'climate', label: 'Heat pump' };

function descriptor() {
  return {
    schema: 1,
    groups: ['control', 'readings'],
    fields: {
      setpoint: {
        label: 'indoor target', kind: 'temperature', group: 'control',
        command: { type: 'float', constraint: { min: 10, max: 30 }, step: 0.5, editable_by: 'family' },
      },
      operating_mode: {
        label: 'mode', kind: 'enum', group: 'control',
        values: [{ value: 1, label: 'normal' }, { value: 2, label: 'block' }, { value: 3, label: 'boost' }],
        command: { type: 'enum', editable_by: 'family' },
      },
      feed_temperature_target: {
        label: 'feed target', kind: 'temperature', group: 'control',
        command: { type: 'float', constraint: { min: 20, max: 60 }, editable_by: 'owner' },
      },
      indoor_temperature: { label: 'indoor', kind: 'temperature', group: 'readings', valid: 'indoor_temperature_valid' },
      compressor: { label: 'compressor', kind: 'boolean', group: 'readings' },
      alarm: { label: 'alarm', kind: 'boolean', group: 'readings', notable: true },
      never_published: { label: 'ghost', kind: 'number', group: 'readings' },
    },
  };
}

function heatPumpState(overrides) {
  return Object.assign({
    'home/state/utility/heat_pump/setpoint': 21,
    'home/state/utility/heat_pump/operating_mode': 2,
    'home/state/utility/heat_pump/feed_temperature_target': 38,
    'home/state/utility/heat_pump/indoor_temperature': 20.3,
    'home/state/utility/heat_pump/indoor_temperature_valid': false,
    'home/state/utility/heat_pump/compressor': true,
    'home/state/utility/heat_pump/GT3_2_raw': 47.25,
    'home/state/utility/heat_pump/available': true,
    'home/state/kitchen/kitchen_lamp/on': true,
  }, overrides || {});
}

test('a described entity plans sections in descriptor order, diagnostics last and collapsed', () => {
  const plan = logic.aspectPlan(HEAT_PUMP, heatPumpState(), descriptor(), true);
  assert.deepEqual(plan.map((s) => s.group), ['control', 'readings', 'diagnostics']);
  assert.deepEqual(plan.map((s) => s.collapsed), [false, false, true]);
  assert.deepEqual(plan[0].rows.map((r) => r.aspect), ['setpoint', 'operating_mode', 'feed_temperature_target']);
  // undescribed aspects fall to diagnostics, sorted; a foreign entity's keys never appear
  assert.deepEqual(plan[2].rows.map((r) => r.aspect), ['GT3_2_raw', 'available']);
  // a described field with no state is not a row
  assert.ok(!plan[1].rows.some((r) => r.aspect === 'never_published'));
});

test('described rows carry labels, kind formatting and the consumed validity flag', () => {
  const plan = logic.aspectPlan(HEAT_PUMP, heatPumpState(), descriptor(), true);
  const readings = plan[1].rows;
  const indoor = readings.find((r) => r.aspect === 'indoor_temperature');
  assert.equal(indoor.label, 'indoor');
  assert.equal(indoor.display, '20.3°');
  assert.equal(indoor.stale, true, 'valid === false marks the reading stale');
  assert.ok(!readings.some((r) => r.aspect === 'indoor_temperature_valid'), 'the flag is consumed, not listed');
  assert.equal(readings.find((r) => r.aspect === 'compressor').display, 'on');
  const mode = plan[0].rows.find((r) => r.aspect === 'operating_mode');
  assert.equal(mode.display, 'block', 'enum values render their label');
  assert.equal(plan[2].rows.find((r) => r.aspect === 'GT3_2_raw').display, '47.3', 'undescribed: one decimal');
});

test('controls follow the command type and tier; ungranted renders inert', () => {
  const granted = logic.aspectPlan(HEAT_PUMP, heatPumpState(), descriptor(), true)[0].rows;
  assert.deepEqual(granted[0].control, { kind: 'stepper', step: 0.5, min: 10, max: 30, disabled: false });
  assert.equal(granted[1].control.kind, 'segment');
  assert.equal(granted[1].control.values.length, 3);
  assert.deepEqual(granted[2].control, { kind: 'readonly', tier: 'owner' }, 'owner commands read, never write');
  const inert = logic.aspectPlan(HEAT_PUMP, heatPumpState(), descriptor(), false)[0].rows;
  assert.equal(inert[0].control.disabled, true);
  assert.equal(inert[1].control.disabled, true);
  assert.equal(logic.controlFor({ command: { type: 'int', constraint: { min: 0, max: 5 }, editable_by: 'family' } }, true).kind, 'slider');
  assert.equal(logic.controlFor({ label: 'x' }, true), null, 'no command, no control');
});

test('an undescribed entity plans one flat state section, as before', () => {
  const plan = logic.aspectPlan(HEAT_PUMP, heatPumpState(), undefined, true);
  assert.equal(plan.length, 1);
  assert.equal(plan[0].group, 'state');
  assert.equal(plan[0].collapsed, false);
  assert.deepEqual(plan[0].rows.map((r) => r.aspect), [
    'GT3_2_raw', 'available', 'compressor', 'feed_temperature_target', 'indoor_temperature',
    'indoor_temperature_valid', 'operating_mode', 'setpoint',
  ]);
  assert.equal(plan[0].rows.find((r) => r.aspect === 'compressor').display, 'true');
  assert.equal(plan[0].rows.find((r) => r.aspect === 'indoor_temperature').display, '20.3°');
  assert.equal(plan[0].rows[0].control, null);
});

test('formatAspect handles the kinds and the empty value', () => {
  assert.equal(logic.formatAspect('x', { kind: 'temperature_delta' }, 1.5), '+1.5°');
  assert.equal(logic.formatAspect('x', { kind: 'temperature_delta' }, -2), '-2.0°');
  assert.equal(logic.formatAspect('x', { kind: 'percent' }, 87.6), '88%');
  assert.equal(logic.formatAspect('x', { kind: 'number' }, 3.14159), '3.14');
  assert.equal(logic.formatAspect('x', null, undefined), '—');
});

test('a notable described aspect deviates when true', () => {
  const m = model({ entities: [HEAT_PUMP] });
  const aspects = { heat_pump: descriptor() };
  const on = logic.computeDeviations(m, heatPumpState({ 'home/state/utility/heat_pump/alarm': true }), {}, {}, aspects);
  assert.equal(on.length, 1);
  assert.equal(on[0].title, 'Heat pump — alarm');
  assert.deepEqual(on[0].target, { type: 'entity', room: 'utility', entity: 'heat_pump' });
  assert.deepEqual(logic.computeDeviations(m, heatPumpState({ 'home/state/utility/heat_pump/alarm': false }), {}, {}, aspects), []);
  assert.deepEqual(logic.computeDeviations(m, heatPumpState({ 'home/state/utility/heat_pump/alarm': true }), {}, {}, {}), [], 'undescribed: no verdict');
});

test('aspect descriptors ride the snapshot and arrive as deltas', () => {
  const store = { state: {}, health: {}, config: {}, events: [], aspects: {} };
  logic.applyMessage(store, { type: 'snapshot', state: {}, health: {}, config: {}, aspects: { heat_pump: { fields: {} } } });
  assert.deepEqual(Object.keys(store.aspects), ['heat_pump']);
  assert.equal(logic.applyMessage(store, { type: 'aspects', entity: 'lamp', value: { fields: { on: {} } } }), 'aspects');
  assert.deepEqual(Object.keys(store.aspects).sort(), ['heat_pump', 'lamp']);
  logic.applyMessage(store, { type: 'snapshot', state: {} });
  assert.deepEqual(store.aspects, {}, 'a snapshot without descriptors clears them');
});
