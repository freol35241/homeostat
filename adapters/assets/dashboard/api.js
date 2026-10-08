/* Talking to the dashboard unit: the model, the WebSocket and what its
 * messages change, and the recorder's history behind every chart. */
import { CHART_VBW } from './charts.js';
import { updateBanner } from './chrome.js';
import { announceOutcome, pending } from './commands.js';
import logic from './logic.js';
import { localState, scheduleRender, store } from './store.js';

export function loadModel() {
  fetch('/api/model').then(function (r) { return r.json(); }).then(function (m) {
    store.model = m || store.model;
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
  var applied = logic.applyMessage(store, msg, Date.now() / 1000);
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

// key like home/state/{room}/{entity}/occupancy -> track lastSeen per
// entity (both presence aspect spellings; see dashboard-logic.js).
function trackLastSeen(key) {
  var pe = logic.presenceEntityFromKey(key);
  if (pe) store.lastSeen[pe] = Date.now();
}

function appendSparklinePoint(key, value) {
  var parts = key.split('/');
  if (parts[0] !== 'home' || parts[1] !== 'state' || parts.length < 5) return;
  if (typeof value !== 'number') return;
  var entity = parts[3], aspect = parts[4];
  var sk = entity + '|' + aspect;
  var s = localState.sparklines[sk];
  if (!s || !s.loaded) return; // only append once history has been fetched
  s.points.push({ ts: new Date().toISOString(), value: value });
  if (s.points.length > 500) s.points.shift();
}

export function errText(e) {
  var msg = e && e.message ? e.message : String(e);
  try {
    var parsed = JSON.parse(msg);
    if (parsed && parsed.error) return parsed.error;
  } catch (ex) { /* not json */ }
  return msg;
}

// The recorder folds the window (docs/design.md#read-path): a line asks
// for one bucket per drawn column, a timeline for the runs of the state.
// The window travels with the points so the chart places them by time.

// `cls` is the recorder's series class: 'state' (the default) is what the
// house did, 'cmd' what was asked of it.
export function fetchHistory(entity, aspect, hours, shape, cls) {
  var win = { from: Date.now() - hours * 3600e3, to: Date.now() };
  var url = '/api/history?entity=' + encodeURIComponent(entity) + '&aspect=' + encodeURIComponent(aspect) + '&hours=' + hours +
    (cls ? '&class=' + encodeURIComponent(cls) : '') +
    (shape === 'timeline' ? '&changes=1' : '&bucket=' + logic.bucketSeconds(hours, CHART_VBW));
  return fetch(url)
    .then(function (r) { return r.json(); })
    .then(function (data) {
      var pts = [];
      (data.series || []).forEach(function (series) {
        (series.points || []).forEach(function (p) { pts.push(p); });
      });
      return { points: pts, window: win, loaded: true, loading: false };
    })
    .catch(function () {
      return { points: [], window: win, loaded: true, loading: false };
    });
}

// tile/row sparklines: numeric readings, fixed 24h window
export function ensureHistory(entity, aspect) {
  var sk = entity + '|' + aspect;
  var s = localState.sparklines[sk];
  if (s && (s.loaded || s.loading)) return;
  localState.sparklines[sk] = { points: [], loaded: false, loading: true };
  fetchHistory(entity, aspect, 24, 'chart').then(function (loaded) {
    localState.sparklines[sk] = loaded;
    scheduleRender();
  });
}
