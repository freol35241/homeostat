/* The controls an aspect descriptor declares, wired to /api/cmd: a
 * stepper, a dial, a slider, segments or a select, in a card's compact form
 * or the overlay's full one. */
import { html } from '../html.js';
import logic from '../logic.js';
import { descriptorField, houseControls, localState, store } from '../store.js';

export function aspectValueSpan(r) {
  return html`<span class="aspect-value${r.stale ? ' stale' : ''}" title="${r.aspect}">${r.display}</span>
    ${r.stale ? html`<span class="stale-mark">stale</span>` : ''}`;
}

// A thermostat dial: the target on a 240° arc from min to max, the
// current reading beneath it when the entity has one, ± at the arc's
// ends. The buttons carry the same action attributes as the compact
// stepper, so the pending/held/timeout stages apply unchanged.
export function renderDial(action, attrs, o) {
  var span = (o.max - o.min) || 1;
  var f = typeof o.value === 'number' ? Math.max(0, Math.min(1, (o.value - o.min) / span)) : 0;
  var point = function (deg) {
    var a = deg * Math.PI / 180;
    return { x: 60 + 46 * Math.cos(a), y: 60 + 46 * Math.sin(a) };
  };
  var arc = function (fromDeg, toDeg) {
    var a = point(fromDeg), b = point(toDeg);
    return 'M ' + a.x.toFixed(1) + ' ' + a.y.toFixed(1) + ' A 46 46 0 ' + (toDeg - fromDeg > 180 ? 1 : 0) + ' 1 ' + b.x.toFixed(1) + ' ' + b.y.toFixed(1);
  };
  var end = -210 + 240 * f;
  var knob = point(end);
  var disabled = o.disabled ? html` disabled` : '';
  return html`<div class="dial">
    <svg viewBox="0 0 120 120" aria-hidden="true"><path class="dial-track" d="${arc(-210, 30)}"/>
    ${f > 0 ? html`<path class="dial-arc" d="${arc(-210, end)}"/>` : ''}
    <circle class="dial-knob" cx="${knob.x.toFixed(1)}" cy="${knob.y.toFixed(1)}" r="6"/></svg>
    <div class="dial-readout"><div class="dial-target">${o.display}</div>
    ${o.current ? html`<div class="dial-current">${o.current}</div>` : ''}</div>
    <div class="dial-buttons">
    <button class="step-btn" data-action="${action}"${attrs} data-delta="${-o.step}"${disabled} aria-label="lower">&minus;</button>
    <button class="step-btn" data-action="${action}"${attrs} data-delta="${o.step}"${disabled} aria-label="raise">+</button>
    </div></div>`;
}

// The first temperature reading the entity has that is not itself the
// commanded aspect — what the dial shows as "now".
function currentTemperatureOf(entity, except) {
  var rows = [];
  logic.aspectPlan(entity, store.state, store.aspects[entity.name], false, houseControls()).forEach(function (s) {
    s.rows.forEach(function (r) { rows.push(r); });
  });
  var field = function (r) { return descriptorField(entity, r.aspect); };
  var hit = rows.filter(function (r) {
    return r.aspect !== except && r.numeric && !r.control &&
      ((field(r) && field(r).kind === 'temperature') || (!field(r) && r.aspect.indexOf('temperature') !== -1));
  })[0];
  return hit ? hit.display + ' now' : '';
}

// The control markup for a planned row with a control: the param-control
// shapes wired to /api/cmd. `big` is the overlay and the dial widget; a
// room card gets every control's compact form (a dial's is a stepper).
export function renderAspectControl(entity, r, big) {
  var attrs = html` data-room="${entity.room}" data-entity="${entity.name}" data-aspect="${r.aspect}"`;
  var value = aspectValueSpan(r);
  var c = r.control;
  var control;
  var disabledAttr = c.disabled ? html` disabled` : '';
  if (c.kind === 'readonly') {
    control = html`${value}<span class="tier-badge">${c.tier}</span>`;
  } else if (c.kind === 'dial' && big) {
    control = renderDial('aspect-step', attrs, {
      value: r.value, display: r.display, min: c.min, max: c.max, step: c.step,
      disabled: c.disabled || !r.numeric, current: currentTemperatureOf(entity, r.aspect)
    });
  } else if (c.kind === 'select') {
    control = html`<select class="control" data-action="aspect-select"${attrs}${disabledAttr}>${c.values.map(function (v) {
      return html`<option value="${JSON.stringify(v.value)}"${v.value === r.value ? html` selected` : ''}>${v.label}</option>`;
    })}</select>`;
  } else if (c.kind === 'stepper' || c.kind === 'dial') {
    var stepOff = r.numeric && !c.disabled ? '' : html` disabled`;
    control = html`<span class="stepper">
      <button class="step-btn" data-action="aspect-step"${attrs} data-delta="${-c.step}"${stepOff}>&minus;</button>
      <span class="stepper-value">${r.display}</span>
      <button class="step-btn" data-action="aspect-step"${attrs} data-delta="${c.step}"${stepOff}>+</button></span>`;
  } else if (c.kind === 'segment') {
    control = html`<div class="seg">${c.values.map(function (v) {
      return html`<button data-action="aspect-enum"${attrs} data-value="${JSON.stringify(v.value)}" class="${v.value === r.value ? 'active' : ''}"${disabledAttr}>${v.label}</button>`;
    })}</div>`;
  } else if (c.kind === 'slider') {
    var slideKey = entity.name + '|' + r.aspect;
    var shown = localState.sliderDrag[slideKey] !== undefined ? localState.sliderDrag[slideKey] : r.value;
    var nudgeOff = c.coarse && r.numeric && !c.disabled ? '' : html` disabled`;
    control = html`<div class="slider-row" style="padding-left:0;">
      <button class="step-btn" data-action="aspect-step"${attrs} data-delta="${-c.coarse}"${nudgeOff} aria-label="lower">&minus;</button>
      <input type="range" min="${c.min}" max="${c.max}" step="${c.step}" value="${shown}" data-action="aspect-slider"${attrs}${disabledAttr}>
      <span class="sval">${shown}</span>
      <button class="step-btn" data-action="aspect-step"${attrs} data-delta="${c.coarse}"${nudgeOff} aria-label="raise">+</button></div>`;
  } else {
    control = value;
  }
  return control;
}
