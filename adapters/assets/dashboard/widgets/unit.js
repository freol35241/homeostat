/* The unit widget: a unit's card and its relations. */
import { healthTooltip, statusGlyph } from '../chrome.js';
import { html } from '../html.js';
import logic from '../logic.js';
import { descriptorField, localState, stateValue, store } from '../store.js';
import { widgetForEntity } from './entity.js';
import { renderParamRow } from './params.js';

// A unit's card (dashboard-logic.js, unitCardPlan): head, then the
// relations the manifest and the grant table already state. Each section
// is drawn only when it has members; the labels are the page's words, not
// the manifest's, and the card can say nothing the manifest did not.
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
  // its own card: the param name alone, the unit is the head
  parts.push(section('Setpoints', plan.params.map(function (p) { return renderParamRow(u.name, p, u.params[p], true); })));
  parts.push(section('Publishes', plan.publishes.map(widgetForEntity)));
  // Drives and From are the unit's wiring, not its surface: a line saying
  // how much of it there is, and the fields themselves when asked for.
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

// One relation: an entity's field and what it reads right now. The card
// states the relation; a tap opens the entity, which is where one acts —
// so a driven light does not drag its whole card into the wiring list.
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
