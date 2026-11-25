/* What the page asks of the house: commands through /api/cmd, parameter
 * writes through /api/param, the whole-house lights-off, and the stage a
 * command is in, as its control shows it. */
import { toast } from './chrome.js';
import logic from './logic.js';
import { baseAspect, descriptorField, findEntity, scheduleRender, stateValue, store, unitLabel } from './store.js';

/* In-flight commands, keyed room/entity/aspect. They are kept outside
 * `store`, which holds the house's state as the bus reports it. A pending
 * command is local to this browser, and nothing on the bus knows this tab
 * tapped a button. */
export var pending = {};

/* A command passes through stages, and the page shows which one it is
 * in. It is "asked" from the tap. It ends as confirmed by readback,
 * adjusted when the device settled elsewhere, held by the arbiter,
 * rejected by the adapter, unheard when nothing subscribes to its key, or
 * unconfirmed when nothing answers. The envelope's id, returned by
 * /api/cmd, ties an event to the command it ended. The stage is shown on
 * the control itself (markPendingControls) and stays for a while after
 * the command ends (`recent`). The toast is only for an outcome whose
 * control is no longer on screen. */
export var recent = {};

// key -> timer: a stepper still being tapped (stepCmd)
var drafts = {};

var STEP_SETTLE_MS = 600;

var cmdSeq = 0;

/* Resolves to whether the command went onto the bus with something
 * listening, so a caller can tell a refusal from a send. */
export function sendCmd(room, entity, aspect, value) {
  var key = logic.pendingKey(room, entity, aspect);
  clearTimeout(drafts[key]);
  delete drafts[key];
  delete recent[key];
  var seq = ++cmdSeq;
  logic.trackCommand(pending, trackedCommand(room, entity, aspect, value, seq), Date.now(), 'sending');
  scheduleRender();
  return postFamily('/api/cmd', { room: room, entity: entity, aspect: aspect, value: value }).then(function (r) {
    return r.json();
  }).then(function (body) {
    var outcome = logic.commandSent(pending, key, seq, body);
    if (outcome) announceOutcome(outcome);
    scheduleRender();
    return !outcome;
  }).catch(function (e) {
    // Refused before it reached the bus, so there is nothing to wait for.
    var entry = pending[key];
    if (entry && entry.outcome === 'sending' && entry.seq === seq) delete pending[key];
    announceOutcome({ key: key, outcome: 'failed', value: value, reason: logic.errText(e) });
    scheduleRender();
    return false;
  });
}

/* Forget how a command on this control ended, so the line under it
 * shows the next one. A notifier's two buttons share one line. */
export function forgetOutcome(room, entity, aspect) {
  delete recent[logic.pendingKey(room, entity, aspect)];
}

/* A stepper's tap. It is held as a draft for a moment, so the taps that
 * follow build on it (logic.commandBase), and one command is sent for
 * where they end. Three taps of + are one request for +1.5, not three
 * requests each computed from a readback that has not moved yet. */
export function stepCmd(room, entity, aspect, value) {
  var key = logic.pendingKey(room, entity, aspect);
  delete recent[key];
  logic.trackCommand(pending, trackedCommand(room, entity, aspect, value), Date.now(), 'draft');
  clearTimeout(drafts[key]);
  drafts[key] = setTimeout(function () {
    delete drafts[key];
    sendCmd(room, entity, aspect, value);
  }, STEP_SETTLE_MS);
  scheduleRender();
}

// What logic.trackCommand records a command from. `seq` is the POST's, once there is one.
function trackedCommand(room, entity, aspect, value, seq) {
  var spec = findEntity(room, entity);
  return {
    room: room, entity: entity, aspect: aspect, value: value, seq: seq, tolerance: logic.cmdTolerance(aspect, store.model.vocabulary),
    capability: spec ? spec.capability : null, descriptor: store.aspects[entity],
    before: stateValue(room, entity, aspect)
  };
}

// Where a stepper's next step starts: the request in flight, if any.
export function stepBase(room, entity, aspect) {
  return logic.commandBase(pending, room, entity, aspect, stateValue(room, entity, aspect));
}

// A function that words a value as the control for this aspect does.
function cmdFormatter(room, entity, aspect) {
  var spec = findEntity(room, entity);
  var field = spec ? descriptorField(spec, aspect) : null;
  return function (v) { return logic.formatCmdValue(aspect, v, field, store.model.vocabulary); };
}

