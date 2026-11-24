/* Communication with the dashboard unit: the model, the WebSocket and the
 * changes its messages make, and the recorder's history behind every
 * chart. */
import { CHART_VBW } from './charts.js';
import { updateBanner } from './chrome.js';
import { announceOutcome, pending } from './commands.js';
import logic from './logic.js';
import { localState, scheduleRender, store } from './store.js';

export function loadModel() {
  getJson('/api/model').then(function (m) {
    store.model = m || store.model;
    stampEarlySeen();
    scheduleRender();
  }).catch(function () { /* keep defaults; page still renders */ });
}

var ws = null;

var wsBackoff = 1000;

export function connectWs() {
  var proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  try {
    ws = new WebSocket(proto + '//' + location.host + '/ws');
  } catch (e) {
    scheduleReconnect();
    return;
  }
  ws.onopen = function () {
    store.connected = true;
    wsBackoff = 1000;
    updateBanner();
  };
  ws.onclose = function () {
    store.connected = false;
    updateBanner();
    scheduleReconnect();
  };
  ws.onerror = function () {
    try { ws.close(); } catch (e) {}
  };
  ws.onmessage = function (ev) {
    var msg;
    try { msg = JSON.parse(ev.data); } catch (e) { return; }
    handleWsMessage(msg);
  };
}

function scheduleReconnect() {
  setTimeout(function () {
    connectWs();
  }, wsBackoff);
  wsBackoff = Math.min(wsBackoff * 2, 30000);
}

function handleWsMessage(msg) {
  var applied = logic.applyMessage(store, msg, Date.now());
  if (!applied) return;
  if (applied === 'state') {
    trackLastSeen(msg.key);
    appendSparklinePoint(msg.key, msg.value);
    var answered = logic.resolveFromState(pending, msg.key, msg.value); // stage 4: the device answered
    if (answered) announceOutcome(answered);
  }
  if (applied === 'event') {
    // stages 2 and 3: the arbiter refused it, or an adapter dropped it
    var outcome = logic.resolveFromEvent(pending, msg.value);
    if (outcome) announceOutcome(outcome);
  }
  scheduleRender();
}

// For a key like home/state/{room}/{entity}/motion, track lastSeen per
// entity (both occupancy aspect names; see logic.js). The
// model's vocabulary names them, so a delta that arrives before the model
// is kept and stamped when it loads.
var earlySeen = {};
function trackLastSeen(key) {
  if (!store.model.vocabulary) { earlySeen[key] = Date.now(); return; }
  var pe = logic.occupancyEntityFromKey(key, store.model.vocabulary);
  if (pe) store.lastSeen[pe] = Date.now();
}

function stampEarlySeen() {
  if (!store.model.vocabulary) return;
  Object.keys(earlySeen).forEach(function (key) {
    var pe = logic.occupancyEntityFromKey(key, store.model.vocabulary);
    if (pe) store.lastSeen[pe] = Math.max(store.lastSeen[pe] || 0, earlySeen[key]);
  });
  earlySeen = {};
}

function appendSparklinePoint(key, value) {
  var k = logic.parseStateKey(key);
  if (!k || typeof value !== 'number') return;
  var sk = k.entity + '|' + k.aspect;
  var s = localState.sparklines[sk];
  if (!s || !s.loaded) return; // only append once history has been fetched
  s.points.push({ ts: new Date().toISOString(), value: value });
  if (s.points.length > 500) s.points.shift();
}

/* GET a JSON endpoint of this unit. An error status rejects, with the
 * reply's `error` text when it has one, so no caller reads an error reply
 * as data. */
export function getJson(url) {
  return fetch(url).then(function (r) {
    if (r.ok) return r.json();
    return r.json().catch(function () { return {}; }).then(function (body) {
      throw new Error((body && body.error) || 'HTTP ' + r.status);
    });
  });
}

export function errText(e) {
  var msg = e && e.message ? e.message : String(e);
  try {
    var parsed = JSON.parse(msg);
    if (parsed && parsed.error) return parsed.error;
  } catch (ex) { /* not json */ }
  return msg;
}

// The recorder summarises the window (docs/design.md#read-path). A line
// chart asks for one bucket per drawn column, and a timeline for the runs
// of the state. The window is returned with the points so the chart can
// place them by time.

// The most presses of a button one history read asks for.
export var PRESSES_READ = 500;

// `cls` is the recorder's series class: 'state' (the default) is what the
// house did, and 'cmd' is what was asked of it.
//
// Bars read the raw rows and spread each from its own stamp on the page.
// The recorder's buckets start at multiples of their width since the
// epoch, which is not where a unit in another zone stamps its day. The
// read starts one interval early, for the interval that straddles the
// window's start, and a day earlier for day bars, which are whole days.
// When the window holds more rows than one read returns, it starts where
// the rows do, so the axis does not show a stretch that was not read.
export function fetchHistory(entity, aspect, hours, shape, cls, intervalS) {
  var win = { from: Date.now() - hours * 3600e3, to: Date.now() };
  // A button's presses are read raw, each its own row, so a repeat stays.
  var query = shape === 'timeline' ? '&hours=' + hours + '&changes=1'
    : shape === 'presses' ? '&hours=' + hours + '&limit=' + PRESSES_READ
      : shape !== 'bars' ? '&hours=' + hours + '&bucket=' + logic.bucketSeconds(hours, CHART_VBW)
        : null;
  var limit = 0;
  if (query === null) {
    var extra = intervalS / 3600 + (logic.barsByDay(hours * 3600e3, intervalS) ? 24 : 0);
    limit = Math.min(Math.ceil((hours + extra) * 3600 / intervalS) + 1, store.model.limits.history_limit_max);
    query = '&hours=' + (hours + extra) + '&limit=' + limit;
  }
  var url = '/api/history?entity=' + encodeURIComponent(entity) + '&aspect=' + encodeURIComponent(aspect) + query +
    (cls ? '&class=' + encodeURIComponent(cls) : '');
  return getJson(url)
    .then(function (data) {
      var pts = [];
      (data.series || []).forEach(function (series) {
        (series.points || []).forEach(function (p) { pts.push(p); });
      });
      if (limit && pts.length >= limit) win.from = Math.max(win.from, Date.parse(pts[0].ts));
      return { points: pts, window: win, loaded: true, loading: false };
    })
    .catch(function () {
      return { points: [], window: win, loaded: true, loading: false };
    });
}

// tile/row sparklines: numeric readings, fixed 24h window. An interval
// aspect reads 48 h, because its tile totals the 24 h that end with the
// newest recorded interval, which ends before now.
export function ensureHistory(entity, aspect, intervalS) {
  var sk = entity + '|' + aspect;
  var s = localState.sparklines[sk];
  if (s && (s.loaded || s.loading)) return;
  localState.sparklines[sk] = { points: [], loaded: false, loading: true };
  var fetched = intervalS
    ? fetchHistory(entity, aspect, 48, 'bars', null, intervalS)
    : fetchHistory(entity, aspect, 24, 'chart');
  fetched.then(function (loaded) {
    localState.sparklines[sk] = loaded;
    scheduleRender();
  });
}
