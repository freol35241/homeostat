/* An entity's row, by its capability or by its adapter's aspect
 * descriptor, and the cards made of rows: the entity widget and the room
 * card. */
import { ensureHistory } from '../api.js';
import { buildChart } from '../charts.js';
import { html } from '../html.js';
import logic, { titleCase } from '../logic.js';
import { controlDisabled, houseControls, localState, presenceValue, stateValue, store } from '../store.js';
import { renderAspectControl, renderDial } from './controls.js';

function widgetForLight(entity) {
  var on = !!stateValue(entity.room, entity.name, 'on');
  var inert = controlDisabled(entity);
  var rows = [html`<div class="entity-row row-clickable" data-action="entity-detail" data-room="${entity.room}" data-entity="${entity.name}">
    <span class="entity-name" title="${entity.label}">${entity.label}</span>
    <span class="toggle ${on ? 'on' : ''}${inert ? ' disabled' : ''}"${inert ? '' : html` data-action="toggle-light"`} data-room="${entity.room}" data-entity="${entity.name}" data-value="${!on}"><span class="knob"></span></span>
    </div>`];

  if (entity.features && entity.features.indexOf('brightness') !== -1) {
    var b = stateValue(entity.room, entity.name, 'brightness');
    var bkey = entity.room + '/' + entity.name + '/brightness';
    var pct = typeof b === 'number' ? Math.round((b / 254) * 100) : 0;
    if (localState.sliderDrag[bkey] !== undefined) pct = localState.sliderDrag[bkey];
    var nudgeAttrs = html` data-room="${entity.room}" data-entity="${entity.name}"${inert || typeof b !== 'number' ? html` disabled` : ''}`;
    // A house may want this percent in steps of five. The light's own
    // controls read the same [[control]] entries as every other slider.
    var bstep = logic.declaredStep(houseControls(), { entity: entity.name, aspect: 'brightness' });
    rows.push(html`<div class="slider-row entity-sub"><span class="slabel">brightness</span>
      <button class="step-btn" data-action="brightness-step"${nudgeAttrs} data-delta="${-(bstep || 10)}" aria-label="dimmer">&minus;</button>
      <input type="range" min="0" max="100"${bstep ? html` step="${bstep}"` : ''} value="${pct}"${inert ? html` disabled` : ''} data-action="slider" data-kind="brightness" data-room="${entity.room}" data-entity="${entity.name}">
      <span class="sval">${pct}%</span>
      <button class="step-btn" data-action="brightness-step"${nudgeAttrs} data-delta="${bstep || 10}" aria-label="brighter">+</button></div>`);
  }
  if (entity.features && entity.features.indexOf('color_temp') !== -1) {
    var ct = stateValue(entity.room, entity.name, 'color_temp');
    var ctkey = entity.room + '/' + entity.name + '/color_temp';
    var mired = typeof ct === 'number' ? ct : 300;
    if (localState.sliderDrag[ctkey] !== undefined) mired = localState.sliderDrag[ctkey];
    var kelvin = Math.round(1e6 / mired / 100) * 100;
    rows.push(html`<div class="slider-row entity-sub"><span class="slabel">color temp</span>
      <input type="range" min="150" max="500" value="${mired}"${inert ? html` disabled` : ''} data-action="slider" data-kind="color_temp" data-room="${entity.room}" data-entity="${entity.name}">
      <span class="sval">${kelvin} K</span></div>`);
  }
  return html`${rows}`;
}

function widgetForPresence(entity) {
  var occ = presenceValue(entity);
  var seenKey = entity.room + '/' + entity.name;
  var lastSeen = store.lastSeen[seenKey];
  var label = occ ? 'Motion' : 'Clear';
  var extra = lastSeen ? html`<span class="faint" title="${new Date(lastSeen).toLocaleString()}"> &middot; ${relTime(lastSeen)}</span>` : '';
  return html`<div class="entity-row row-clickable" data-action="entity-detail" data-room="${entity.room}" data-entity="${entity.name}">
    <span class="entity-name" title="${entity.label}">${entity.label}</span>
    <span><span class="status-dot ${occ ? 'dot-on' : 'dot-off'}"></span>${label}${extra}</span>
    </div>`;
}

function widgetForLock(entity) {
  var locked = !!stateValue(entity.room, entity.name, 'locked');
  var badge = entity.write_mode === 'arbitrated' ? html`<span class="badge-arbitrated">ARBITRATED</span>` : '';
  return html`<div class="entity-row row-clickable" data-action="entity-detail" data-room="${entity.room}" data-entity="${entity.name}">
    <span class="entity-name" title="${entity.label}">${entity.label}${badge}</span>
    <span class="toggle ${locked ? 'on' : ''}${controlDisabled(entity) ? ' disabled' : ''}"${controlDisabled(entity) ? '' : html` data-action="toggle-lock"`} data-room="${entity.room}" data-entity="${entity.name}" data-value="${!locked}"><span class="knob"></span></span>
    </div>`;
}

