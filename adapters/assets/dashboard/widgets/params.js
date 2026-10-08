/* A unit's parameters, one row each, with the control for its type,
 * sending to /api/param. Also the params widget, which shows a unit's
 * family-editable parameters as a card. */
import { byId, html } from '../html.js';
import logic from '../logic.js';
import { houseControls, localState, store, unitLabel } from '../store.js';

// A unit's family-editable parameters as one card.
export function widgetParams(unit) {
  var params = unit.params || {};
  var rows = Object.keys(params).filter(function (p) { return familyEditable(params[p]); })
    .map(function (p) { return renderParamRow(unit.name, p, params[p], true); });
  return html`<div class="card"><div class="card-label">${unit.label}</div>
    ${rows.length ? rows : html`<div class="empty-hint">No family-editable parameters.</div>`}</div>`;
}

function paramConstraintCaption(p) {
  var c = p.constraint || {};
  if (p.type === 'time' && (c.after || c.before)) return (c.after || '') + '–' + (c.before || '');
  if ((p.type === 'int' || p.type === 'float') && (c.min !== undefined || c.max !== undefined)) {
    return (c.min !== undefined ? c.min : '') + '–' + (c.max !== undefined ? c.max : '');
  }
  if (p.type === 'enum' && p.values) return p.values.join(' / ');
  return '';
}

function currentParamValue(unitName, pname, p) {
  var key = 'home/config/' + unitName + '/' + pname;
  if (key in store.config) return store.config[key];
  return p.default;
}

function renderParamControl(unitName, pname, p) {
  var value = currentParamValue(unitName, pname, p);
  var id = 'param-' + unitName + '-' + pname;
  if (p.type === 'time') {
    return html`<input type="time" id="${id}" value="${value}" data-action="param-time" data-unit="${unitName}" data-param="${pname}">`;
  }
  if (p.type === 'bool') {
    var on = !!value;
    return html`<span class="toggle ${on ? 'on' : ''}" data-action="param-toggle" data-unit="${unitName}" data-param="${pname}" data-value="${!on}"><span class="knob"></span></span>`;
  }
  if (p.type === 'enum' && p.values && p.values.length > logic.SELECT_ABOVE) {
    return html`<select class="control" id="${id}" data-action="param-select" data-unit="${unitName}" data-param="${pname}">${p.values.map(function (v) {
      return html`<option value="${v}"${v === value ? html` selected` : ''}>${v}</option>`;
    })}</select>`;
  }
  if (p.type === 'enum' && p.values) {
    return html`<div class="seg">${p.values.map(function (v) {
      return html`<button data-action="param-enum" data-unit="${unitName}" data-param="${pname}" data-value="${v}" class="${v === value ? 'active' : ''}">${v}</button>`;
    })}</div>`;
  }
  if (p.type === 'int' || p.type === 'float') {
    var c = p.constraint || {};
    var min = c.min !== undefined ? c.min : 0;
    var max = c.max !== undefined ? c.max : 100;
    var declared = logic.declaredStep(houseControls(), { unit: unitName, param: pname });
    var step = declared ? String(declared) : (p.type === 'float' ? 'any' : '1');
    var slideKey = unitName + '|' + pname;
    var shown = localState.sliderDrag[slideKey] !== undefined ? localState.sliderDrag[slideKey] : value;
    return html`<div class="slider-row" style="padding-left:0;">
      <input type="range" min="${min}" max="${max}" step="${step}" value="${shown}" data-action="param-slider" data-unit="${unitName}" data-param="${pname}">
      <span class="sval">${shown}</span></div>`;
  }
  // string fallback
  return html`<input type="text" id="${id}" value="${value === undefined ? '' : value}" data-action="param-text" data-unit="${unitName}" data-param="${pname}">`;
}

// The parameter's default from the manifest, so a value moved away from it
// can be put back by reading it rather than remembering it. A slider is
// easy to move by accident and has no undo. The default is the next best
// thing, and the page already has it from the house text. A param with no
// default shows nothing here.
function paramDefaultCaption(p) {
  if (p.default === undefined || p.default === null) return '';
  return 'default ' + String(p.default);
}

export function familyEditable(p) { return p.editable_by === 'family'; }

// Shared by the Setpoints view and the unit-detail overlay. A param the
// family cannot edit shows its live value and the manifest default
// instead of a control. It is visible but not writable, and /api/param
// enforces that.
export function renderParamRow(unitName, pname, p, bare) {
  var body;
  if (familyEditable(p)) {
    body = html`<div>${renderParamControl(unitName, pname, p)}</div>`;
  } else {
    var value = currentParamValue(unitName, pname, p);
    var offDefault = JSON.stringify(value) !== JSON.stringify(p.default);
    body = html`<div class="param-readonly">${String(value)}
      ${offDefault ? html` <span class="faint">(default ${String(p.default)})</span>` : ''} <span class="tier-badge">${p.editable_by || 'owner'}</span></div>`;
  }
  return html`<div class="param-row" id="paramrow-${unitName}-${pname}"><div class="param-head">
    <span class="name" title="${unitLabel(unitName)} · ${pname}">${bare ? '' : unitLabel(unitName) + ' · '}${pname}</span></div>
    ${body}
    <div class="param-caption" title="${JSON.stringify(p.constraint || {})}">
    ${[paramConstraintCaption(p), paramDefaultCaption(p)].filter(Boolean).join(' \u00b7 ')}
    </div></div>`;
}

export function flashParamRow(unitName, pname) {
  var rowEl = byId('paramrow-' + unitName + '-' + pname);
  if (!rowEl) return;
  rowEl.classList.remove('flash-highlight');
  void rowEl.offsetWidth; // restart the animation
  rowEl.classList.add('flash-highlight');
}
