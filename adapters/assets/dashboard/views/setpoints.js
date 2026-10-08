/* The Setpoints view. */
import { byId, html } from '../html.js';
import { store } from '../store.js';
import { familyEditable, renderParamRow } from '../widgets/params.js';

// Setpoints is the family's levers: family-editable params only. Owner
// params show read-only in the unit overlay (Health -> unit).
export function renderSetpoints() {
  var rows = [];
  (store.model.units || []).forEach(function (u) {
    var params = u.params || {};
    Object.keys(params).forEach(function (pname) {
      if (familyEditable(params[pname])) rows.push(renderParamRow(u.name, pname, params[pname]));
    });
  });
  byId('view').innerHTML = html`<h1 class="view-title">Setpoints</h1>
    ${rows.length === 0
      ? html`<div class="card"><div class="empty-hint">No family-editable parameters.</div></div>`
      : html`<div class="card">${rows}</div>`}`;
}
