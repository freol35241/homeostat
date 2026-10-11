/* The unit overlay: a unit's health, parameters, events and log. */
import { fmtOrDash, statusChipHtml } from '../chrome.js';
import { html } from '../html.js';
import logic, { titleCase, unitNameFromHealthKey } from '../logic.js';
import { renderOverlayContent, showOverlay } from './panel.js';
import { overlay, store, unitsByName } from '../store.js';
import { renderEventRow } from '../views/health.js';
import { renderParamRow, renderPauseSwitch } from '../widgets/params.js';

function fetchUnitLog(unitName) {
  overlay.log = { lines: [], loaded: false, loading: true };
  fetch('/api/logs?unit=' + encodeURIComponent(unitName) + '&lines=100')
    .then(function (r) { return r.json(); })
    .then(function (data) {
      overlay.log = { lines: data || [], loaded: true, loading: false };
      if (overlay.open && overlay.type === 'unit') renderOverlayContent();
    })
    .catch(function () {
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

// The unit's recorded events, so a reload does not empty the list. Without
// an answer from the recorder the overlay shows what this page saw live.
function fetchUnitEvents(unitName) {
  overlay.unitEvents = { rows: [], loaded: false, loading: true, failed: false };
  var done = function (rows, failed) {
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

export function openUnitDetail(unitName) {
  overlay.type = 'unit';
  overlay.unit = unitName;
  fetchUnitLog(unitName);
  fetchUnitEvents(unitName);
  showOverlay();
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
    <div class="card-label" style="margin-top:20px;">Events</div><div class="event-feed">${eventsHtml}</div>
    <div class="card-label" style="margin-top:20px;">Log</div><div class="log-feed">${logHtml}</div>`;
  return { title: unit.label, body: body };
}