/* The line under a control: the stage its command is in, or how it
 * ended. `tone` is the colour: busy while waiting, ok when the device took
 * it, warn when it settled elsewhere or was held, and bad when it went
 * nowhere. A refusal is not worded as a failure. The command was
 * well-formed and lost to a higher band, and a retry would lose the same
 * way. */
function commandStatus(key) {
  var parts = key.split('/');
  var room = parts[0], entity = parts[1], aspect = parts[2];
  var fmt = cmdFormatter(room, entity, aspect);
  var entry = pending[key];
  if (entry) {
    var text = 'Asked ' + fmt(entry.value);
    if (entry.outcome === 'draft') return { tone: 'busy', text: text };
    if (entry.seen !== undefined) text += ' · still ' + fmt(entry.seen);
    else text += entry.readback === aspect ? ' · waiting for the device' : ' · waiting for delivery';
    var spec = findEntity(room, entity);
    var warning = spec
      ? logic.commandWarning(store.health[logic.healthKey(spec.owner)], unitLabel(spec.owner), stateValue(room, entity, 'available'))
      : '';
    return warning ? { tone: 'warn', text: text + ' · ' + warning } : { tone: 'busy', text: text };
  }
  var done = logic.recentFor(recent, key, Date.now());
  if (!done) return null;
  return outcomeStatus(done, fmt, room, entity, aspect);
}

// logic.outcomeStatus, given what it reads from the store.
function outcomeStatus(done, fmt, room, entity, aspect) {
  var hold = done.outcome === 'held' ? logic.holdOn(store.holds, room, entity, aspect, Date.now(), store.arbitration) : null;
  var spec = findEntity(room, entity);
  return logic.outcomeStatus(done, fmt, hold, spec ? unitLabel(spec.owner) : null);
}

/* An ended command is shown on its control. The toast is the fallback
 * when that control is no longer on screen (the overlay closed, or the
 * view changed). A confirmation needs no toast. */
export function announceOutcome(outcome) {
  logic.noteOutcome(recent, outcome, Date.now());
  if (outcome.outcome === 'confirmed') return;
  var shown = document.querySelector('[data-cmd-status="' + cssEscape(outcome.key) + '"]');
  if (shown) return;
  var parts = outcome.key.split('/');
  var status = outcomeStatus(outcome, cmdFormatter(parts[0], parts[1], parts[2]), parts[0], parts[1], parts[2]);
  var spec = findEntity(parts[0], parts[1]);
  if (status) toast((spec ? spec.label + ': ' : '') + status.text);
}

function cssEscape(text) {
  return window.CSS && CSS.escape ? CSS.escape(text) : String(text).replace(/["\\]/g, '\\$&');
}

/* POST `body` as JSON with the family's header. Resolves to the response.
 * An error status rejects with the reply's text, or 'HTTP <status>'
 * when the reply is empty. */
function postFamily(url, body) {
  return fetch(url, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'X-Homeostat': 'family' },
    body: JSON.stringify(body)
  }).then(function (r) {
    if (!r.ok) return r.text().then(function (t) { throw new Error(t || ('HTTP ' + r.status)); });
    return r;
  });
}

/* Change a family hold (docs/design.md#hold-intents): "release" hands
 * the aspect back, and an instant holds `value` until then. Neither is
 * tracked as a command. A release reaches no device, and a new end sends
 * the value the device already holds, so no readback answers it. The
 * hold's record changing is the answer, and the page redraws when it
 * does. Resolves to whether the dashboard took it. */
export function sendHold(room, entity, aspect, value, hold) {
  var body = hold === logic.HOLD_RELEASE
    ? { room: room, entity: entity, aspect: aspect, hold: hold }
    : { room: room, entity: entity, aspect: aspect, value: value, hold: hold };
  return postFamily('/api/cmd', body).then(function () {
    return true;
  }).catch(function (e) {
    toast(logic.errText(e));
    return false;
  });
}

export function sendLightsOff() {
  postFamily('/api/lights/off').catch(function (e) {
    toast(logic.errText(e));
  });
}

