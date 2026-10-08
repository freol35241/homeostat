/* The dashboard page's entry module, loaded by dashboard.html. It holds
 * the page's render, the delegated taps, edits and drags (each passed to
 * the module that owns it), the timers, and the startup.
 *
 * The page is rendered in the browser from the house's text and its live
 * state. docs/design.md#dashboard explains why, and #the-page how it is
 * tested. */
import { VideoRTC } from '../video-rtc.js';
import { connectWs, loadModel } from './api.js';
import { currentView, renderShell } from './chrome.js';
import { announceOutcome, markPendingControls, pending, recent, sendCmd, sendLightsOff, sendParam, stepBase, stepCmd } from './commands.js';
import logic from './logic.js';
import { openEntityDetail } from './overlay/entity.js';
import { fetchHistoryRange, fetchIssues, fetchSourceEvents, fetchSources, openHistoryDetail } from './overlay/history.js';
import { closeOverlay, overlayWide, renderOverlayContent, setOverlayWide } from './overlay/panel.js';
import { openViewText } from './overlay/text.js';
import { openUnitDetail } from './overlay/unit.js';
import { descriptorField, findEntity, localState, overlay, scheduleRender, setRenderer, store } from './store.js';
import { renderView } from './views/index.js';
import { flashParamRow } from './widgets/params.js';

// The vendored go2rtc player (assets/README.md). Only MSE is used, because
// WebRTC can't go through the dashboard's /api/camera proxy
// (docs/design.md#cameras).
customElements.define('video-rtc', VideoRTC);

function render() {
  renderShell();
  renderView();
  if (overlay.open) renderOverlayContent();
  markPendingControls();
}

