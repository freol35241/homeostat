/* The page's state: the house as the bus reports it (store), this
 * browser's own UI state (localState, overlay), lookups over the model,
 * and scheduleRender, which any module calls to redraw the page. */
import logic from './logic.js';

export var store = {
  model: { zones: {}, entities: [], units: [] },
  state: {},
  forecasts: {},    // home/forecast key -> that source's current forecast
  holds: {},        // home/hold/{unit} -> what that arbiter is holding right now
  health: {},
  config: {},
  aspects: {},      // entity -> its adapter's aspect descriptor (from discovery)
  events: [],
  lastSeen: {},
  connected: false,
  view: 'now',
  roomsFilter: 'all'
};

export var localState = {
  sliderDrag: {},   // key -> true while user is dragging
  sparklines: {},   // entity|aspect -> {points:[], window, loaded:bool, loading:bool}
  charts: {},       // entity|aspect|hours -> the same, for chart widgets
  relations: {}     // unit -> true while its Drives/From sections are open
};

// detail overlay state (right slide-over / mobile sheet)
export var overlay = {
  open: false,
  type: null,        // 'history' | 'entity' | 'unit' | 'text'
  entity: null,       // for history/entity: entity object
  aspect: null,        // for history: aspect name
  shape: 'chart',      // for history: 'chart' (a number) | 'timeline' (a bool or string)
  shapeSettled: true,  // false while the shape was guessed without a live value
  unit: null,           // for unit: unit name
  viewName: null,        // for text: the view whose dashboard.toml block it shows
  range: 24,             // hours, for history
  rangeData: {},          // hours -> {points, window, loaded, loading}
  cmdData: {},             // hours -> the same, for the cmd series (what was asked)
  // What is drawn beside the record: '' (the value and its current
  // forecast), 'sources' (the contributors it is derived from) or
  // 'forecasts' (every issue the recorder kept). Only one is shown at a
  // time, because two sets of thin grey lines on one chart look like one
  // set. So this is one field rather than a flag for each
  // (docs/design.md#charts-forecasts-and-sources).
  layer: '',
  issues: [],              // decoded stored issues for the current window
  issuesLoading: false,
  sourceData: {},          // "<hours>|<entity>|<aspect>" -> fetched contributor history
  sourceEvents: {},        // same key -> when each source went in or out
  pinned: null,            // the issue time a reader pinned out of the braid
  sourcePinned: null,      // the contributor a reader pinned out of the legend
  lastPoints: null,
  log: null,               // for unit: {lines, loaded, loading}
  expanded: {}              // for entity: group -> true when a collapsed section is opened
};

export function unitsByName() {
  var m = {};
  (store.model.units || []).forEach(function (u) { m[u.name] = u; });
  return m;
}

export function findEntity(room, name) {
  var list = store.model.entities || [];
  for (var i = 0; i < list.length; i++) {
    if (list[i].room === room && list[i].name === name) return list[i];
  }
  return null;
}

// current value for a given entity/aspect from the state map
export function stateValue(room, entity, aspect) {
  return logic.stateValue(store.state, room, entity, aspect);
}

// aspects present in state for an entity (the history title's "· aspect" suffix)
export function aspectsFor(entity) {
  var prefix = 'home/state/' + entity.room + '/' + entity.name + '/';
  return Object.keys(store.state).filter(function (k) { return k.indexOf(prefix) === 0; })
    .map(function (k) { return k.slice(prefix.length); });
}

export function descriptorField(entity, aspect) {
  var d = store.aspects[entity.name];
  return (d && d.fields && d.fields[aspect]) || null;
}

export function unitLabel(unitName) {
  var u = unitsByName()[unitName];
  return u ? u.label : unitName;
}

export function presenceValue(entity) {
  return logic.presenceValue(store.state, entity);
}

export function entitySpec(room, entity) {
  var entities = (store.model && store.model.entities) || [];
  for (var i = 0; i < entities.length; i++) {
    if (entities[i].room === room && entities[i].name === entity) return entities[i];
  }
  return null;
}

/* One render per frame, however many modules ask for it. The render
 * function belongs to main.js, which passes it in at startup, so modules
 * can request a render without importing the entry module. */
var renderQueued = false;
var renderPage = null;

export function setRenderer(render) {
  renderPage = render;
}

export function scheduleRender() {
  if (renderQueued) return;
  renderQueued = true;
  requestAnimationFrame(function () {
    renderQueued = false;
    renderPage();
  });
}

// dashboard.toml's [[control]] entries, or none. They set a control's
// grain wherever that control is drawn.
export function houseControls() {
  return (store.model && store.model.controls) || [];
}

// Every live forecast for one aspect, one per source.
export function forecastsOf(entity, aspect) {
  return logic.forecastsFor(store.forecasts, entity.room, entity.name, aspect);
}

// The forecasts that still reach into the future. A forecast whose
// horizon has run out is not drawn as a forecast. The mirror keeps a
// producer's last value for as long as the core runs, and a curve that
// ends before now says nothing about what is ahead
// (docs/design.md#charts-forecasts-and-sources). It is still shown in the
// overlay under `forecasts`, where an expired forecast can be compared
// with what happened.
export function liveForecastsOf(entity, aspect) {
  return forecastsOf(entity, aspect).filter(function (b) {
    return !logic.forecastFreshness(b.forecast, Date.now()).expired;
  });
}

// Controls are disabled when this dashboard's manifest does not grant the
// entity's capability (model.commandable). The server would refuse such a
// command anyway, and an enabled control would misrepresent the grant
// table.
export function controlDisabled(entity) { return !entity.commandable; }

export function personEntities() {
  return (store.model.entities || []).filter(function (e) { return e.capability === 'person'; });
}

export function entitiesInRoom(room) {
  return (store.model.entities || []).filter(function (e) { return e.room === room; });
}

export function unitByName(name) {
  return (store.model.units || []).filter(function (u) { return u.name === name; })[0];
}

export function findEntityByName(name) {
  return (store.model.entities || []).filter(function (e) { return e.name === name; })[0];
}
