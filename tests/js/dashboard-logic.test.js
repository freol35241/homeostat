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

test('personStatus reads presence and the last fix, and nothing else', () => {
  const alice = { room: 'person', name: 'alice', capability: 'person' };
  assert.deepEqual(
    logic.personStatus({ 'home/state/person/alice/presence': true, 'home/state/person/alice/fixed_at': 1700000000 }, alice),
    { home: true, seenAt: 1700000000000 }
  );
  assert.deepEqual(
    logic.personStatus({ 'home/state/person/alice/presence': false }, alice),
    { home: false, seenAt: undefined }
  );
  // A fix without a presence aspect is not "away": home stays unknown.
  assert.deepEqual(
    logic.personStatus({ 'home/state/person/alice/fixed_at': 1700000000, 'home/state/person/alice/lat': 59.3 }, alice),
    { home: undefined, seenAt: 1700000000000 }
  );
  assert.deepEqual(logic.personStatus({}, alice), { home: undefined, seenAt: undefined });
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
  assert.deepEqual(plan[2].rows.map((r) => r.aspect), ['available', 'GT3_2_raw']);
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
  assert.deepEqual(granted[0].control, { kind: 'dial', step: 0.5, min: 10, max: 30, disabled: false }, 'a stepped temperature is a dial');
  assert.equal(granted[1].control.kind, 'segment');
  assert.equal(granted[1].control.values.length, 3);
  assert.deepEqual(granted[2].control, { kind: 'readonly', tier: 'owner' }, 'owner commands read, never write');
  const inert = logic.aspectPlan(HEAT_PUMP, heatPumpState(), descriptor(), false)[0].rows;
  assert.equal(inert[0].control.disabled, true);
  assert.equal(inert[1].control.disabled, true);
  assert.equal(logic.controlFor({ command: { type: 'int', constraint: { min: 0, max: 5 }, editable_by: 'family' } }, true).kind, 'slider');
  assert.equal(logic.controlFor({ label: 'x' }, true), null, 'no command, no control');
});

test('richer controls: a select past four values, a stepper for non-temperatures, coarse slider steps', () => {
  const values = (n) => Array.from({ length: n }, (_, i) => ({ value: i, label: 'v' + i }));
  const enumOf = (n) => logic.controlFor({ kind: 'enum', values: values(n), command: { type: 'enum', editable_by: 'family' } }, true);
  assert.equal(enumOf(4).kind, 'segment');
  assert.equal(enumOf(5).kind, 'select');
  assert.equal(logic.controlFor({ kind: 'number', command: { type: 'float', step: 1, editable_by: 'family' } }, true).kind, 'stepper');
  assert.equal(logic.controlFor({ kind: 'temperature', command: { type: 'float', step: 0.5, editable_by: 'family' } }, true).kind, 'stepper',
    'a temperature without bounds has no arc: a stepper');
  const pct = logic.controlFor({ kind: 'percent', command: { type: 'float', constraint: { min: 0, max: 100 }, editable_by: 'family' } }, true);
  assert.equal(pct.kind, 'slider');
  assert.equal(pct.coarse, 5, 'a percent nudges by five');
  assert.equal(logic.coarseStep(0, 5, 1), 1, 'never finer than an integer step');
  assert.equal(logic.coarseStep(150, 500, 0), 20, 'mireds nudge by a round twenty');
  assert.equal(logic.coarseStep(0, 1, 0), 0.1);
});

test('an undescribed entity plans one flat state section, as before', () => {
  const plan = logic.aspectPlan(HEAT_PUMP, heatPumpState(), undefined, true);
  assert.equal(plan.length, 1);
  assert.equal(plan[0].group, 'state');
  assert.equal(plan[0].collapsed, false);
  assert.deepEqual(plan[0].rows.map((r) => r.aspect), [
    'available', 'indoor_temperature', 'GT3_2_raw', 'compressor', 'feed_temperature_target',
    'operating_mode', 'setpoint',
  ]);
  assert.equal(plan[0].rows.find((r) => r.aspect === 'compressor').display, 'true');
  assert.equal(plan[0].rows.find((r) => r.aspect === 'indoor_temperature').display, '20.3°');
  assert.equal(plan[0].rows[0].control, null);
});

