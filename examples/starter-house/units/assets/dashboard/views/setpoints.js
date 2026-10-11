/* The Setpoints view. */
import { byId, html } from '../html.js';
import logic from '../logic.js';
import { store } from '../store.js';
import { renderParamRow } from '../widgets/params.js';

// Setpoints holds the family's settings: family-editable params only.
// Owner params are shown read-only in the unit overlay (Health -> unit),
// and an automation's pause switch on its cards.
export function renderSetpoints() {
  var rows = [];
  (store.model.units || []).forEach(function (u) {
    logic.familyParams(u).forEach(function (pname) {
      rows.push(renderParamRow(u.name, pname, u.params[pname]));
    });
  });
  byId('view').innerHTML = html`<h1 class="view-title">Setpoints</h1>
    ${rows.length === 0
      ? html`<div class="card"><div class="empty-hint">No family-editable parameters.</div></div>`
      : html`<div class="card">${rows}</div>`}`;
}