export function widgetForClimate(entity, big) {
  var setpoint = stateValue(entity.room, entity.name, 'setpoint');
  var hasSetpoint = typeof setpoint === 'number';
  var readout = hasSetpoint ? setpoint.toFixed(1) + '°' : '—';
  var indoor = stateValue(entity.room, entity.name, 'indoor_temperature');
  var feed = stateValue(entity.room, entity.name, 'feed_temperature');
  var nowParts = [];
  if (typeof indoor === 'number') nowParts.push(indoor.toFixed(1) + '° now');
  if (typeof feed === 'number') nowParts.push(feed.toFixed(1) + '° feed');
  var badge = entity.write_mode === 'arbitrated' ? html`<span class="badge-arbitrated">ARBITRATED</span>` : '';
  var disabled = !(hasSetpoint && !controlDisabled(entity));
  if (big) {
    // The overlay and the dial widget: the vocabulary's setpoint on a
    // dial, with the capability's usual bounds. The adapter enforces its
    // own.
    return renderDial('climate-step', html` data-room="${entity.room}" data-entity="${entity.name}"`, {
      value: setpoint, display: readout, min: 5, max: 30, step: 0.5,
      disabled: disabled, current: nowParts.join(' · ')
    });
  }
  var disabledAttr = disabled ? html` disabled` : '';
  return html`<div class="entity-row row-clickable" data-action="entity-detail" data-room="${entity.room}" data-entity="${entity.name}">
    <span class="entity-name" title="${entity.label}">${entity.label}${badge}</span>
    <span style="display:flex;align-items:center;gap:10px;">
    ${nowParts.length ? html`<span class="faint">${nowParts.join(' · ')}</span>` : ''}
    <span class="stepper">
    <button class="step-btn" data-action="climate-step" data-room="${entity.room}" data-entity="${entity.name}" data-delta="-0.5"${disabledAttr}>&minus;</button>
    <span class="stepper-value">${readout}</span>
    <button class="step-btn" data-action="climate-step" data-room="${entity.room}" data-entity="${entity.name}" data-delta="0.5"${disabledAttr}>+</button>
    </span></span></div>`;
}

// A sensor's card: one sparkline row per reading, and tapping a row opens
// that aspect's history. The descriptor decides which readings appear and
// in what order (dashboard-logic.js, sensorCardPlan). A thermometer lists
// temperature and humidity, not its link quality. A sensor with several
// aspects gets a head row with the entity name, which opens the entity
// detail. Without it, every tap on the card would lead to a chart. The
// overlay is that detail, and it renders the rows without the head.
export function widgetForSensor(entity, withHead) {
  var rows = logic.sensorCardPlan(entity, store.state, store.aspects[entity.name]);
  if (rows.length === 0) {
    return html`<div class="entity-row"><span class="entity-name">${entity.label}</span><span class="faint">no data</span></div>`;
  }
  var multi = rows.length > 1;
  var attrs = html` data-room="${entity.room}" data-entity="${entity.name}"`;
  var head = multi && withHead !== false
    ? html`<div class="entity-row row-clickable" data-action="entity-detail"${attrs}>
      <span class="entity-name" title="${entity.label}">${entity.label}</span></div>`
    : '';
  return html`${head}${rows.map(function (r) {
    var sk = entity.name + '|' + r.aspect;
    ensureHistory(entity.name, r.aspect); // fetched once, on the first render while visible
    var s = localState.sparklines[sk];
    var spark = s && s.loaded ? buildChart('room-' + sk, s.points, { height: 28, sizeClass: 'chart-row', aspect: r.aspect, area: true, window: s.window }) : null;
    var name = multi ? r.label : entity.label;
    return html`<div class="entity-row row-clickable${multi ? ' sensor-reading' : ''}" data-action="history-detail"${attrs} data-aspect="${r.aspect}">
      <span class="entity-name" title="${name}">${name}</span>
      <span style="display:flex;align-items:center;gap:8px;">
      ${spark ? html`<span style="width:80px;">${spark}</span>` : ''}
      <span class="muted${r.stale ? ' stale' : ''}">${r.display}</span></span>
      </div>`;
  })}`;
}

function widgetGeneric(entity) {
  var prefix = 'home/state/' + entity.room + '/' + entity.name + '/';
  var rows = [];
  Object.keys(store.state).forEach(function (k) {
    if (k.indexOf(prefix) === 0) {
      var aspect = k.slice(prefix.length);
      rows.push(html`<div class="aspect-row">${aspect}: ${logic.formatAspect(aspect, null, store.state[k])}</div>`);
    }
  });
  if (rows.length === 0) rows.push(html`<div class="aspect-row faint">no data</div>`);
  return html`<div class="entity-row row-clickable" data-action="entity-detail" data-room="${entity.room}" data-entity="${entity.name}" style="flex-direction:column;align-items:flex-start;">
    <span class="entity-name" title="${entity.label}">${entity.label}</span>${rows}</div>`;
}

