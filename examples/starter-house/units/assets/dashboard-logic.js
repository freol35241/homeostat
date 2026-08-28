/* Dashboard decision logic: the pure functions behind the Now view and the
 * WebSocket store — extracted from dashboard.html so `node --test
 * tests/js` can pin them (the DOM wiring stays in the page). Functions in,
 * functions out: no DOM, no fetch, no globals.
 *
 * Loaded two ways: as a plain script by dashboard.html (defines
 * window.HomeostatLogic) and via require() by the node test runner.
 */
'use strict';
(function (root, factory) {
  if (typeof module === 'object' && module.exports) module.exports = factory();
  else root.HomeostatLogic = factory();
})(typeof self !== 'undefined' ? self : this, function () {

  function entityKey(room, entity, aspect) {
    return 'home/state/' + room + '/' + entity + '/' + aspect;
  }

  function titleCase(s) {
    if (!s) return '';
    return String(s).split(/[\s_-]+/).map(function (w) {
      return w ? w[0].toUpperCase() + w.slice(1) : w;
    }).join(' ');
  }

  function stateValue(state, room, entity, aspect) {
    return state[entityKey(room, entity, aspect)];
  }

  // Adapters differ on the presence aspect name (z2m passes through
  // "occupancy", other worlds say "presence") — no schema authority
  // exists yet, so accept both. See docs/design.md: typed vocabulary is
  // a known gap.
  var PRESENCE_ASPECTS = ['occupancy', 'presence'];

  function presenceValue(state, entity) {
    for (var i = 0; i < PRESENCE_ASPECTS.length; i++) {
      var v = stateValue(state, entity.room, entity.name, PRESENCE_ASPECTS[i]);
      if (v !== undefined) return v;
    }
    return undefined;
  }

  // "room/entity" when the key is a presence-aspect state key, else null.
  function presenceEntityFromKey(key) {
    var parts = key.split('/');
    if (parts[0] === 'home' && parts[1] === 'state' && parts.length >= 5 &&
        PRESENCE_ASPECTS.indexOf(parts[4]) !== -1) {
      return parts[2] + '/' + parts[3];
    }
    return null;
  }

  function unitNameFromHealthKey(key) {
    // home/health/{unit}
    var parts = key.split('/');
    return parts[2] || key;
  }

  /* Applies one WebSocket message to the store (state/health/config maps,
   * the capped events feed). Returns the message type when applied, null
   * for anything unrecognized — the caller renders (and tracks per-key
   * side effects) only on a true apply. `nowSeconds` stamps events whose
   * message carries no ts. */
  var EVENTS_CAP = 200;

  function applyMessage(store, msg, nowSeconds) {
    if (!msg || !msg.type) return null;
    if (msg.type === 'snapshot') {
      store.state = msg.state || {};
      store.health = msg.health || {};
      store.config = msg.config || {};
      return 'snapshot';
    }
    if (msg.type === 'state') {
      store.state[msg.key] = msg.value;
      return 'state';
    }
    if (msg.type === 'config') {
      store.config[msg.key] = msg.value;
      return 'config';
    }
    if (msg.type === 'health') {
      store.health[msg.key] = msg.value;
      return 'health';
    }
    if (msg.type === 'event') {
      store.events.unshift({ key: msg.key, value: msg.value, ts: msg.ts || nowSeconds });
      if (store.events.length > EVENTS_CAP) store.events.length = EVENTS_CAP;
      return 'event';
    }
    return null;
  }

  /* The Now view's "out of the ordinary" list, in render order. Each
   * record: { tag, title, detail, target, button? } where target names
   * what a tap opens — {type:'unit', unit}, {type:'rooms'},
   * {type:'entity', room, entity}, or {type:'setpoint', unit, param} —
   * and button is the optional corrective action. */
  function computeDeviations(model, state, health, config) {
    var entities = model.entities || [];
    var units = model.units || [];
    var labels = {};
    units.forEach(function (u) { labels[u.name] = u.label || u.name; });
    var deviations = [];

    // 1. supervision: any unit not running
    Object.keys(health).forEach(function (k) {
      var h = health[k] || {};
      var status = h.status || 'running';
      if (status === 'running') return;
      var unit = unitNameFromHealthKey(k);
      var detail = [];
      if (h.restarts) detail.push('restarts: ' + h.restarts);
      if (h.backoff_ms) detail.push('backoff ' + h.backoff_ms + 'ms');
      deviations.push({
        tag: 'supervision',
        title: labels[unit] || unit,
        detail: status + (detail.length ? ' — ' + detail.join(', ') : ''),
        target: { type: 'unit', unit: unit }
      });
    });

    // 2. state: lights on (one aggregate row with the corrective action —
    // a manual-band fan-out server-side; the family always wins)
    var litRooms = {};
    var litCount = 0;
    entities.filter(function (e) { return e.capability === 'light'; }).forEach(function (e) {
      if (stateValue(state, e.room, e.name, 'on') === true) {
        litCount++;
        litRooms[e.room] = true;
      }
    });
    if (litCount > 0) {
      deviations.push({
        tag: 'state',
        title: litCount + ' light' + (litCount === 1 ? '' : 's') + ' on',
        detail: Object.keys(litRooms).map(titleCase).join(', '),
        target: { type: 'rooms' },
        button: { action: 'lights-off', label: 'All off' }
      });
    }

    var entityRow = function (e, title) {
      return {
        tag: 'state', title: title, detail: '',
        target: { type: 'entity', room: e.room, entity: e.name }
      };
    };

    // state: unlocked locks
    entities.filter(function (e) { return e.capability === 'lock'; }).forEach(function (e) {
      if (stateValue(state, e.room, e.name, 'locked') === false) {
        deviations.push(entityRow(e, e.label + ' unlocked'));
      }
    });

    // state: connectivity — WAN or a VPN tunnel down
    entities.filter(function (e) { return e.capability === 'router'; }).forEach(function (e) {
      if (stateValue(state, e.room, e.name, 'wan') === false) {
        deviations.push(entityRow(e, e.label + ' — WAN down'));
      }
    });
    entities.filter(function (e) { return e.capability === 'vpn'; }).forEach(function (e) {
      if (stateValue(state, e.room, e.name, 'up') === false) {
        deviations.push(entityRow(e, e.label + ' down'));
      }
    });

    // state: a device gone quiet — the owning adapter published
    // available = false (any capability; the aspect is orthogonal)
    entities.forEach(function (e) {
      if (stateValue(state, e.room, e.name, 'available') === false) {
        deviations.push(entityRow(e, e.label + ' unresponsive'));
      }
    });

    // 3. setpoint: live config differing from the manifest default
    units.forEach(function (u) {
      var params = u.params || {};
      Object.keys(params).forEach(function (pname) {
        var configKey = 'home/config/' + u.name + '/' + pname;
        if (!(configKey in config)) return;
        var live = config[configKey];
        var def = params[pname].default;
        if (JSON.stringify(live) !== JSON.stringify(def)) {
          deviations.push({
            tag: 'setpoint',
            title: (labels[u.name] || u.name) + ' · ' + pname,
            detail: String(live) + ' (default ' + String(def) + ')',
            target: { type: 'setpoint', unit: u.name, param: pname }
          });
        }
      });
    });

    // 4. arbiter preemptions — not yet a data source; leave as future
    // work. TODO: once the arbiter publishes preemption events, surface
    // "{entity} — manual override active" here.

    return deviations;
  }

  return {
    PRESENCE_ASPECTS: PRESENCE_ASPECTS,
    titleCase: titleCase,
    entityKey: entityKey,
    stateValue: stateValue,
    presenceValue: presenceValue,
    presenceEntityFromKey: presenceEntityFromKey,
    unitNameFromHealthKey: unitNameFromHealthKey,
    applyMessage: applyMessage,
    computeDeviations: computeDeviations
  };
});
