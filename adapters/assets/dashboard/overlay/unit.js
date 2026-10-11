/* The unit overlay: a unit's health, parameters, events or timeline, and log. */
import { fmtOrDash, statusChipHtml } from '../chrome.js';
import { html } from '../html.js';
import logic, { titleCase, unitNameFromHealthKey } from '../logic.js';
import { renderOverlayContent, showOverlay } from './panel.js';
import { findEntityByName, overlay, store, unitsByName } from '../store.js';
import { renderEventRow } from '../views/health.js';
import { renderParamRow, renderPauseSwitch } from '../widgets/params.js';

function fetchUnitLog(unitName) {
  overlay.log = { lines: [], loaded: false, loading: true };
  fetch('/api/logs?unit=' + encodeURIComponent(unitName) + '&lines=100')
    .then(function (r) { return r.json(); })
    .then(function (data) {
      if (overlay.unit !== unitName) return; // the overlay moved on to another unit
      overlay.log = { lines: data || [], loaded: true, loading: false };
      if (overlay.open && overlay.type === 'unit') renderOverlayContent();
    })
    .catch(function () {
      if (overlay.unit !== unitName) return;
      overlay.log = { lines: [], loaded: true, loading: false };
      if (overlay.open && overlay.type === 'unit') renderOverlayContent();
    });
}

function renderLogLine(entry) {
  var t = new Date((entry.ts_us || 0) / 1000);
  var tStr = isNaN(t.getTime()) ? '' : t.toLocaleTimeString();
  var tFull = isNaN(t.getTime()) ? '' : t.toLocaleString();
  var cls = entry.stream === 'stderr' ? 'log-line stderr' : 'log-line';
  return html`<div class="${cls}"><span class="ts" title="${tFull}">${tStr}</span>${entry.line}</div>`;
}

// The unit's recorded events, so a reload does not empty the list. When the
// recorder answers with an error the overlay shows what this page saw live.
// The empty wording below matches the route's default window of 24 hours.
function fetchUnitEvents(unitName) {
  overlay.unitEvents = { rows: [], loaded: false, loading: true, failed: false };
  var done = function (rows, failed) {
    if (overlay.unit !== unitName) return; // the overlay moved on to another unit
    overlay.unitEvents = { rows: rows, loaded: true, loading: false, failed: failed };
    if (overlay.open && overlay.type === 'unit') renderOverlayContent();
  };
  fetch('/api/unit-events?unit=' + encodeURIComponent(unitName))
    .then(function (r) {
      if (!r.ok) throw new Error('HTTP ' + r.status);
      return r.json();
    })
    .then(function (data) { done((data && data.events) || [], false); })
    .catch(function () { done([], true); });
}

// An automation's timeline rows (docs/design.md#unit-timeline). Its
// health events come with them, so it needs no separate events fetch.
function fetchUnitTimeline(unitName) {
  overlay.timeline = { raw: null, loaded: false, loading: true, failed: false };
  var done = function (raw, failed) {
    if (overlay.unit !== unitName) return; // the overlay moved on to another unit
    overlay.timeline = { raw: raw, loaded: true, loading: false, failed: failed };
    if (overlay.open && overlay.type === 'unit') renderOverlayContent();
  };
  fetch('/api/unit-timeline?unit=' + encodeURIComponent(unitName))
    .then(function (r) {
      if (!r.ok) throw new Error('HTTP ' + r.status);
      return r.json();
    })
    .then(function (raw) { done(raw, false); })
    .catch(function () { done(null, true); });
}

export function openUnitDetail(unitName) {
  overlay.type = 'unit';
  overlay.unit = unitName;
  fetchUnitLog(unitName);
  var unit = unitsByName()[unitName];
  if (unit && unit.kind === 'automation') fetchUnitTimeline(unitName);
  else fetchUnitEvents(unitName);
  showOverlay();
}

function fieldLabel(entityName, aspect) {
  var e = findEntityByName(entityName);
  return (e ? e.label : titleCase(entityName || '')) + ' · ' + aspect;
}

function show(value) {
  return typeof value === 'string' ? value : JSON.stringify(value);
}

// One timeline entry: when, what kind, and what happened.
function renderTimelineRow(entry) {
  var t = new Date((entry.ts || 0) * 1000);
  var tStr = isNaN(t.getTime()) ? '' : t.toLocaleTimeString();
  var tFull = isNaN(t.getTime()) ? '' : t.toLocaleString();
  var tag, text;
  if (entry.type === 'input') {
    tag = 'read';
    text = fieldLabel(entry.entity, entry.aspect) + (entry.first ? ' was ' : ' → ') + show(entry.value);
  } else if (entry.type === 'param') {
    tag = 'set';
    text = entry.param + ' set to ' + show(entry.value);
  } else if (entry.type === 'command') {
    tag = 'sent';
    var outcome = entry.outcome;
    if (entry.outcome === 'refused') outcome = 'refused, held by ' + (entry.by || 'a higher band');
    if (entry.outcome === 'dropped') outcome = 'dropped by ' + (entry.by || 'its adapter') + (entry.reason ? ': ' + entry.reason : '');
    text = fieldLabel(entry.entity, entry.aspect) + ' = ' + show(entry.value) + ' (' + outcome + ')';
  } else if (entry.type === 'taken-over') {
    tag = 'taken';
    text = fieldLabel(entry.entity, entry.aspect) + ' taken over by ' + (entry.by || 'another actor') +
      (entry.priority ? ' (' + entry.priority + ')' : '');
  } else {
    tag = 'event';
    var extra = Object.keys(entry.fields || {}).filter(function (k) { return k !== 'kind'; })
      .map(function (k) { return k + '=' + JSON.stringify(entry.fields[k]); }).join(' ');
    text = entry.kind + (extra ? ' (' + extra + ')' : '');
  }
  return html`<div class="event-row timeline-row"><span class="ts" title="${tFull}">${tStr}</span><span class="tl-tag tl-${entry.type}">${tag}</span>${text}</div>`;
}