test('available and _valid flags are schema vocabulary: rendered without a descriptor', () => {
  // undescribed: available is a boolean row, the _valid flag folds into its reading
  const flat = logic.aspectPlan(HEAT_PUMP, heatPumpState(), undefined, true)[0].rows;
  assert.equal(flat.find((r) => r.aspect === 'available').display, 'on');
  const indoor = flat.find((r) => r.aspect === 'indoor_temperature');
  assert.equal(indoor.stale, true);
  assert.ok(!flat.some((r) => r.aspect === 'indoor_temperature_valid'), 'consumed, not listed');
  // described with a status group: available lands there; without one, in diagnostics
  const withStatus = Object.assign(descriptor(), { groups: ['control', 'readings', 'status'] });
  const plan = logic.aspectPlan(HEAT_PUMP, heatPumpState(), withStatus, true);
  assert.deepEqual(plan.find((s) => s.group === 'status').rows.map((r) => r.aspect), ['available']);
  const noStatus = logic.aspectPlan(HEAT_PUMP, heatPumpState(), descriptor(), true);
  assert.ok(noStatus.find((s) => s.group === 'diagnostics').rows.some((r) => r.aspect === 'available'));
  // an adapter that describes available itself wins
  const own = descriptor(); own.fields.available = { label: 'reachable', kind: 'boolean', group: 'readings' };
  const ownPlan = logic.aspectPlan(HEAT_PUMP, heatPumpState(), own, true);
  assert.equal(ownPlan.find((s) => s.group === 'readings').rows.find((r) => r.aspect === 'available').label, 'reachable');
});

