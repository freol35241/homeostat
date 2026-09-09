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
      store.aspects = msg.aspects || {};
      return 'snapshot';
    }
    if (msg.type === 'aspects') {
      store.aspects[msg.entity] = msg.value;
      return 'aspects';
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
   * {type:'entity', room, entity}, {type:'setpoint', unit, param} (a
   * family-editable param) or {type:'unit', unit} (an owner param) —
   * and button is the optional corrective action. */
  function computeDeviations(model, state, health, config, aspects) {
    aspects = aspects || {};
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

    // state: an aspect its adapter's descriptor marks notable, when true
    // (an alarm flag, say) — vocabulary the adapter declares, never house
    // configuration
    entities.forEach(function (e) {
      var fields = (aspects[e.name] && aspects[e.name].fields) || {};
      Object.keys(fields).forEach(function (aspect) {
        if (fields[aspect].notable && stateValue(state, e.room, e.name, aspect) === true) {
          deviations.push(entityRow(e, e.label + ' — ' + (fields[aspect].label || aspect)));
        }
      });
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
          // A family param taps through to its editor on Setpoints; an
          // owner param is read-only here, so it taps to the unit overlay
          // where it is shown against its default.
          var family = params[pname].editable_by === 'family';
          deviations.push({
            tag: 'setpoint',
            title: (labels[u.name] || u.name) + ' · ' + pname,
            detail: String(live) + ' (default ' + String(def) + ')',
            target: family
              ? { type: 'setpoint', unit: u.name, param: pname }
              : { type: 'unit', unit: u.name }
          });
        }
      });
    });

    // 4. arbiter preemptions — not yet a data source; leave as future
    // work. TODO: once the arbiter publishes preemption events, surface
    // "{entity} — manual override active" here.

    return deviations;
  }


  /* ---- aspect descriptors (docs/design.md, Aspect descriptors) ----
   * An adapter may describe an entity's aspects in its discovery record:
   * { schema, groups: [name...], fields: { aspect: { label, kind, group,
   * unit?, values?, valid?, notable?, command? } } }. The page renders the
   * description through the widgets it already has; this is the pure
   * mapping from descriptor + state to a render plan. */
  var DIAGNOSTICS = 'diagnostics';

  // Display text for one value: the field's kind decides, falling back to
  // the undescribed rule (one decimal; a degree sign when the aspect name
  // says temperature).
  function formatAspect(aspect, field, value) {
    if (value === undefined || value === null) return '—';
    var kind = field && field.kind;
    if (field && field.values) {
      // an enum's labels; a boolean may carry them too ("locked"/"unlocked")
      for (var i = 0; i < field.values.length; i++) {
        if (field.values[i].value === value) return field.values[i].label;
      }
      if (kind === 'enum') return String(value);
    }
    if (typeof value === 'number') {
      if (kind === 'temperature') return value.toFixed(1) + '°';
      if (kind === 'temperature_delta') return (value > 0 ? '+' : '') + value.toFixed(1) + '°';
      if (kind === 'percent') return Math.round(value) + '%';
      if (kind === 'number') return String(Math.round(value * 100) / 100) + (field.unit ? ' ' + field.unit : '');
      if (aspect.indexOf('temperature') !== -1) return value.toFixed(1) + '°';
      return value.toFixed(1);
    }
    if (typeof value === 'boolean') {
      if (kind === 'boolean') return value ? 'on' : 'off';
      return value ? 'true' : 'false';
    }
    return String(value);
  }

  // The control a described command renders as — the param-control
  // shapes: an enum is a segmented control, a float with a step is a
  // stepper, any other number a slider. A command the family may not
  // edit reads its value with a tier badge instead. `commandable` is the
  // dashboard's own grant on the capability: without it the control is
  // inert, as for every other widget.
  function controlFor(field, commandable) {
    var cmd = field && field.command;
    if (!cmd) return null;
    var tier = cmd.editable_by || 'owner';
    if (tier !== 'family') return { kind: 'readonly', tier: tier };
    var c = cmd.constraint || {};
    if (cmd.type === 'enum') {
      return { kind: 'segment', values: field.values || [], disabled: !commandable };
    }
    if (cmd.type === 'float' || cmd.type === 'int') {
      if (cmd.step) {
        return { kind: 'stepper', step: cmd.step, min: c.min, max: c.max, disabled: !commandable };
      }
      return {
        kind: 'slider', min: c.min !== undefined ? c.min : 0, max: c.max !== undefined ? c.max : 100,
        step: cmd.type === 'int' ? 1 : 'any', disabled: !commandable
      };
    }
    return null;
  }

  /* Sections of rows for an entity's detail, in render order: the
   * descriptor's groups as listed, then diagnostics for every present
   * aspect it does not describe (an undescribed entity is one 'state'
   * section — exactly today's flat list). A described field's `valid`
   * pointer names the boolean aspect that marks the value stale; that
   * aspect is consumed into the row's `stale` flag rather than listed.
   * Two aspects are schema vocabulary and need no descriptor: `available`
   * (device liveness, docs/design.md, Availability) renders as a boolean
   * in the descriptor's `status` group when it has one, and any
   * `{aspect}_valid` beside an undescribed `{aspect}` is consumed the
   * same way a declared pointer is.
   * Rows: { aspect, label, value, display, stale, numeric, control }. */
  function aspectPlan(entity, state, descriptor, commandable) {
    var prefix = 'home/state/' + entity.room + '/' + entity.name + '/';
    var present = {};
    Object.keys(state).forEach(function (k) {
      if (k.indexOf(prefix) === 0) present[k.slice(prefix.length)] = state[k];
    });
    var fields = {};
    Object.keys((descriptor && descriptor.fields) || {}).forEach(function (a) { fields[a] = descriptor.fields[a]; });
    var described = Object.keys(fields).length > 0;
    var groups = (descriptor && descriptor.groups) || [];
    if (!fields.available && 'available' in present) {
      fields.available = { label: 'available', kind: 'boolean', group: groups.indexOf('status') !== -1 ? 'status' : null };
    }
    Object.keys(present).forEach(function (a) {
      var base = a.replace(/_valid$/, '');
      if (base !== a && base in present && !fields[a] && !fields[base]) {
        fields[base] = { label: base, valid: a };
      }
    });
    var validOf = {};
    Object.keys(fields).forEach(function (a) {
      if (fields[a].valid) validOf[fields[a].valid] = a;
    });
    var order = groups.slice();
    var rest = described ? DIAGNOSTICS : 'state';
    if (order.indexOf(rest) === -1) order.push(rest);
    var byGroup = {};
    order.forEach(function (g) { byGroup[g] = []; });

    var row = function (aspect, field) {
      var value = present[aspect];
      var stale = !!(field && field.valid && present[field.valid] === false);
      return {
        aspect: aspect,
        label: (field && field.label) || aspect,
        value: value,
        display: formatAspect(aspect, field, value),
        stale: stale,
        numeric: typeof value === 'number',
        control: controlFor(field, commandable)
      };
    };

    Object.keys(fields).forEach(function (aspect) {
      if (!(aspect in present)) return;
      var group = fields[aspect].group;
      if (!group || !byGroup[group]) group = rest;
      byGroup[group].push(row(aspect, fields[aspect]));
    });
    Object.keys(present).sort().forEach(function (aspect) {
      if (fields[aspect] || validOf[aspect]) return;
      byGroup[rest].push(row(aspect, null));
    });

    return order.filter(function (g) { return byGroup[g].length > 0; }).map(function (g) {
      return { group: g, label: titleCase(g), rows: byGroup[g], collapsed: g === DIAGNOSTICS };
    });
  }

  /* The room-card row for a described entity (#32): at most two headline
   * readings and the family-editable controls. Headline is a convention,
   * not vocabulary — the first two control-less rows of the first group
   * that has any, so the adapter's own ordering decides — revisited if an
   * adapter ever needs to say otherwise. Card labels drop a trailing
   * "(CODE)" the overlay keeps: "feed line (GT1)" reads as "feed line". */
  function cardPlan(entity, state, descriptor, commandable) {
    var sections = aspectPlan(entity, state, descriptor, commandable);
    var controls = [];
    var readings = [];
    sections.forEach(function (s) {
      if (s.group === DIAGNOSTICS) return;
      s.rows.forEach(function (r) {
        if (r.control && r.control.kind !== 'readonly') controls.push(r);
      });
      if (readings.length === 0) {
        readings = s.rows.filter(function (r) { return !r.control; }).slice(0, 2);
      }
    });
    readings = readings.map(function (r) {
      var short = {};
      Object.keys(r).forEach(function (k) { short[k] = r[k]; });
      short.label = r.label.replace(/\s*\([^)]*\)$/, '');
      return short;
    });
    return { readings: readings, controls: controls };
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
    computeDeviations: computeDeviations,
    formatAspect: formatAspect,
    controlFor: controlFor,
    aspectPlan: aspectPlan,
    cardPlan: cardPlan
  };
});