export function sendParam(unit, param, value, onError) {
  postFamily('/api/param', { unit: unit, param: param, value: value }).catch(function (e) {
    toast(logic.errText(e));
    if (onError) onError();
  });
}

/* The aspect a control commands. Most carry it explicitly. These actions
 * command their capability's base aspect, and the brightness stepper its
 * aspect. The light sliders name theirs `data-kind`. */
var CONTROL_CAPABILITIES = {
  'toggle-light': 'light',
  'toggle-lock': 'lock',
  'climate-step': 'climate'
};

function controlAspect(el) {
  var action = el.getAttribute('data-action');
  if (CONTROL_CAPABILITIES[action]) return baseAspect(CONTROL_CAPABILITIES[action]);
  if (action === 'brightness-step') return 'brightness';
  if (action === 'slider') return el.getAttribute('data-kind');
  return el.getAttribute('data-aspect');
}

// The actions that send a command. Other elements with a room and entity
// (a row that opens a detail, a label that opens history) are not
// controls.
var COMMAND_ACTIONS = {
  'toggle-light': 1, 'toggle-lock': 1, 'climate-step': 1, 'brightness-step': 1,
  'aspect-step': 1, 'aspect-enum': 1, 'aspect-select': 1, 'aspect-slider': 1, 'slider': 1,
  'notify-send': 1
};

/* Applied after every render rather than built into each control's
 * markup. The views rebuild their HTML completely, so the pending state
 * has to be applied to the new nodes anyway. Doing it in one pass keeps it
 * out of a dozen render paths.
 *
 * A control with a command in flight shows the request as a request. The
 * asked-for value appears in the value slot, or the toggle's knob moves
 * where it was asked to go, with a dashed outline. A line under the
 * control says which stage the command is in. The control still accepts
 * taps: a toggle tapped again asks to go back, and a stepper tapped again
 * steps on from the request. Blocking it until the device answers would
 * make several steps in a row impossible. The line stays for a while
 * after the command ends and says how it ended. */
export function markPendingControls() {
  var old = document.querySelectorAll('.cmd-status');
  for (var o = 0; o < old.length; o++) old[o].remove();
  var nodes = document.querySelectorAll('[data-action][data-room][data-entity]');
  var said = [];
  for (var i = 0; i < nodes.length; i++) {
    var el = nodes[i];
    var action = el.getAttribute('data-action');
    if (!COMMAND_ACTIONS[action]) continue;
    var aspect = controlAspect(el);
    if (!aspect) continue;
    var room = el.getAttribute('data-room');
    var entity = el.getAttribute('data-entity');
    var key = logic.pendingKey(room, entity, aspect);
    var entry = pending[key];
    el.classList.remove('cmd-pending');
    if (entry) el.setAttribute('aria-busy', 'true');
    else el.removeAttribute('aria-busy');
    var row = el.closest('.dial, .slider-row, .aspect-row, .entity-row') || el.parentNode;
    if (entry) showRequest(el, row, room, entity, aspect, entry.value);
    if (said.indexOf(row) !== -1) continue;
    var status = commandStatus(key);
    if (!status) continue;
    said.push(row);
    var line = document.createElement('div');
    line.className = 'cmd-status tone-' + status.tone;
    line.setAttribute('data-cmd-status', key);
    line.setAttribute('role', 'status');
    line.textContent = status.text;
    row.insertAdjacentElement('afterend', line);
  }
}

// The asked-for value, drawn as a request rather than as done.
function showRequest(el, row, room, entity, aspect, value) {
  if (el.classList.contains('toggle')) {
    el.classList.toggle('on', value === true);
    el.setAttribute('data-value', String(value !== true)); // a second tap asks to go back
    el.classList.add('cmd-pending');
  } else if (el.getAttribute('data-action') === 'aspect-enum') {
    var mine = el.getAttribute('data-value') === JSON.stringify(value);
    el.classList.toggle('active', mine);
    if (mine) el.classList.add('cmd-pending');
  } else if (el.tagName === 'SELECT') {
    el.value = JSON.stringify(value);
    el.classList.add('cmd-pending');
  } else if (el.tagName !== 'INPUT') {
    var slot = row.querySelector('.stepper-value, .dial-target, .sval');
    if (slot) {
      slot.textContent = cmdFormatter(room, entity, aspect)(value);
      slot.classList.add('cmd-target');
    }
  }
}
