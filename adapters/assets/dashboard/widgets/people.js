/* The people widget. */
import { html } from '../html.js';
import logic from '../logic.js';
import { store } from '../store.js';
import { relTime } from './entity.js';

// People: the person entities, shown as home or away from their presence
// aspect, or otherwise by when they were last seen. Motion sensors belong
// to rooms, not to people.
export function widgetPeople(persons) {
  if (persons.length === 0) return '';
  var pRows = persons.map(function (e) {
    var st = logic.personStatus(store.state, e);
    var detail = st.home === true ? 'Home' : st.home === false ? 'Away'
      : st.seenAt !== undefined ? 'last fix ' + relTime(st.seenAt) : 'no signal';
    return html`<div class="row-clickable" data-action="entity-detail" data-room="${e.room}" data-entity="${e.name}" style="display:flex;align-items:center;gap:6px;padding:3px 0;">
      <span class="status-dot ${st.home === true ? 'dot-on' : 'dot-off'}"></span>
      <span>${e.label} &middot; ${detail}</span></div>`;
  });
  return html`<div class="card tile"><div class="card-label">People</div>${pRows}</div>`;
}
