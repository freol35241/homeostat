/* The Rooms view. */
import { byId, html } from '../html.js';
import { titleCase } from '../logic.js';
import { store } from '../store.js';
import { renderRoomCard } from '../widgets/entity.js';

function allZoneNames() {
  var zones = store.model.zones || {};
  var out = [];
  Object.keys(zones).forEach(function (zone) {
    out.push(zone);
  });
  return out;
}

function roomsInZone(zone) {
  var zones = store.model.zones || {};
  return zones[zone] || [];
}

export function renderRooms() {
  var zones = allZoneNames();
  var chips = [html`<button class="chip ${store.roomsFilter === 'all' ? 'active' : ''}" data-action="zone-filter" data-zone="all">Whole house</button>`];
  zones.forEach(function (z) {
    chips.push(html`<button class="chip ${store.roomsFilter === z ? 'active' : ''}" data-action="zone-filter" data-zone="${z}">${titleCase(z)}</button>`);
  });

  var allowedRooms = null;
  if (store.roomsFilter !== 'all') {
    allowedRooms = roomsInZone(store.roomsFilter);
  }

  var byRoom = {};
  (store.model.entities || []).forEach(function (e) {
    if (!e.room) return;
    if (e.capability === 'person') return; // the map is their widget, not a room card
    if (allowedRooms && allowedRooms.indexOf(e.room) === -1) return;
    if (!byRoom[e.room]) byRoom[e.room] = [];
    byRoom[e.room].push(e);
  });

  var roomNames = Object.keys(byRoom).sort();
  var cards = roomNames.map(function (room) { return renderRoomCard(room, byRoom[room]); });

  byId('view').innerHTML = html`<h1 class="view-title">Rooms</h1>
    <div class="chips">${chips}</div>
    ${cards.length === 0
      ? html`<div class="card"><div class="empty-hint">No rooms with entities yet.</div></div>`
      : html`<div class="grid">${cards}</div>`}`;
}