function widgetForCamera(entity) {
  // No poster image. A still frame would need transcoding H.264 to JPEG,
  // which means shipping a transcoder in the image for a thumbnail. The
  // camera path only remuxes (docs/design.md#cameras). A failing <img>
  // renders as a black rectangle that looks like a dark room, which is
  // worse than a plain note that the picture is one tap away. Motion is
  // still shown here, from the event plane, and that is what matters at a
  // glance. The live view opens in the detail overlay, so the page never
  // runs one stream per camera all the time.
  var motion = stateValue(entity.room, entity.name, 'motion');
  var badge = motion === true ? html`<span class="badge-motion">MOTION</span>` : '';
  return html`<div class="entity-row row-clickable" data-action="entity-detail" data-room="${entity.room}" data-entity="${entity.name}" style="flex-direction:column;align-items:stretch;gap:6px;">
    <span style="display:flex;justify-content:space-between;align-items:center;">
    <span class="entity-name" title="${entity.label}">${entity.label}</span>${badge}</span>
    <span class="camera-tap">&#9654; tap to view</span>
    </div>`;
}

// The room-card row for an entity whose adapter published an aspect
// descriptor. The name and the first family control share one line, with
// the headline readings below. This avoids the undescribed climate
// widget's layout, which wraps the readout around a stepper in a narrow
// column. dashboard-logic.js's cardPlan picks the headline readings.
function widgetForDescribed(entity) {
  var plan = logic.cardPlan(entity, store.state, store.aspects[entity.name], !controlDisabled(entity), houseControls());
  var badge = entity.write_mode === 'arbitrated' ? html`<span class="badge-arbitrated">ARBITRATED</span>` : '';
  var control = plan.controls.length ? renderAspectControl(entity, plan.controls[0]) : '';
  var readings = plan.readings.map(function (r, i) {
    return html`${i ? html`<span class="sep">&middot;</span>` : ''}<span class="card-reading${r.stale ? ' stale' : ''}" title="${r.aspect}">
      ${r.label} <b>${r.display}</b></span>`;
  });
  // The badge goes on the readings line. Inside the truncated name it
  // would be cut off in a narrow column, and the head line is for the
  // control.
  return html`<div class="entity-row described-card row-clickable" data-action="entity-detail" data-room="${entity.room}" data-entity="${entity.name}">
    <div class="described-head"><span class="entity-name" title="${entity.label}">${entity.label}</span>${control}</div>
    ${readings.length || badge ? html`<div class="described-readings">${readings}${badge ? html`<span class="card-badge">${badge}</span>` : ''}</div>` : ''}
    </div>`;
}

export function widgetForEntity(entity) {
  var described = !!store.aspects[entity.name];
  if (described && (entity.capability === 'climate' || !BESPOKE_WIDGET[entity.capability])) return widgetForDescribed(entity);
  if (entity.capability === 'light' || entity.capability === 'switch') return widgetForLight(entity);
  if (entity.capability === 'presence') return widgetForPresence(entity);
  if (entity.capability === 'lock') return widgetForLock(entity);
  if (entity.capability === 'climate') return widgetForClimate(entity);
  if (entity.capability === 'sensor') return widgetForSensor(entity);
  if (entity.capability === 'camera') return widgetForCamera(entity);
  return widgetGeneric(entity);
}

var CAP_ORDER = { light: 0, lock: 1, presence: 2, climate: 3, sensor: 4, camera: 5 };

// Capabilities whose card widget is the capability's own (a toggle, a
// sparkline, a poster) rather than the descriptor-driven row.
export var BESPOKE_WIDGET = { light: true, switch: true, presence: true, lock: true, sensor: true, camera: true };

function capRank(entity) {
  var r = CAP_ORDER[entity.capability];
  return r === undefined ? 6 : r;
}

export function relTime(ms) {
  var diff = Math.max(0, Date.now() - ms);
  var mins = Math.round(diff / 60000);
  if (mins < 1) return 'just now';
  if (mins === 1) return '1 min ago';
  if (mins < 60) return mins + ' min ago';
  var hrs = Math.round(mins / 60);
  if (hrs === 1) return '1 hr ago';
  return hrs + ' hr ago';
}

// An entity's own row (its control or its readings) as a card.
export function widgetEntity(entity) {
  return html`<div class="card">${widgetForEntity(entity)}</div>`;
}

// The room card: every entity in the room, controls first.
export function renderRoomCard(room, ents) {
  var sorted = ents.slice().sort(function (a, b) { return capRank(a) - capRank(b); });
  var title = room === 'global' ? 'House' : titleCase(room);
  return html`<div class="card room-card"><h3 title="${title}">${title}</h3>${sorted.map(widgetForEntity)}</div>`;
}
