/* The Health view. */
import { healthTooltip, statusGlyph } from '../chrome.js';
import { byId, html } from '../html.js';
import { unitNameFromHealthKey } from '../logic.js';
import { store, unitLabel } from '../store.js';

export function renderEventRow(ev) {
  var unit = unitNameFromHealthKey(ev.key);
  var extra = Object.keys(ev.value || {}).filter(function (k) { return k !== 'kind'; })
    .map(function (k) { return k + '=' + JSON.stringify(ev.value[k]); }).join(' ');
  var t = new Date((ev.ts || 0) * 1000);
  var tStr = isNaN(t.getTime()) ? '' : t.toLocaleTimeString();
  var tFull = isNaN(t.getTime()) ? '' : t.toLocaleString();
  return html`<div class="event-row"><span class="ts" title="${tFull}">${tStr}</span>
    ${unitLabel(unit)} — ${(ev.value && ev.value.kind) || 'event'}${extra ? ' (' + extra + ')' : ''}</div>`;
}

export function renderHealth() {
  var units = store.model.units || [];
  var cards = units.map(function (u) {
    var key = 'home/health/' + u.name;
    var h = store.health[key] || {};
    var status = h.status || 'stopped';
    var meta = [];
    if (h.restarts !== undefined && h.restarts !== null) meta.push('restarts: ' + h.restarts);
    if (h.last_exit_code !== undefined && h.last_exit_code !== null) meta.push('last exit: ' + h.last_exit_code);
    return html`<div class="card health-card clickable" data-action="unit-detail" data-unit="${u.name}"><div class="health-head">
      <span class="name" title="${u.label}">${u.label}</span>
      <span class="status-badge" title="${healthTooltip(h)}"><span class="status-glyph status-${status}">${statusGlyph(status)}</span>${status}</span></div>
      ${meta.length ? html`<div class="health-meta">${meta.join(' · ')}</div>` : ''}
      </div>`;
  });

  var events = store.events.slice().sort(function (a, b) { return b.ts - a.ts; });
  var eventRows = events.map(renderEventRow);

  byId('view').innerHTML = html`<h1 class="view-title">Health</h1>
    <div class="grid">${cards}</div>
    <div class="card event-feed"><div class="card-label">Events</div>
    ${eventRows.length ? eventRows : html`<div class="empty-hint">No events yet this session.</div>`}
    </div>`;
}
