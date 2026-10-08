/* The unit widget: a unit's card and its relations. */
import { healthTooltip, statusGlyph } from '../chrome.js';
import { html } from '../html.js';
import logic from '../logic.js';
import { descriptorField, localState, stateValue, store } from '../store.js';
import { widgetForEntity } from './entity.js';
import { renderParamRow } from './params.js';

// A unit's card (dashboard-logic.js, unitCardPlan): the head, then the
// relations the manifest and the grant table state. Each section is drawn
// only when it has members. The labels are the page's words, not the
// manifest's, but the card shows nothing the manifest does not say.
export function widgetUnit(unitName) {
  var plan = logic.unitCardPlan(store.model, unitName);
  if (!plan) return '';
  var u = plan.unit;
  var h = store.health['home/health/' + u.name] || {};
  var status = h.status || 'stopped';
  var parts = [html`<div class="unit-head row-clickable" data-action="unit-detail" data-unit="${u.name}">
    <div><div class="unit-name" title="${u.label}">${u.label}</div>
    ${u.description ? html`<div class="unit-desc">${u.description}</div>` : ''}</div>
    <span class="status-badge" title="${healthTooltip(h)}"><span class="status-glyph status-${status}">${statusGlyph(status)}</span>${status}</span></div>`];
  var section = function (label, rows) {
    return rows.length ? html`<div class="section-label">${label}</div>${rows}` : '';
  };
  // The card's head already names the unit, so each row shows only the
  // param name.
  parts.push(section('Setpoints', plan.params.map(function (p) { return renderParamRow(u.name, p, u.params[p], true); })));
  parts.push(section('Publishes', plan.publishes.map(widgetForEntity)));
  // Drives and From describe the unit's wiring rather than what it offers
  // the family. The card shows a count, and the fields when expanded.
  var wiring = plan.drives.length + plan.sources.length;
  if (wiring) {
    var open = !!localState.relations[u.name];
    var summary = [];
    if (plan.drives.length) summary.push('drives ' + plan.drives.length);
    if (plan.sources.length) summary.push('reads ' + plan.sources.length);
    parts.push(html`<button class="group-toggle" data-action="toggle-relations" data-unit="${u.name}">
      ${summary.join(' · ')} ${open ? '\u25be' : '\u25b8'}</button>`);
    if (open) {
      parts.push(section('Drives', plan.drives.map(relationRow)));
      parts.push(section('From', plan.sources.map(relationRow)));
    }
  }
  return html`<div class="card unit-card">${parts}</div>`;
}

// One relation: an entity's field and its current value. The card shows
// the relation, and a tap opens the entity, where the user can act on it.
// A driven light's whole card is therefore not repeated in the wiring
// list.
function relationRow(f) {
  var e = f.entity;
  var field = f.aspect ? descriptorField(e, f.aspect) : null;
  var label = e.label + (f.aspect ? ' · ' + ((field && field.label) || f.aspect) : '');
  var value = f.aspect
    ? logic.formatAspect(f.aspect, field, stateValue(e.room, e.name, f.aspect))
    : '';
  return html`<div class="rel-row row-clickable" data-action="entity-detail" data-room="${e.room}" data-entity="${e.name}"><span>${label}</span>
    <span class="rel-value">${value}</span></div>`;
}