function renderTimeline(unit, live) {
  var tl = overlay.timeline || { raw: null, loaded: false, loading: false, failed: false };
  var raw = tl.raw || {};
  var merged = Object.assign({}, raw, { events: logic.mergeUnitEvents(raw.events, live) });
  var rows = logic.unitTimeline(merged, store.model, store.aspects, unit.name).map(renderTimelineRow);
  var hint = tl.failed
    ? html`<div class="empty-hint">The recorder did not answer, so this lists the events since the page opened.</div>`
    : raw.truncated ? html`<div class="empty-hint">A busy day: the oldest rows did not fit.</div>` : '';
  if (rows.length) return html`${hint}${rows}`;
  if (tl.loading) return html`<div class="empty-hint">Loading&hellip;</div>`;
  return tl.failed ? hint : html`<div class="empty-hint">Nothing in the last 24 hours.</div>`;
}

export function renderUnitDetailBody() {
  var unit = unitsByName()[overlay.unit] || { name: overlay.unit, label: overlay.unit, kind: '', description: '', params: {} };
  var hkey = 'home/health/' + unit.name;
  var h = store.health[hkey] || {};
  var status = h.status || 'stopped';

  var fieldRows = html`<div class="aspect-row"><span class="muted">status:</span> ${statusChipHtml(status)}</div>
    <div class="aspect-row"><span class="muted">pid:</span> ${fmtOrDash(h.pid)}</div>
    <div class="aspect-row"><span class="muted">restarts:</span> ${fmtOrDash(h.restarts)}</div>
    <div class="aspect-row"><span class="muted">backoff_ms:</span> ${fmtOrDash(h.backoff_ms)}</div>
    <div class="aspect-row"><span class="muted">last_exit_code:</span> ${fmtOrDash(h.last_exit_code)}</div>
    ${logic.pauseParam(unit) ? html`<div class="aspect-row"><span class="muted">enabled:</span> ${renderPauseSwitch(unit)}</div>` : ''}`;

  var params = unit.params || {};
  // An automation's `enabled` is its pause switch, shown under Health.
  var paramRows = Object.keys(params).filter(function (pname) {
    return !(pname === 'enabled' && logic.pauseParam(unit));
  }).map(function (pname) {
    return renderParamRow(unit.name, pname, params[pname]);
  });
  var paramsHtml = paramRows.length ? paramRows : html`<div class="empty-hint">No parameters.</div>`;

  var recorded = overlay.unitEvents || { rows: [], loaded: false, loading: false, failed: false };
  var live = store.events.filter(function (ev) { return unitNameFromHealthKey(ev.key) === unit.name; });
  var automation = unit.kind === 'automation';
  var eventRows = logic.mergeUnitEvents(recorded.rows, live).map(renderEventRow);
  var eventsHint = recorded.failed
    ? html`<div class="empty-hint">The recorder did not answer, so this lists the events since the page opened.</div>`
    : '';
  var eventsHtml = eventRows.length ? html`${eventsHint}${eventRows}`
    : recorded.loading ? html`<div class="empty-hint">Loading&hellip;</div>`
    : recorded.failed ? eventsHint
    : html`<div class="empty-hint">No events in the last 24 hours.</div>`;

  var log = overlay.log || { lines: [], loaded: false, loading: false };
  var logRows = log.lines.map(renderLogLine);
  var logHtml = log.loading ? html`<div class="empty-hint">Loading&hellip;</div>` :
    (logRows.length ? logRows : html`<div class="empty-hint">No output captured.</div>`);

  var body = html`<div class="muted">${titleCase(unit.kind || '')}</div>
    ${unit.description ? html`<div style="margin-top:4px;">${unit.description}</div>` : ''}
    <div class="card-label" style="margin-top:20px;">Health</div>${fieldRows}
    <div class="card-label" style="margin-top:20px;">Params</div>${paramsHtml}
    ${automation
      ? html`<div class="card-label" style="margin-top:20px;">Timeline</div><div class="event-feed">${renderTimeline(unit, live)}</div>`
      : html`<div class="card-label" style="margin-top:20px;">Events</div><div class="event-feed">${eventsHtml}</div>`}
    <div class="card-label" style="margin-top:20px;">Log</div><div class="log-feed">${logHtml}</div>`;
  return { title: unit.label, body: body };
}