test('formatAspect handles the kinds and the empty value', () => {
  assert.equal(logic.formatAspect('x', { kind: 'temperature_delta' }, 1.5), '+1.5°');
  assert.equal(logic.formatAspect('x', { kind: 'temperature_delta' }, -2), '-2.0°');
  assert.equal(logic.formatAspect('x', { kind: 'percent' }, 87.6), '88%');
  assert.equal(logic.formatAspect('x', { kind: 'number' }, 3.14159), '3.14');
  assert.equal(logic.formatAspect('x', { kind: 'number', unit: 'lqi' }, 87), '87 lqi', 'a unit rides a plain number');
  assert.equal(logic.formatAspect('x', { kind: 'percent', unit: '%' }, 87), '87%', 'and only a plain number');
  assert.equal(logic.formatAspect('x', null, undefined), '—');
  const locked = { kind: 'boolean', values: [{ value: true, label: 'locked' }, { value: false, label: 'unlocked' }] };
  assert.equal(logic.formatAspect('locked', locked, true), 'locked', 'a boolean may carry value labels');
  assert.equal(logic.formatAspect('locked', locked, false), 'unlocked');
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

test('the card plan takes the first two control-less readings and the family controls', () => {
  const d = descriptor();
  // field order is the adapter's ordering: feed line right after indoor
  const fields = {};
  Object.keys(d.fields).forEach((a) => {
    fields[a] = d.fields[a];
    if (a === 'indoor_temperature') fields.feed_temperature = { label: 'feed line (GT1)', kind: 'temperature', group: 'readings' };
  });
  d.fields = fields;
  const state = heatPumpState({ 'home/state/utility/heat_pump/feed_temperature': 38.4 });
  const plan = logic.cardPlan(HEAT_PUMP, state, d, true);
  assert.deepEqual(plan.readings.map((r) => [r.label, r.display]), [['indoor', '20.3°'], ['feed line', '38.4°']]);
  assert.equal(plan.readings[0].stale, true, 'rows keep their flags');
  assert.deepEqual(plan.controls.map((r) => [r.aspect, r.control.kind]), [['setpoint', 'dial'], ['operating_mode', 'segment']]);
  // the overlay label is untouched
  assert.equal(logic.aspectPlan(HEAT_PUMP, state, d, true)[1].rows.find((r) => r.aspect === 'feed_temperature').label, 'feed line (GT1)');
});

test('the card plan never reaches into diagnostics and skips owner-tier commands', () => {
  const d = descriptor();
  d.groups = ['control'];
  Object.keys(d.fields).forEach((a) => { if (d.fields[a].group !== 'control') delete d.fields[a]; });
  const plan = logic.cardPlan(HEAT_PUMP, heatPumpState(), d, true);
  assert.deepEqual(plan.readings, [], 'undescribed aspects are not headline material');
  assert.deepEqual(plan.controls.map((r) => r.aspect), ['setpoint', 'operating_mode'], 'feed target is owner-tier');
});

// ---- sensor card rows (#56) ----

const THERMOMETER = { name: 'snzb_02', room: 'bedroom', capability: 'sensor', label: 'Bedroom thermometer' };

function thermometerDescriptor() {
  return {
    schema: 1,
    groups: ['readings', 'diagnostics'],
    fields: {
      temperature: { label: 'temperature', kind: 'temperature', group: 'readings' },
      humidity: { label: 'humidity', kind: 'percent', group: 'readings' },
      battery: { label: 'battery', kind: 'percent', group: 'readings' },
      voltage: { label: 'voltage', kind: 'number', unit: 'mV', group: 'diagnostics' },
      linkquality: { label: 'link quality', kind: 'number', unit: 'lqi', group: 'diagnostics' },
    },
  };
}

function thermometerState() {
  return {
    // state-key order is arrival order: linkquality first, as z2m publishes it
    'home/state/bedroom/snzb_02/linkquality': 120,
    'home/state/bedroom/snzb_02/battery': 87,
    'home/state/bedroom/snzb_02/humidity': 41.2,
    'home/state/bedroom/snzb_02/temperature': 20.55,
    'home/state/bedroom/snzb_02/voltage': 2900,
    'home/state/bedroom/snzb_02/available': true,
    'home/state/utility/heat_pump/indoor_temperature': 20.3,
  };
}

test('a described sensor card lists the readings in descriptor order and skips diagnostics', () => {
  const rows = logic.sensorCardPlan(THERMOMETER, thermometerState(), thermometerDescriptor());
  assert.deepEqual(rows.map((r) => r.aspect), ['temperature', 'humidity', 'battery']);
  assert.deepEqual(rows.map((r) => r.display), ['20.6°', '41%', '87%'], 'rows carry the descriptor formatting');
  assert.ok(rows.every((r) => r.numeric && !r.control));
});

test('an undescribed sensor card is its numeric state, sorted', () => {
  const rows = logic.sensorCardPlan(THERMOMETER, thermometerState(), undefined);
  assert.deepEqual(rows.map((r) => r.aspect), ['battery', 'humidity', 'linkquality', 'temperature', 'voltage']);
  assert.ok(!rows.some((r) => r.aspect === 'available'), 'booleans have no sparkline');
});

test('a sensor card keeps a stale reading, flagged, and never a foreign entity', () => {
  const d = thermometerDescriptor();
  d.fields.temperature.valid = 'temperature_valid';
  const state = Object.assign(thermometerState(), { 'home/state/bedroom/snzb_02/temperature_valid': false });
  const rows = logic.sensorCardPlan(THERMOMETER, state, d);
  assert.equal(rows.find((r) => r.aspect === 'temperature').stale, true);
  assert.ok(!rows.some((r) => r.aspect === 'indoor_temperature'));
});

// ---- pending commands (issue #94) ----

function cmd(overrides) {
  return Object.assign(
    { id: 'c0ffee01', room: 'livingroom', entity: 'lamp', aspect: 'on', value: true, capability: 'light' },
    overrides || {}
  );
}

test('the timeout is scaled to the capability, not one global constant', () => {
  // The spread the issue measured: a z2m lamp answers in about a second,
  // a burner behind a polling bridge can take half a minute.
  assert.ok(logic.commandTimeoutMs('burner') > logic.commandTimeoutMs('light'));
  assert.equal(logic.commandTimeoutMs('climate'), logic.COMMAND_TIMEOUT_MS.climate);
  assert.equal(logic.commandTimeoutMs('no-such-capability'), logic.DEFAULT_COMMAND_TIMEOUT_MS);
});

test('a tracked command is pending until something ends it', () => {
  const pending = logic.trackCommand({}, cmd(), 1000);
  const entry = logic.pendingFor(pending, 'livingroom', 'lamp', 'on');
  assert.equal(entry.outcome, 'pending');
  assert.equal(entry.id, 'c0ffee01');
  assert.equal(entry.value, true);
  assert.equal(logic.pendingFor(pending, 'livingroom', 'lamp', 'brightness'), null);
});

test('a readback confirms it', () => {
  const pending = logic.trackCommand({}, cmd(), 1000);
  const done = logic.resolveFromState(pending, 'home/state/livingroom/lamp/on');
  assert.equal(done.outcome, 'confirmed');
  assert.equal(logic.pendingFor(pending, 'livingroom', 'lamp', 'on'), null);
});

test('a readback for another aspect leaves it pending', () => {
  const pending = logic.trackCommand({}, cmd(), 1000);
  assert.equal(logic.resolveFromState(pending, 'home/state/livingroom/lamp/brightness'), null);
  assert.equal(logic.pendingFor(pending, 'livingroom', 'lamp', 'on').outcome, 'pending');
});

test('a device that clamped the value has still answered', () => {
  // Resolution is "the aspect reported", not "the aspect reported what I
  // asked" — otherwise a clamped setpoint hangs pending until it times out
  // and reports a failure that did not happen.
  const pending = logic.trackCommand({}, cmd({ aspect: 'setpoint', value: 99, capability: 'climate' }), 1000);
  const done = logic.resolveFromState(pending, 'home/state/livingroom/lamp/setpoint');
  assert.equal(done.outcome, 'confirmed');
});

test('an arbiter refusal is held, not failed, and names the band', () => {
  const pending = logic.trackCommand({}, cmd(), 1000);
  const done = logic.resolveFromEvent(pending, {
    kind: 'refuse', cmd_id: 'c0ffee01', holder_priority: 'manual', holder_actor: 'owner'
  });
  assert.equal(done.outcome, 'held');
  assert.equal(done.by, 'manual');
  assert.equal(done.actor, 'owner');
  assert.equal(logic.pendingFor(pending, 'livingroom', 'lamp', 'on'), null);
});

test("an adapter's drop is a rejection carrying the adapter's own reason", () => {
  const pending = logic.trackCommand({}, cmd(), 1000);
  const done = logic.resolveFromEvent(pending, {
    kind: 'drop', reason: 'invalid-command', cmd_id: 'c0ffee01'
  });
  assert.equal(done.outcome, 'rejected');
  assert.equal(done.reason, 'invalid-command');
});

test('an event for a different command leaves ours alone', () => {
  // The reason the envelope carries an id at all: two commands to one
  // aspect must not resolve each other.
  const pending = logic.trackCommand({}, cmd(), 1000);
  assert.equal(logic.resolveFromEvent(pending, { kind: 'refuse', cmd_id: 'somethingelse' }), null);
  assert.equal(logic.resolveFromEvent(pending, { kind: 'refuse' }), null);
  assert.equal(logic.pendingFor(pending, 'livingroom', 'lamp', 'on').outcome, 'pending');
});

test('a second tap replaces the first, and the stale id no longer resolves', () => {
  let pending = logic.trackCommand({}, cmd(), 1000);
  pending = logic.trackCommand(pending, cmd({ id: 'second', value: false }), 2000);
  assert.equal(logic.pendingFor(pending, 'livingroom', 'lamp', 'on').id, 'second');
  assert.equal(logic.resolveFromEvent(pending, { kind: 'refuse', cmd_id: 'c0ffee01' }), null);
});

test('nothing answering expires, and is not reported as success', () => {
  const pending = logic.trackCommand({}, cmd(), 1000);
  const timeout = logic.COMMAND_TIMEOUT_MS.light;
  assert.deepEqual(logic.expirePending(pending, 1000 + timeout), []);
  const expired = logic.expirePending(pending, 1000 + timeout + 1);
  assert.equal(expired.length, 1);
  assert.equal(expired[0].outcome, 'unconfirmed');
  assert.equal(logic.pendingFor(pending, 'livingroom', 'lamp', 'on'), null);
});

test('a slow device is not expired on a fast device timeout', () => {
  const pending = logic.trackCommand({}, cmd({ entity: 'burner', aspect: 'power_level', capability: 'burner' }), 1000);
  assert.deepEqual(logic.expirePending(pending, 1000 + logic.COMMAND_TIMEOUT_MS.light + 1), []);
  assert.equal(logic.expirePending(pending, 1000 + logic.COMMAND_TIMEOUT_MS.burner + 1).length, 1);
});

// ---- history shapes ----

test('an undescribed reading is charted by its type: numbers a line, anything else a timeline', () => {
  assert.equal(logic.historyShape(21.5), 'chart');
  assert.equal(logic.historyShape(true), 'timeline');
  assert.equal(logic.historyShape('auto'), 'timeline');
  assert.equal(logic.historyShape(undefined), 'timeline');
});

test('a described enum is runs whatever its values are coded as', () => {
  // ivt490's operating_mode: labelled codes, not a quantity — a line
  // between 1 and 3, with a mean, says nothing.
  const mode = { label: 'mode', kind: 'enum', values: [{ value: 1, label: 'normal' }, { value: 3, label: 'boost' }] };
  assert.equal(logic.historyShape(1, mode), 'timeline');
  assert.equal(logic.historyShape('auto', { kind: 'enum' }), 'timeline');
  assert.equal(logic.historyShape(true, { kind: 'boolean' }), 'timeline');
  // and the run's value still reads as its label
  assert.equal(logic.formatAspect('operating_mode', mode, 1), 'normal');
  // a described number is still a line
  assert.equal(logic.historyShape(21.5, { kind: 'temperature' }), 'chart');
});

test('one bucket per drawn column, never below a second', () => {
  assert.equal(logic.bucketSeconds(24, 400), 216);
  assert.equal(logic.bucketSeconds(168, 400), 1512);
  assert.equal(logic.bucketSeconds(1, 400), 9);
  assert.equal(logic.bucketSeconds(0.01, 400), 1);
});

test('change rows become runs that end where the next begins or at the window', () => {
  const T0 = Date.parse('2026-09-17T10:00:00Z');
  const points = [
    { ts: '2026-09-17T09:00:00Z', value: true },   // before the window: clipped to it
    { ts: '2026-09-17T11:00:00Z', value: false },
    { ts: '2026-09-17T12:30:00Z', value: true },
  ];
  const runs = logic.timelineRuns(points, T0, T0 + 4 * 3600e3);
  assert.deepEqual(runs, [
    { value: true, start: T0, end: T0 + 3600e3 },
    { value: false, start: T0 + 3600e3, end: T0 + 2.5 * 3600e3 },
    { value: true, start: T0 + 2.5 * 3600e3, end: T0 + 4 * 3600e3 },
  ]);
  // nothing is known before the first row: the timeline starts there
  const late = logic.timelineRuns(points.slice(1), T0, T0 + 4 * 3600e3);
  assert.equal(late[0].start, T0 + 3600e3);
  assert.deepEqual(logic.timelineRuns([], T0, T0 + 1), []);
});

test('timeline stats: time on for a boolean, changes and the latest value for any', () => {
  const T0 = 0;
  const bools = logic.timelineRuns(
    [{ ts: new Date(0).toISOString(), value: true }, { ts: new Date(3600e3).toISOString(), value: false },
     { ts: new Date(3 * 3600e3).toISOString(), value: true }],
    T0, 4 * 3600e3);
  assert.deepEqual(logic.timelineStats(bools), { onMs: 2 * 3600e3, changes: 2, latest: true });
  const modes = logic.timelineRuns(
    [{ ts: new Date(0).toISOString(), value: 'auto' }, { ts: new Date(3600e3).toISOString(), value: 'off' }],
    T0, 2 * 3600e3);
  assert.deepEqual(logic.timelineStats(modes), { onMs: null, changes: 1, latest: 'off' });
  assert.deepEqual(logic.timelineStats([]), { onMs: null, changes: 0, latest: null });
});

// ---- views: dashboard.toml is the nav ----

function viewsModel(views) {
  return {
    zones: { downstairs: ['kitchen', 'livingroom'] },
    entities: [
      { name: 'lamp', label: 'Lamp', capability: 'light', room: 'livingroom', owner: 'zigbee' },
      { name: 'thermo', label: 'Thermo', capability: 'sensor', room: 'kitchen', owner: 'zigbee' },
      { name: 'fused', label: 'Fused', capability: 'sensor', room: 'global', owner: 'fusion' },
      { name: 'anna', label: 'Anna', capability: 'person', room: 'person', owner: 'owntracks' },
    ],
    units: [
      { name: 'evening_lights', params: { off_time: { type: 'time', editable_by: 'family' } },
        drives: [{ entity: 'lamp', aspect: 'on' }],
        sources: [{ entity: 'thermo', aspect: 'temperature' }, { entity: 'lamp', aspect: 'on' }] },
      { name: 'fusion', params: {}, drives: [], sources: [{ entity: 'thermo', aspect: 'temperature' }] },
      { name: 'zigbee', params: { poll: { type: 'float', editable_by: 'owner' } }, drives: [], sources: [] },
      { name: 'heating', params: { night: { type: 'float', editable_by: 'family' } }, drives: [], sources: [] },
    ],
    views: views,
  };
}

test('without a views file the nav is the generated three, Health living on the pin', () => {
  assert.deepEqual(logic.viewsOf(viewsModel(null)).map((v) => [v.name, v.kind]),
    [['now', 'now'], ['setpoints', 'setpoints'], ['rooms', 'rooms']]);
});

test('a views file is the whole nav, labels falling back to the title-cased name', () => {
  const views = logic.viewsOf(viewsModel([
    { name: 'downstairs', widgets: [{ kind: 'people' }] },
    { name: 'all_rooms', kind: 'rooms' },
    { name: 'x', label: 'Custom' },
  ]));
  assert.deepEqual(views.map((v) => [v.name, v.label, v.kind, v.widgets.length]),
    [['downstairs', 'Downstairs', null, 1], ['all_rooms', 'All Rooms', 'rooms', 0], ['x', 'Custom', null, 0]]);
});

test('the generated views place everything, so nothing is unshown by default', () => {
  assert.deepEqual(logic.placement(viewsModel(null)), { entities: [], params: [] });
});

test('placement: each widget kind places exactly what it shows', () => {
  const names = (p) => ({ entities: p.entities.map((e) => e.name), params: p.params.map((q) => q.unit + '.' + q.param) });
  const all = { entities: ['lamp', 'thermo', 'fused', 'anna'], params: ['evening_lights.off_time', 'heating.night'] };
  assert.deepEqual(names(logic.placement(viewsModel([{ name: 'v', widgets: [{ kind: 'deviations' }, { kind: 'map' }] }]))), all,
    'signals place nothing');
  assert.deepEqual(names(logic.placement(viewsModel([{ name: 'v', widgets: [{ kind: 'tile', entity: 'thermo' }] }]))).entities,
    ['lamp', 'fused', 'anna']);
  assert.deepEqual(names(logic.placement(viewsModel([{ name: 'v', widgets: [{ kind: 'room', room: 'kitchen' }] }]))).entities,
    ['lamp', 'fused', 'anna']);
  assert.deepEqual(names(logic.placement(viewsModel([{ name: 'v', widgets: [{ kind: 'people' }] }]))).entities,
    ['lamp', 'thermo', 'fused']);
  // a unit card places what it publishes, drives and sets — not what it reads
  const unit = names(logic.placement(viewsModel([{ name: 'v', widgets: [{ kind: 'unit', unit: 'evening_lights' }, { kind: 'unit', unit: 'fusion' }] }])));
  assert.deepEqual(unit, { entities: ['thermo', 'anna'], params: ['heating.night'] });
  assert.deepEqual(names(logic.placement(viewsModel([{ name: 'v', widgets: [{ kind: 'params', unit: 'heating' }] }]))).params,
    ['evening_lights.off_time']);
  // a group places exactly what its members place, and nothing itself
  assert.deepEqual(names(logic.placement(viewsModel([{ name: 'v', widgets: [
    { kind: 'group', label: 'Kitchen', widgets: [{ kind: 'tile', entity: 'thermo' }, { kind: 'people' }] },
  ] }]))).entities, ['lamp', 'fused']);
  assert.deepEqual(names(logic.placement(viewsModel([{ name: 'v', widgets: [{ kind: 'group', widgets: [] }] }]))), all,
    'an empty group places nothing');
  // generated views inside the file place like their standalone selves
  assert.deepEqual(names(logic.placement(viewsModel([{ name: 'r', kind: 'rooms' }]))).entities, []);
  assert.deepEqual(names(logic.placement(viewsModel([{ name: 's', kind: 'setpoints' }]))).params, []);
  assert.deepEqual(names(logic.placement(viewsModel([{ name: 'n', kind: 'now' }]))).entities, ['lamp', 'thermo', 'fused']);
});

test('a deviation tap looks inside a group for the view that shows its unit', () => {
  const views = [{ name: 'heat', widgets: [{ kind: 'group', widgets: [{ kind: 'params', unit: 'heating' }] }] }];
  assert.equal(logic.viewFor({ type: 'setpoint', unit: 'heating', param: 'night' }, logic.viewsOf(viewsModel(views))), 'heat');
});

test('the unit card reads its four relations back from the model', () => {
  const field = (f) => f.entity.name + (f.aspect ? '.' + f.aspect : '');
  const plan = logic.unitCardPlan(viewsModel(null), 'evening_lights');
  assert.deepEqual(plan.params, ['off_time']);
  assert.deepEqual(plan.publishes, []);
  // fields, not entities: the lamp is driven on `on` and read back on it,
  // which is two rows saying different things, not one entity listed twice
  assert.deepEqual(plan.drives.map(field), ['lamp.on']);
  assert.deepEqual(plan.sources.map(field), ['thermo.temperature', 'lamp.on']);
  assert.deepEqual(logic.unitCardPlan(viewsModel(null), 'fusion').publishes.map((e) => e.name), ['fused']);
  assert.equal(logic.unitCardPlan(viewsModel(null), 'nope'), null);
  // an entity the model does not carry drops out; a relation without an
  // aspect (an entity whose commandable fields are not known yet) stays
  const model = viewsModel(null);
  model.units[0].drives = [{ entity: 'gone', aspect: 'on' }, { entity: 'lamp', aspect: null }];
  assert.deepEqual(logic.unitCardPlan(model, 'evening_lights').drives.map(field), ['lamp']);
});

test('a deviation tap lands on the view that shows its subject, or nowhere', () => {
  const views = logic.viewsOf(viewsModel([
    { name: 'heat', widgets: [{ kind: 'params', unit: 'heating' }] },
    { name: 'down', widgets: [{ kind: 'unit', unit: 'evening_lights' }] },
  ]));
  assert.equal(logic.viewFor({ type: 'setpoint', unit: 'heating' }, views), 'heat');
  assert.equal(logic.viewFor({ type: 'setpoint', unit: 'evening_lights' }, views), 'down');
  assert.equal(logic.viewFor({ type: 'setpoint', unit: 'zigbee' }, views), null);
  assert.equal(logic.viewFor({ type: 'rooms' }, views), null);
  const generated = logic.viewsOf(viewsModel(null));
  assert.equal(logic.viewFor({ type: 'setpoint', unit: 'zigbee' }, generated), 'setpoints');
  assert.equal(logic.viewFor({ type: 'rooms' }, generated), 'rooms');
});

// ---- forecasts (docs/design.md, Forecasts) ----

const FORECAST_KEY = 'home/forecast/global/spot/price';

function doc(points, issued) {
  return {
    [FORECAST_KEY]: {
      schema: 1,
      issued: issued || '2026-09-21T08:00:00+00:00',
      points,
    },
  };
}

test('a forecast decodes to millisecond points on the chart axis', () => {
  const f = logic.forecastFor(
    doc([
      { t: '2026-09-21T09:00:00+00:00', v: 1.2, d: 3600 },
      { t: '2026-09-21T10:00:00+00:00', v: 0.4 },
    ]),
    'global',
    'spot',
    'price',
  );
  assert.equal(f.points.length, 2);
  assert.equal(f.points[0].v, 1.2);
  assert.equal(f.points[0].d, 3600);
  assert.equal(f.points[1].d, null, 'an instant carries no extent');
  assert.equal(f.from, Date.parse('2026-09-21T09:00:00+00:00'));
});

test("the horizon ends where a final interval ends, not where it starts", () => {
  // Otherwise a coarse trailing window — the shape real sources publish
  // furthest out — is drawn as a dot at its own start.
  const f = logic.forecastFor(
    doc([
      { t: '2026-09-21T09:00:00+00:00', v: 1.0, d: 3600 },
      { t: '2026-09-21T12:00:00+00:00', v: 2.0, d: 10800 },
    ]),
    'global',
    'spot',
    'price',
  );
  assert.equal(f.to, Date.parse('2026-09-21T15:00:00+00:00'));
});

test('a trailing instant ends the horizon at itself', () => {
  const f = logic.forecastFor(
    doc([
      { t: '2026-09-21T09:00:00+00:00', v: 1.0 },
      { t: '2026-09-21T12:00:00+00:00', v: 2.0 },
    ]),
    'global',
    'spot',
    'price',
  );
  assert.equal(f.to, Date.parse('2026-09-21T12:00:00+00:00'));
});

test('nothing to draw reads as nothing, never as a broken chart', () => {
  for (const [label, forecasts] of [
    ['no forecast for this aspect', {}],
    ['an empty horizon', doc([])],
    ['a malformed timestamp', doc([{ t: 'soon', v: 1 }])],
    ['a non-numeric value', doc([{ t: '2026-09-21T09:00:00+00:00', v: 'cold' }])],
  ]) {
    assert.equal(logic.forecastFor(forecasts, 'global', 'spot', 'price'), null, label);
  }
});

test('a horizon summary names where the series goes and when', () => {
  const f = logic.forecastFor(
    doc([
      { t: '2026-09-21T09:00:00+00:00', v: 1.2 },
      { t: '2026-09-21T10:00:00+00:00', v: 0.4 },
      { t: '2026-09-21T11:00:00+00:00', v: 1.9 },
    ]),
    'global',
    'spot',
    'price',
  );
  const summary = logic.horizonSummary(f);
  assert.equal(summary.min.v, 0.4);
  assert.equal(summary.min.t, Date.parse('2026-09-21T10:00:00+00:00'));
  assert.equal(summary.max.v, 1.9);
});

test('a flat horizon summarises to nothing rather than to min = max', () => {
  const f = logic.forecastFor(
    doc([
      { t: '2026-09-21T09:00:00+00:00', v: 7.0 },
      { t: '2026-09-21T10:00:00+00:00', v: 7.0 },
    ]),
    'global',
    'spot',
    'price',
  );
  assert.equal(logic.horizonSummary(f), null);
  assert.equal(logic.horizonSummary(null), null);
});

test('a forecast delta lands in the store and a snapshot replaces the lot', () => {
  const store = { state: {}, forecasts: {}, health: {}, config: {}, aspects: {} };
  assert.equal(
    logic.applyMessage(store, { type: 'forecast', key: FORECAST_KEY, value: { schema: 1 } }, 0),
    'forecast',
  );
  assert.deepEqual(store.forecasts[FORECAST_KEY], { schema: 1 });
  logic.applyMessage(store, { type: 'snapshot', state: {}, forecasts: {} }, 0);
  assert.deepEqual(store.forecasts, {}, 'a snapshot is the whole truth, not a merge');
});

// ---- stored forecasts: the braid's data ----

function issueDoc(issued, points) {
  return { schema: 1, issued: issued, points: points };
}

test('stored issues decode like live ones and come back oldest first', () => {
  // The store replies in the wire's own spelling, so one decoder serves
  // both — which is why the recorder answers in issues rather than rows.
  const issues = logic.decodeIssues([
    issueDoc('2026-09-21T09:00:00+00:00', [{ t: '2026-09-21T12:00:00+00:00', v: 1.4, d: 3600 }]),
    issueDoc('2026-09-21T08:00:00+00:00', [{ t: '2026-09-21T12:00:00+00:00', v: 1.0, d: 3600 }]),
  ]);
  assert.equal(issues.length, 2);
  assert.equal(issues[0].issued, Date.parse('2026-09-21T08:00:00+00:00'));
  assert.equal(issues[1].points[0].v, 1.4);
});

test('an unreadable issue is dropped, not drawn as a broken line', () => {
  const issues = logic.decodeIssues([
    issueDoc('2026-09-21T08:00:00+00:00', [{ t: 'soon', v: 1 }]),
    issueDoc('2026-09-21T09:00:00+00:00', []),
    issueDoc('2026-09-21T10:00:00+00:00', [{ t: '2026-09-21T12:00:00+00:00', v: 2 }]),
  ]);
  assert.equal(issues.length, 1);
  assert.equal(issues[0].points[0].v, 2);
});

test("one issue's value follows the extent rule, not the nearest point", () => {
  const [held] = logic.decodeIssues([
    issueDoc('2026-09-21T08:00:00+00:00', [
      { t: '2026-09-21T12:00:00+00:00', v: 1.0, d: 3600 },
      { t: '2026-09-21T13:00:00+00:00', v: 2.0, d: 3600 },
    ]),
  ]);
  // An interval HOLDS across its window rather than sliding toward the
  // next point, and stops at its end rather than running on.
  assert.equal(logic.valueAt(held, Date.parse('2026-09-21T12:30:00+00:00')), 1.0);
  assert.equal(logic.valueAt(held, Date.parse('2026-09-21T13:30:00+00:00')), 2.0);
  assert.equal(logic.valueAt(held, Date.parse('2026-09-21T14:30:00+00:00')), null);

  const [instants] = logic.decodeIssues([
    issueDoc('2026-09-21T08:00:00+00:00', [
      { t: '2026-09-21T12:00:00+00:00', v: 1.0 },
      { t: '2026-09-21T14:00:00+00:00', v: 2.0 },
    ]),
  ]);
  // Instants interpolate between themselves.
  assert.equal(logic.valueAt(instants, Date.parse('2026-09-21T13:00:00+00:00')), 1.5);
  assert.equal(logic.valueAt(instants, Date.parse('2026-09-21T11:00:00+00:00')), null);
});

test('a column reads what every issue said about one instant', () => {
  // The slice whose x axis is issue time, and which therefore cannot
  // share the chart — delivered by the scrub instead.
  const issues = logic.decodeIssues([
    issueDoc('2026-09-21T08:00:00+00:00', [{ t: '2026-09-21T12:00:00+00:00', v: 1.0, d: 3600 }]),
    issueDoc('2026-09-21T09:00:00+00:00', [{ t: '2026-09-21T12:00:00+00:00', v: 1.6, d: 3600 }]),
    issueDoc('2026-09-21T10:00:00+00:00', [{ t: '2026-09-21T12:00:00+00:00', v: 1.3, d: 3600 }]),
  ]);
  assert.deepEqual(logic.columnAt(issues, Date.parse('2026-09-21T12:30:00+00:00')), {
    count: 3,
    min: 1.0,
    max: 1.6,
  });
  // Outside every horizon there is nothing to report — not a zero-width
  // spread, which would read as perfect agreement.
  assert.equal(logic.columnAt(issues, Date.parse('2026-09-21T20:00:00+00:00')), null);
  assert.equal(logic.columnAt([], Date.parse('2026-09-21T12:30:00+00:00')), null);
});
