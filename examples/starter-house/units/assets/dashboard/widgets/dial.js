/* The dial widget: a thermostat's target as a card. */
import { html } from '../html.js';
import logic from '../logic.js';
import { controlDisabled, houseControls, store } from '../store.js';
import { renderAspectControl } from './controls.js';
import { widgetForClimate, widgetForEntity } from './entity.js';

// A thermostat dial as a card. It uses the named described temperature
// command (or the first one), and otherwise the climate capability's own
// setpoint. An entity with neither falls back to its own row.
export function widgetDial(entity, aspect) {
  var rows = [];
  logic.aspectPlan(entity, store.state, store.aspects[entity.name], !controlDisabled(entity), houseControls()).forEach(function (s) {
    s.rows.forEach(function (r) { rows.push(r); });
  });
  var row = rows.filter(function (r) { return r.control && r.control.kind === 'dial' && (!aspect || r.aspect === aspect); })[0];
  var body = row ? renderAspectControl(entity, row, true)
    : entity.capability === 'climate' ? widgetForClimate(entity, true)
    : widgetForEntity(entity);
  return html`<div class="card clickable" data-action="entity-detail" data-room="${entity.room}" data-entity="${entity.name}">
    <div class="card-label" title="${entity.label}">${entity.label.toUpperCase()}${row ? html` &middot; ${row.label.toUpperCase()}` : ''}</div>${body}</div>`;
}
