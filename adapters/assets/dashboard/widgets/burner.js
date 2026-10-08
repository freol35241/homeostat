/* The burner widget: the `burner` vocabulary as one card
 * (docs/design.md#burners-and-interlocks): its two commands, the family
 * lever `on` and the output `power_level`, over the two temperatures an
 * interlock reads, each with its day. Nothing dialectal: run-state codes
 * and a firmware's own readings stay in the entity's overlay, which the
 * card taps through to. */
import { ensureHistory } from '../api.js';
import { buildChart } from '../charts.js';
import { html } from '../html.js';
import logic from '../logic.js';
import { controlDisabled, houseControls, localState, store } from '../store.js';
import { aspectValueSpan, renderAspectControl } from './controls.js';

// A descriptor keeps the firmware's field name in parentheses — "flue
// (smoke_temp)" — which the overlay shows and a card has no room for, the
// same trim the room card makes (dashboard-logic.js, cardPlan).
function shortLabel(label) { return label.replace(/\s*\([^)]*\)$/, ''); }

var BURNER_READINGS = ['flue_temperature', 'boiler_temperature'];

var BURNER_COMMANDS = ['on', 'power_level'];

export function widgetBurner(entity) {
  var rows = {};
  logic.aspectPlan(entity, store.state, store.aspects[entity.name], !controlDisabled(entity), houseControls())
    .forEach(function (s) { s.rows.forEach(function (r) { rows[r.aspect] = r; }); });

  var lit = rows.on && rows.on.value === true;
  var state = rows.available && rows.available.value === false ? 'offline'
    : !rows.on || rows.on.value === undefined ? 'no signal'
    : lit ? 'burning' : 'idle';

  var controls = BURNER_COMMANDS.map(function (aspect) {
    var r = rows[aspect];
    if (!r) return '';
    return html`<div class="burner-control"><span>${shortLabel(r.label)}</span>
      ${r.control ? renderAspectControl(entity, r) : aspectValueSpan(r)}</div>`;
  });

  var readings = BURNER_READINGS.map(function (aspect) {
    var r = rows[aspect];
    if (!r) return '';
    ensureHistory(entity.name, aspect);
    var sk = entity.name + '|' + aspect;
    var hist = localState.sparklines[sk];
    var spark = hist && hist.loaded
      ? buildChart('burner-' + sk, hist.points, { height: 36, sizeClass: 'chart-tile', aspect: aspect, area: true, window: hist.window })
      : '';
    return html`<div class="burner-reading row-clickable" data-action="history-detail" data-room="${entity.room}" data-entity="${entity.name}" data-aspect="${aspect}">
      <div class="burner-reading-head"><span>${shortLabel(r.label)}</span><b>${r.display}</b></div>
      ${spark}</div>`;
  });

  // The head taps through to the entity: the run-state codes and the
  // device's own readings live there, and the card does not repeat them.
  return html`<div class="card burner-card">
    <div class="burner-head row-clickable" data-action="entity-detail" data-room="${entity.room}" data-entity="${entity.name}">
    <div class="card-label" style="margin:0;" title="${entity.label}">${entity.label.toUpperCase()}</div>
    <span class="burner-state${lit ? ' lit' : ''}">${state}</span></div>
    ${controls}${readings}</div>`;
}