document.addEventListener('click', function (e) {
  var el = e.target.closest('[data-action]');
  if (!el) return;
  var action = el.getAttribute('data-action');

  if (action === 'lights-off') {
    sendLightsOff();
    return;
  }
  if (action === 'toggle-light') {
    var room = el.getAttribute('data-room');
    var entity = el.getAttribute('data-entity');
    var value = el.getAttribute('data-value') === 'true';
    sendCmd(room, entity, 'on', value);
    return;
  }
  if (action === 'toggle-lock') {
    var lockRoom = el.getAttribute('data-room');
    var lockEntity = el.getAttribute('data-entity');
    var lockValue = el.getAttribute('data-value') === 'true';
    sendCmd(lockRoom, lockEntity, 'locked', lockValue);
    return;
  }
  if (action === 'climate-step') {
    var stepRoom = el.getAttribute('data-room');
    var stepEntity = el.getAttribute('data-entity');
    var current = stepBase(stepRoom, stepEntity, 'setpoint');
    if (typeof current !== 'number') return; // no fabricated default to step from
    var delta = parseFloat(el.getAttribute('data-delta'));
    var next = Math.round((current + delta) * 10) / 10;
    stepCmd(stepRoom, stepEntity, 'setpoint', next);
    return;
  }
  if (action === 'brightness-step') {
    var bRoom = el.getAttribute('data-room');
    var bEntity = el.getAttribute('data-entity');
    var bCurrent = stepBase(bRoom, bEntity, 'brightness');
    if (typeof bCurrent !== 'number') return;
    var bPct = Math.max(0, Math.min(100, Math.round(bCurrent / 254 * 100) + parseInt(el.getAttribute('data-delta'), 10)));
    stepCmd(bRoom, bEntity, 'brightness', Math.round(bPct / 100 * 254));
    return;
  }
  if (action === 'aspect-step') {
    var aRoom = el.getAttribute('data-room');
    var aEntity = el.getAttribute('data-entity');
    var aAspect = el.getAttribute('data-aspect');
    var aCurrent = stepBase(aRoom, aEntity, aAspect);
    if (typeof aCurrent !== 'number') return; // no fabricated default to step from
    var aNext = Math.round((aCurrent + parseFloat(el.getAttribute('data-delta'))) * 100) / 100;
    var aField = descriptorField(findEntity(aRoom, aEntity) || {}, aAspect);
    var aBounds = (aField && aField.command && aField.command.constraint) || {};
    if (typeof aBounds.min === 'number') aNext = Math.max(aBounds.min, aNext);
    if (typeof aBounds.max === 'number') aNext = Math.min(aBounds.max, aNext);
    stepCmd(aRoom, aEntity, aAspect, aNext);
    return;
  }
  if (action === 'aspect-enum') {
    sendCmd(el.getAttribute('data-room'), el.getAttribute('data-entity'), el.getAttribute('data-aspect'),
      JSON.parse(el.getAttribute('data-value')));
    return;
  }
  if (action === 'toggle-relations') {
    var relUnit = el.getAttribute('data-unit');
    localState.relations[relUnit] = !localState.relations[relUnit];
    render();
    return;
  }
  if (action === 'toggle-group') {
    var group = el.getAttribute('data-group');
    overlay.expanded[group] = !overlay.expanded[group];
    renderOverlayContent();
    return;
  }
  if (action === 'param-toggle') {
    var unit = el.getAttribute('data-unit');
    var param = el.getAttribute('data-param');
    var val = el.getAttribute('data-value') === 'true';
    sendParam(unit, param, val, function () { scheduleRender(); });
    return;
  }
  if (action === 'param-enum') {
    var unit2 = el.getAttribute('data-unit');
    var param2 = el.getAttribute('data-param');
    var val2 = el.getAttribute('data-value');
    sendParam(unit2, param2, val2, function () { scheduleRender(); });
    return;
  }
  if (action === 'zone-filter') {
    store.roomsFilter = el.getAttribute('data-zone');
    renderView();
    return;
  }
  if (action === 'history-detail') {
    openHistoryDetail(el.getAttribute('data-room'), el.getAttribute('data-entity'), el.getAttribute('data-aspect'));
    return;
  }
  if (action === 'entity-detail') {
    openEntityDetail(el.getAttribute('data-room'), el.getAttribute('data-entity'));
    return;
  }
  if (action === 'unit-detail') {
    openUnitDetail(el.getAttribute('data-unit'));
    return;
  }
  if (action === 'view-text') {
    openViewText(el.getAttribute('data-view-name'));
    return;
  }
  // A deviation tap opens whichever view shows the item
  // (dashboard-logic.js, viewFor). If no view does, the unit overlay has
  // every param, and Not shown has every unplaced light.
  if (action === 'goto-rooms') {
    store.view = logic.viewFor({ type: 'rooms' }, logic.viewsOf(store.model)) || 'notshown';
    renderShell();
    renderView();
    return;
  }
  if (action === 'goto-setpoint') {
    var gUnit = el.getAttribute('data-unit');
    var gParam = el.getAttribute('data-param');
    var gView = logic.viewFor({ type: 'setpoint', unit: gUnit }, logic.viewsOf(store.model));
    if (!gView) {
      openUnitDetail(gUnit);
      return;
    }
    store.view = gView;
    renderShell();
    renderView();
    flashParamRow(gUnit, gParam);
    return;
  }
  if (action === 'chart-layer') {
    // Choosing one layer hides the other, because two sets of thin grey
    // lines on one chart look like one set.
    overlay.layer = el.getAttribute('data-layer') || '';
    // A pin belongs to a braid or a legend that may no longer be drawn.
    overlay.pinned = null;
    overlay.sourcePinned = null;
    if (overlay.layer === 'forecasts' && !overlay.issues.length && !overlay.issuesLoading) {
      fetchIssues(overlay.entity.name, overlay.aspect, overlay.range);
    }
    if (overlay.layer === 'sources') {
      fetchSources(overlay.entity, overlay.aspect, overlay.range);
      fetchSourceEvents(overlay.entity, overlay.aspect, overlay.range);
    }
    renderOverlayContent();
    return;
  }
  if (action === 'range-chip') {
    var hrs = parseInt(el.getAttribute('data-hours'), 10);
    overlay.range = hrs;
    fetchHistoryRange(overlay.entity.name, overlay.aspect, hrs, overlay.shape);
    if (overlay.layer === 'sources') {
      fetchSources(overlay.entity, overlay.aspect, hrs);
      fetchSourceEvents(overlay.entity, overlay.aspect, hrs);
    }
    // A different window has a different set of issues, and a pin on a
    // forecast that is no longer drawn would highlight nothing.
    if (overlay.layer === 'forecasts') {
      overlay.issues = [];
      overlay.pinned = null;
      fetchIssues(overlay.entity.name, overlay.aspect, hrs);
    }
    renderOverlayContent();
    return;
  }
  if (action === 'overlay-wide') {
    setOverlayWide(!overlayWide());
    return;
  }
  if (action === 'overlay-close') {
    closeOverlay();
    return;
  }
});

