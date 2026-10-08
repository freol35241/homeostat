/* The Not shown view. */
import { byId, html } from '../html.js';
import logic from '../logic.js';
import { store } from '../store.js';
import { renderRoomCard } from '../widgets/entity.js';
import { renderParamRow } from '../widgets/params.js';

// The complement of every placement in dashboard.toml (dashboard-logic.js,
// placement): what the family cannot reach from the nav, by room, each
// row its normal widget so it is usable here and not merely listed.
export function renderNotShown() {
  var unplaced = logic.placement(store.model);
  var byRoom = {};
  unplaced.entities.forEach(function (e) {
    if (!byRoom[e.room]) byRoom[e.room] = [];
    byRoom[e.room].push(e);
  });
  var rooms = Object.keys(byRoom).sort(function (a, b) {
    // the pseudo-rooms last: the house's own things, and people
    var pa = a === 'global' || a === 'person', pb = b === 'global' || b === 'person';
    return pa === pb ? (a < b ? -1 : 1) : (pa ? 1 : -1);
  });
  var cards = rooms.map(function (room) { return renderRoomCard(room, byRoom[room]); });
  if (unplaced.params.length) {
    cards.push(html`<div class="card"><h3>Setpoints</h3>${unplaced.params.map(function (p) {
      return renderParamRow(p.unit, p.param, p.spec);
    })}</div>`);
  }
  byId('view').innerHTML = html`<h1 class="view-title">Not shown</h1>
    <div class="muted" style="margin:-8px 0 16px;max-width:62ch;">Everything no view in <code>dashboard.toml</code> places. Placing it there removes it from here.</div>
    ${cards.length ? html`<div class="grid">${cards}</div>` : html`<div class="card"><div class="empty-hint">Every entity and setpoint is on a view.</div></div>`}`;
}
