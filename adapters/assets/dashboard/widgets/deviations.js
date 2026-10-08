/* The deviations widget: what is out of the ordinary. */
import { html } from '../html.js';
import logic from '../logic.js';
import { scheduleRender, store } from '../store.js';

// The next time a drawn hold ends, as a single timer. The rows are sorted
// by deadline, so only the first one matters.
var holdTimer = null;

function scheduleHoldExpiry(deviations) {
  var next = deviations
    .filter(function (d) { return d.tag === 'hold' && d.until; })
    .map(function (d) { return d.until; })
    .sort(function (a, b) { return a - b; })[0];
  if (holdTimer) { clearTimeout(holdTimer); holdTimer = null; }
  if (!next) return;
  holdTimer = setTimeout(function () {
    holdTimer = null;
    scheduleRender();
  }, Math.max(250, next - Date.now() + 250));
}

// What is out of the ordinary. The rules are in dashboard-logic.js. Here
// each record's target is mapped to the tap handlers.
export function widgetDeviations() {
  var deviations = logic.computeDeviations(
    store.model, store.state, store.health, store.config, store.aspects, store.holds
  );
  // A hold ends on its own, and nothing may arrive on the bus to say so.
  // The arbiter republishes, but a browser that missed it, or whose clock
  // is a second ahead, would keep showing the row. Re-render at the
  // earliest drawn deadline, and the row is gone on the next render.
  scheduleHoldExpiry(deviations);

  function deviationAttrs(target) {
    if (target.type === 'unit') {
      return html`data-action="unit-detail" data-unit="${target.unit}"`;
    }
    if (target.type === 'entity') {
      return html`data-action="entity-detail" data-room="${target.room}" data-entity="${target.entity}"`;
    }
    if (target.type === 'setpoint') {
      return html`data-action="goto-setpoint" data-unit="${target.unit}" data-param="${target.param}"`;
    }
    return html`data-action="goto-rooms"`;
  }

  var devHtml;
  if (deviations.length === 0) {
    devHtml = html`<div class="equilibrium"><span class="eq-dot"></span>In equilibrium.</div>`;
  } else {
    devHtml = deviations.map(function (d) {
      return html`<div class="dev-row row-clickable" ${deviationAttrs(d.target)}>
        <div class="dev-main"><div class="title" title="${d.title}">${d.title}</div>
        ${d.detail ? html`<div class="detail">${d.detail}</div>` : ''}</div>
        ${d.button ? html`<button class="dev-btn" data-action="${d.button.action}">${d.button.label}</button>` : ''}
        <div class="dev-tag">${d.tag}</div></div>`;
    });
  }
  return html`<div class="card span-all"><div class="card-label">Out of the ordinary${deviations.length ? ' — ' + deviations.length : ''}</div>${devHtml}</div>`;
}