document.addEventListener('change', function (e) {
  var el = e.target;
  if (!el.matches) return;

  if (el.matches('[data-action="aspect-select"]')) {
    var chosen;
    try { chosen = JSON.parse(el.value); } catch (err) { return; } // a descriptor value that is not JSON is not a command
    sendCmd(el.getAttribute('data-room'), el.getAttribute('data-entity'), el.getAttribute('data-aspect'), chosen);
    return;
  }
  if (el.matches('[data-action="param-select"]')) {
    sendParam(el.getAttribute('data-unit'), el.getAttribute('data-param'), el.value, function () { scheduleRender(); });
    return;
  }

  if (el.matches('[data-action="slider"]')) {
    var kind = el.getAttribute('data-kind');
    var room = el.getAttribute('data-room');
    var entity = el.getAttribute('data-entity');
    var key = room + '/' + entity + '/' + kind;
    var pct = parseInt(el.value, 10);
    var sendValue;
    if (kind === 'brightness') {
      sendValue = Math.round((pct / 100) * 254);
      sendCmd(room, entity, 'brightness', sendValue);
    } else if (kind === 'color_temp') {
      sendValue = pct; // el.value already the mired value for color_temp
      sendCmd(room, entity, 'color_temp', sendValue);
    }
    delete localState.sliderDrag[key];
    scheduleRender();
    return;
  }
  if (el.matches('[data-action="param-slider"]')) {
    var unit = el.getAttribute('data-unit');
    var param = el.getAttribute('data-param');
    var slideKey = unit + '|' + param;
    var v = el.step === 'any' ? parseFloat(el.value) : parseInt(el.value, 10);
    sendParam(unit, param, v, function () { scheduleRender(); });
    delete localState.sliderDrag[slideKey];
    scheduleRender();
    return;
  }
  if (el.matches('[data-action="aspect-slider"]')) {
    var sRoom = el.getAttribute('data-room');
    var sEntity = el.getAttribute('data-entity');
    var sAspect = el.getAttribute('data-aspect');
    sendCmd(sRoom, sEntity, sAspect, el.step === 'any' ? parseFloat(el.value) : parseInt(el.value, 10));
    delete localState.sliderDrag[sEntity + '|' + sAspect];
    scheduleRender();
    return;
  }
  if (el.matches('[data-action="param-time"]')) {
    var unit2 = el.getAttribute('data-unit');
    var param2 = el.getAttribute('data-param');
    var prevKey = 'home/config/' + unit2 + '/' + param2;
    var prevValue = store.config[prevKey];
    sendParam(unit2, param2, el.value, function () {
      el.value = prevValue !== undefined ? prevValue : el.value;
    });
    return;
  }
  if (el.matches('[data-action="param-text"]')) {
    var unit3 = el.getAttribute('data-unit');
    var param3 = el.getAttribute('data-param');
    var prevKey3 = 'home/config/' + unit3 + '/' + param3;
    var prevValue3 = store.config[prevKey3];
    sendParam(unit3, param3, el.value, function () {
      el.value = prevValue3 !== undefined ? prevValue3 : el.value;
    });
    return;
  }
});

// live drag feedback: keep sliders responsive without re-render clobbering them
document.addEventListener('input', function (e) {
  var el = e.target;
  if (!el.matches) return;

  if (el.matches('[data-action="slider"]')) {
    var kind = el.getAttribute('data-kind');
    var room = el.getAttribute('data-room');
    var entity = el.getAttribute('data-entity');
    var key = room + '/' + entity + '/' + kind;
    var pct = parseInt(el.value, 10);
    localState.sliderDrag[key] = pct;
    var valEl = el.parentElement.querySelector('.sval');
    if (valEl) {
      if (kind === 'brightness') {
        valEl.textContent = pct + '%';
      } else {
        var kelvin = Math.round(1e6 / pct / 100) * 100;
        valEl.textContent = kelvin + ' K';
      }
    }
    return;
  }
  if (el.matches('[data-action="param-slider"]')) {
    var unit = el.getAttribute('data-unit');
    var param = el.getAttribute('data-param');
    var slideKey = unit + '|' + param;
    localState.sliderDrag[slideKey] = el.value;
    var valEl2 = el.parentElement.querySelector('.sval');
    if (valEl2) valEl2.textContent = el.value;
    return;
  }
  if (el.matches('[data-action="aspect-slider"]')) {
    localState.sliderDrag[el.getAttribute('data-entity') + '|' + el.getAttribute('data-aspect')] = el.value;
    var valEl3 = el.parentElement.querySelector('.sval');
    if (valEl3) valEl3.textContent = el.value;
    return;
  }
});

// The relative-time ticker.
setInterval(function () {
  if (currentView() !== 'health') scheduleRender(); // every other view shows relative times
}, 30000);

/* Some commands get no answer at all, for example when the adapter is not
 * listening or the device is off. "No confirmation" is a real outcome,
 * and the page must not show it as success. */
setInterval(function () {
  var expired = logic.expirePending(pending, Date.now());
  expired.forEach(announceOutcome);
  if (expired.length) scheduleRender();
  // An ended command's line is removed when its time is up, and no delta
  // triggers that. Only the lines are redrawn, and only when one was
  // removed. Rebuilding the view could replace a button between a press
  // and its release, and the tap would be lost. Redrawing the lines every
  // second would make a screen reader announce each one again.
  else if (logic.pruneRecent(recent, Date.now())) markPendingControls();
}, 1000);

setRenderer(render);
render();
loadModel();
connectWs();
