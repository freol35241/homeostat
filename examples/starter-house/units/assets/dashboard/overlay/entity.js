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

// One planned row of an entity's detail: the label, the value, and the
// control the descriptor declared. The controls have the same shapes as
// the param controls but send to /api/cmd instead of /api/param. Tapping
// a numeric row without a control opens its history.
function renderAspectRow(entity, r) {
  var attrs = html` data-room="${entity.room}" data-entity="${entity.name}" data-aspect="${r.aspect}"`;
  var value = aspectValueSpan(r);
  // An arbiter lease is in force on this aspect, so automations' commands
  // for it are being refused. `Now` shows this only when the hold
  // displaced somebody. The control shows it whenever the aspect is held,
  // because this is where a person looks when they wonder why the house
  // is not driving it (docs/design.md#views-are-text).
  var hold = logic.holdOn(store.holds, entity.room, entity.name, r.aspect, Date.now());
  var heldMark = hold
    ? html`<span class="stale-mark" title="${'held by ' + hold.actor + ' at the ' + hold.priority + ' band'}">held</span>`
    : '';
  if (!r.control) {
    // a chart for a number, a timeline for a bool or a string
    return html`<div class="aspect-row described row-clickable" data-action="history-detail"${attrs}>
      <span class="aspect-label">${r.label}</span>${value}${heldMark}</div>`;
  }
  // Tapping the label opens the history, even though the row has a
  // control. A reader often wants the history of a commanded aspect, and
  // the row's click belongs to the control, so the label is the only way
  // in. The control's own buttons take precedence, because
  // closest('[data-action]') finds the innermost.
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

  // Sections come from the adapter's aspect descriptor, if it published
  // one (dashboard-logic.js, aspectPlan). An undescribed entity gets one
  // flat State list. A described climate's declared commands render in
  // their own group, so its Controls row is not repeated.
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

  // A camera's detail is the live view, not a Controls row, because
  // cameras take no commands. The view is mounted in renderOverlayContent,
  // because a custom element cannot be set through innerHTML. Its sections
  // move to `rest`, below the mount, so the player stays above them.
  var middle = '', rest = html`${sections}`, camera = null;
  if (entity.capability === 'camera') {
    camera = entity.name;
    middle = html`<div class="card-label" style="margin-top:20px;">Live</div>`;
  } else if (entity.capability === 'sensor') {
    // A described sensor's readings are its sections, and tapping each
    // opens its history. An undescribed sensor keeps the sparkline rows
    // here.
    if (!descriptor) middle = html`<div class="card-label" style="margin-top:20px;">Controls</div>${widgetForSensor(entity, false)}`;
  } else if (!descriptor || BESPOKE_WIDGET[entity.capability]) {
    // A capability with its own widget (a toggle, a slider) keeps it here,
    // whether or not the adapter described its other aspects. The
    // descriptor does not declare commands for the capability's own
    // aspects.
    middle = html`<div class="card-label" style="margin-top:20px;">Controls</div>
      ${entity.capability === 'climate' ? widgetForClimate(entity, true) : widgetForEntity(entity)}`;
  }
  var head = html`<div class="muted" style="margin-top:2px;">${meta}</div>
    <div class="muted" style="margin-top:6px;">${ownerLine}</div>${middle}`;
  // Without a camera everything goes in one half, and the mount stays
  // empty.
  return camera
    ? { title: entity.label, body: head, rest: rest, camera: camera }
    : { title: entity.label, body: html`${head}${rest}` };
}
