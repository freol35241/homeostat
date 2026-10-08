/* The burner widget: the `burner` vocabulary as one card
 * (docs/design.md#burners-and-interlocks). It shows the two commands, the
 * family control `on` and the output `power_level`, above the two
 * temperatures an interlock reads, each with its day. Device-specific
 * values, such as run-state codes and the firmware's own readings, stay
 * in the entity's overlay, which a tap on the card opens. */
import { ensureHistory } from '../api.js';
import { buildChart } from '../charts.js';
import { html } from '../html.js';
import logic from '../logic.js';
import { controlDisabled, houseControls, localState, store } from '../store.js';
import { aspectValueSpan, renderAspectControl } from './controls.js';

// A descriptor label keeps the firmware's field name in parentheses, as
// in "flue (smoke_temp)". The overlay shows it, but a card has no room,
// so it is trimmed here as on the room card (dashboard-logic.js,
// cardPlan).
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

  // Tapping the head opens the entity. The run-state codes and the
  // device's own readings are shown there, and the card does not repeat
  // them.
  return html`<div class="card burner-card">
    <div class="burner-head row-clickable" data-action="entity-detail" data-room="${entity.room}" data-entity="${entity.name}">
    <div class="card-label" style="margin:0;" title="${entity.label}">${entity.label.toUpperCase()}</div>
    <span class="burner-state${lit ? ' lit' : ''}">${state}</span></div>
    ${controls}${readings}</div>`;
}
