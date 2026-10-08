/* The unit overlay: a unit's health, parameters, events and log. */
import { fmtOrDash, statusChipHtml } from '../chrome.js';
import { html } from '../html.js';
import { titleCase, unitNameFromHealthKey } from '../logic.js';
import { renderOverlayContent, showOverlay } from './panel.js';
import { overlay, store, unitsByName } from '../store.js';
import { renderEventRow } from '../views/health.js';
import { renderParamRow } from '../widgets/params.js';

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

export function openUnitDetail(unitName) {
  overlay.type = 'unit';
  overlay.unit = unitName;
  fetchUnitLog(unitName);
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
    <div class="aspect-row"><span class="muted">last_exit_code:</span> ${fmtOrDash(h.last_exit_code)}</div>`;

  var params = unit.params || {};
  var paramRows = Object.keys(params).map(function (pname) {
    return renderParamRow(unit.name, pname, params[pname]);
  });
  var paramsHtml = paramRows.length ? paramRows : html`<div class="empty-hint">No parameters.</div>`;

  var events = store.events.filter(function (ev) { return unitNameFromHealthKey(ev.key) === unit.name; });
  var eventRows = events.map(renderEventRow);
  var eventsHtml = eventRows.length ? eventRows : html`<div class="empty-hint">No events yet this session.</div>`;

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
