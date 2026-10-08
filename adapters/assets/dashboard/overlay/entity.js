/* The entity overlay: every aspect of one entity, with its controls. */
import { html } from '../html.js';
import logic, { titleCase } from '../logic.js';
import { showOverlay } from './panel.js';
import { controlDisabled, findEntity, houseControls, overlay, store, unitLabel } from '../store.js';
import { aspectValueSpan, renderAspectControl } from '../widgets/controls.js';
import { BESPOKE_WIDGET, widgetForClimate, widgetForEntity, widgetForSensor } from '../widgets/entity.js';

export function openEntityDetail(room, entityName) {
  var entity = findEntity(room, entityName);
  if (!entity) return;
  overlay.type = 'entity';
  overlay.entity = entity;
  overlay.expanded = {};
  showOverlay();
}

// One planned row of an entity's detail: the label and the value, plus the
// control the descriptor declared — the param-control shapes, wired to
// /api/cmd instead of /api/param. A numeric row without a control taps
// through to its history.
function renderAspectRow(entity, r) {
  var attrs = html` data-room="${entity.room}" data-entity="${entity.name}" data-aspect="${r.aspect}"`;
  var value = aspectValueSpan(r);
  // Possession, wherever the aspect is: an arbiter lease is in force here,
  // so an automation's wish for it is being refused. `Now` says so only
  // when the hold displaced somebody; the control says so whenever it is
  // held, because this is where a person stands when they wonder why the
  // house is not driving it (docs/design.md#views-are-text).
  var hold = logic.holdOn(store.holds, entity.room, entity.name, r.aspect, Date.now());
  var heldMark = hold
    ? html`<span class="stale-mark" title="${'held by ' + hold.actor + ' at the ' + hold.priority + ' band'}">held</span>`
    : '';
  if (!r.control) {
    // a chart for a number, a timeline for a bool or a string
    return html`<div class="aspect-row described row-clickable" data-action="history-detail"${attrs}>
      <span class="aspect-label">${r.label}</span>${value}${heldMark}</div>`;
  }
  // The label taps through to history even though the row carries a
  // control. A commanded aspect is exactly the one whose past a reader
  // wants, and the row's click belongs to the control, so the label is
  // the only way to reach it. The control's own buttons still win:
  // closest('[data-action]') takes the innermost.
  var dial = r.control.kind === 'dial';
  return html`<div class="aspect-row described${dial ? ' dial-row' : ''}">
    <span class="aspect-label linked" data-action="history-detail"${attrs}>${r.label}${heldMark}</span>
    ${renderAspectControl(entity, r, true)}</div>`;
}

export function renderEntityDetailBody() {
  var entity = overlay.entity;
  var meta = titleCase(entity.room) + ' · ' + titleCase(entity.capability) +
    ((entity.features && entity.features.length) ? ' · ' + entity.features.map(titleCase).join(', ') : '');
  var badge = entity.write_mode === 'arbitrated' ? html`<span class="badge-arbitrated">ARBITRATED</span>` : '';
  var ownerLine = html`Owned by ${unitLabel(entity.owner)} · ${entity.write_mode}${badge}`;

  // Sections come from the adapter's aspect descriptor when it published
  // one (dashboard-logic.js, aspectPlan); an undescribed entity gets the
  // one flat State list. A described climate's declared commands render
  // in their own group, so its Controls row is not repeated.
  var descriptor = store.aspects[entity.name];
  var plan = logic.aspectPlan(entity, store.state, descriptor, !controlDisabled(entity), houseControls());
  var sections = plan.map(function (section) {
    var open = !section.collapsed || overlay.expanded[section.group];
    var head = section.collapsed
      ? html`<button class="group-toggle" data-action="toggle-group" data-group="${section.group}">
        ${section.label} (${section.rows.length}) ${open ? '\u25be' : '\u25b8'}</button>`
      : html`<div class="card-label" style="margin-top:20px;">${section.label}</div>`;
    return html`${head}${open ? section.rows.map(function (r) { return renderAspectRow(entity, r); }) : ''}`;
  });
  if (sections.length === 0) sections.push(html`<div class="card-label" style="margin-top:20px;">State</div><div class="aspect-row faint">no data</div>`);

  // A camera's detail is the live view (mounted imperatively in
  // renderOverlayContent — a custom element can't ride innerHTML), not a
  // Controls row: cameras take no commands. Its sections move to `rest`,
  // below the mount, so the player keeps its place above them.
  var middle = '', rest = html`${sections}`, camera = null;
  if (entity.capability === 'camera') {
    camera = entity.name;
    middle = html`<div class="card-label" style="margin-top:20px;">Live</div>`;
  } else if (entity.capability === 'sensor') {
    // A described sensor's readings are its sections, each tapping through
    // to its history; an undescribed one keeps the sparkline rows here.
    if (!descriptor) middle = html`<div class="card-label" style="margin-top:20px;">Controls</div>${widgetForSensor(entity, false)}`;
  } else if (!descriptor || BESPOKE_WIDGET[entity.capability]) {
    // A capability with its own widget (a toggle, a slider) keeps it here
    // whether or not the adapter described the rest of its aspects: the
    // descriptor never declares commands for the capability's vocabulary.
    middle = html`<div class="card-label" style="margin-top:20px;">Controls</div>
      ${entity.capability === 'climate' ? widgetForClimate(entity, true) : widgetForEntity(entity)}`;
  }
  var head = html`<div class="muted" style="margin-top:2px;">${meta}</div>
    <div class="muted" style="margin-top:6px;">${ownerLine}</div>${middle}`;
  // Without a camera everything is one half and the mount stays empty.
  return camera
    ? { title: entity.label, body: head, rest: rest, camera: camera }
    : { title: entity.label, body: html`${head}${rest}` };
}
