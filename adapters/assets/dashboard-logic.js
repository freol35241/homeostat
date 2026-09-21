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

  /* A person on Now: { home, seenAt }. home is the person's `presence`
   * aspect (true, false, or undefined when nothing publishes it); seenAt is
   * the last fix as epoch ms, when a location adapter reports one. The
   * page words it; this only reads the vocabulary. */
  function personStatus(state, entity) {
    var presence = stateValue(state, entity.room, entity.name, 'presence');
    var fixedAt = stateValue(state, entity.room, entity.name, 'fixed_at');
    return {
      home: typeof presence === 'boolean' ? presence : undefined,
      seenAt: typeof fixedAt === 'number' ? fixedAt * 1000 : undefined
    };
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
      store.forecasts = msg.forecasts || {};
      store.health = msg.health || {};
      store.config = msg.config || {};
      store.aspects = msg.aspects || {};
      return 'snapshot';
    }
    if (msg.type === 'aspects') {
      // null retires a descriptor the adapter no longer publishes
      if (msg.value === null || msg.value === undefined) delete store.aspects[msg.entity];
      else store.aspects[msg.entity] = msg.value;
      return 'aspects';
    }
    if (msg.type === 'state') {
      store.state[msg.key] = msg.value;
      return 'state';
    }
    if (msg.type === 'forecast') {
      store.forecasts[msg.key] = msg.value;
      return 'forecast';
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

  // An enum with more choices than fit on one segmented row.
  var SELECT_ABOVE = 4;

  // The control a described command renders as — the param-control
  // shapes: an enum is a segmented control (a select past SELECT_ABOVE
  // values), a temperature with a step is a dial (the page draws its
  // compact form, a stepper, where a card has no room), any other float
  // with a step a stepper, any other number a slider carrying a coarse
  // step for its ± buttons. A command the family may not edit reads its
  // value with a tier badge instead. `commandable` is the dashboard's own
  // grant on the capability: without it the control is inert, as for
  // every other widget.
  function controlFor(field, commandable) {
    var cmd = field && field.command;
    if (!cmd) return null;
    var tier = cmd.editable_by || 'owner';
    if (tier !== 'family') return { kind: 'readonly', tier: tier };
    var c = cmd.constraint || {};
    // step/min/max go into attributes and arithmetic: a non-number there
    // is a malformed descriptor, not a control (the server refuses the
    // command too).
    var bounds = [cmd.step, c.min, c.max];
    for (var i = 0; i < bounds.length; i++) {
      if (bounds[i] !== undefined && (typeof bounds[i] !== 'number' || !isFinite(bounds[i]))) return null;
    }
    if (cmd.type === 'enum') {
      var values = field.values || [];
      return { kind: values.length > SELECT_ABOVE ? 'select' : 'segment', values: values, disabled: !commandable };
    }
    if (cmd.type === 'float' || cmd.type === 'int') {
      if (cmd.step) {
        // a dial is an arc from min to max: without both bounds there is
        // no arc to draw, and the stepper is the honest control
        var bounded = typeof c.min === 'number' && typeof c.max === 'number' && c.max > c.min;
        var kind = field.kind === 'temperature' && bounded ? 'dial' : 'stepper';
        return { kind: kind, step: cmd.step, min: c.min, max: c.max, disabled: !commandable };
      }
      var min = c.min !== undefined ? c.min : 0, max = c.max !== undefined ? c.max : 100;
      return {
        kind: 'slider', min: min, max: max,
        step: cmd.type === 'int' ? 1 : 'any', coarse: coarseStep(min, max, cmd.type === 'int' ? 1 : 0),
        disabled: !commandable
      };
    }
    return null;
  }

  /* A slider's ± step: a twentieth of the range, never finer than the
   * value's own step, rounded to something a person would say (5 on a
   * percent, 1 on a small integer range). */
  function coarseStep(min, max, atLeast) {
    var raw = (max - min) / 20;
    var nice = raw >= 5 ? 5 * Math.round(raw / 5) : raw >= 1 ? Math.round(raw) : Math.round(raw * 10) / 10;
    return Math.max(nice, atLeast, 0.1);
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

  /* The sparkline rows of a sensor's room card (#56): every numeric,
   * control-less row outside diagnostics, in the descriptor's order — so
   * a thermometer's card lists temperature and humidity and not its link
   * quality, and the sensor widget keeps its sparklines instead of being
   * routed to the described card. An undescribed sensor is its flat
   * state list, sorted. Same rows as the overlay, minus the collapsed
   * group. */
  function sensorCardPlan(entity, state, descriptor) {
    var rows = [];
    aspectPlan(entity, state, descriptor, false).forEach(function (s) {
      if (s.group === DIAGNOSTICS) return;
      s.rows.forEach(function (r) {
        if (r.numeric && !r.control) rows.push(r);
      });
    });
    return rows;
  }

  /* ---- pending commands (issue #94) ----
   *
   * A command is a proposal, not a write. It passes through arbitration,
   * the adapter's validation and finally the device's own readback, and
   * each of those can end it. The page used to show nothing at all
   * between the tap and stage 4, so a slow device looked like a dead
   * button and the natural response was to tap again.
   *
   * Optimistic painting is not the fix: posting an out-of-range value
   * returns ok and is then dropped at stage 3, so the control would show
   * a value the house never took. Instead the stages are made visible,
   * and the envelope's correlation id is what ties an event back to the
   * command it ended. */

  /* How long to wait for a readback before calling it unconfirmed. There
   * is no readback cadence on the wire, and the capability is the only
   * thing the browser knows about a device's class, so it is what the
   * wait is scaled to: a z2m lamp answers in about a second, a lock waits
   * on a motor, a burner behind a polling bridge can take half a minute
   * (issue #94). Generous on purpose — a timeout that fires early reports
   * a failure that did not happen. */
  var COMMAND_TIMEOUT_MS = {
    light: 8000,
    switch: 8000,
    lock: 15000,
    climate: 15000,
    burner: 60000
  };
  var DEFAULT_COMMAND_TIMEOUT_MS = 20000;

  function commandTimeoutMs(capability) {
    return COMMAND_TIMEOUT_MS[capability] || DEFAULT_COMMAND_TIMEOUT_MS;
  }

  function pendingKey(room, entity, aspect) {
    return room + '/' + entity + '/' + aspect;
  }

  /* One in-flight command per (room, entity, aspect): a second tap on the
   * same control replaces the first, which is what the user means by it. */
  function trackCommand(pending, cmd, nowMs) {
    pending[pendingKey(cmd.room, cmd.entity, cmd.aspect)] = {
      id: cmd.id,
      value: cmd.value,
      at: nowMs,
      timeoutMs: commandTimeoutMs(cmd.capability),
      outcome: 'pending'
    };
    return pending;
  }

  function pendingFor(pending, room, entity, aspect) {
    return pending[pendingKey(room, entity, aspect)] || null;
  }

  /* Stage 4. Any state update for the commanded aspect resolves it —
   * including one whose value differs from what was asked, because a
   * device that clamped the value has still answered. */
  function resolveFromState(pending, key) {
    var parts = String(key).split('/');
    if (parts[0] !== 'home' || parts[1] !== 'state' || parts.length < 5) return null;
    var pk = pendingKey(parts[2], parts[3], parts[4]);
    var entry = pending[pk];
    if (!entry || entry.outcome !== 'pending') return null;
    delete pending[pk];
    return { key: pk, outcome: 'confirmed' };
  }

  /* Stages 2 and 3, both carried by health events and both addressed by
   * cmd_id. `refuse` is deliberately not a failure: the command was
   * well-formed and simply lost to a higher band, and the user's next
   * move differs completely from a retry. An event without a cmd_id
   * belongs to no command we are tracking. */
  function resolveFromEvent(pending, event) {
    if (!event || !event.cmd_id) return null;
    var keys = Object.keys(pending);
    for (var i = 0; i < keys.length; i++) {
      var entry = pending[keys[i]];
      if (!entry || entry.id !== event.cmd_id) continue;
      delete pending[keys[i]];
      if (event.kind === 'refuse') {
        return {
          key: keys[i],
          outcome: 'held',
          by: event.holder_priority || 'another band',
          actor: event.holder_actor || null
        };
      }
      return { key: keys[i], outcome: 'rejected', reason: event.reason || 'dropped' };
    }
    return null;
  }

  /* Nothing answered. Not the same as success, and today the page cannot
   * tell the two apart at all. */
  function expirePending(pending, nowMs) {
    var out = [];
    Object.keys(pending).forEach(function (k) {
      var entry = pending[k];
      if (entry && entry.outcome === 'pending' && nowMs - entry.at > entry.timeoutMs) {
        delete pending[k];
        out.push({ key: k, outcome: 'unconfirmed' });
      }
    });
    return out;
  }

  /* ---- forecasts (docs/design.md, Forecasts) ----
   *
   * What the house believes about a series' future, carried live beside
   * its present. The wire shape is {schema, issued, points:[{t, v, d?}]};
   * `d` is a point's extent in seconds, absent for an instant. */

  // The decoded forecast for one aspect, or null. Points become
  // millisecond timestamps here so the chart can place them on the same
  // axis as recorded history without every caller re-parsing.
  function forecastFor(forecasts, room, entity, aspect) {
    var doc = forecasts && forecasts['home/forecast/' + room + '/' + entity + '/' + aspect];
    if (!doc || !doc.points || !doc.points.length) return null;
    var issued = Date.parse(doc.issued);
    var points = [];
    for (var i = 0; i < doc.points.length; i++) {
      var p = doc.points[i];
      var t = Date.parse(p.t);
      if (isNaN(t) || typeof p.v !== 'number') return null;  // malformed: show nothing
      points.push({ t: t, v: p.v, d: typeof p.d === 'number' ? p.d : null });
    }
    // The horizon runs to the end of a final interval, not its start —
    // otherwise a coarse trailing window is drawn as a dot.
    var last = points[points.length - 1];
    return {
      issued: isNaN(issued) ? null : issued,
      points: points,
      from: points[0].t,
      to: last.d ? last.t + last.d * 1000 : last.t
    };
  }

  // What a tile says about a horizon: where it is going, not just where
  // it is. An extreme is worth a glance only if the series actually
  // moves, so a flat horizon reports nothing rather than "min = max".
  function horizonSummary(forecast) {
    if (!forecast || forecast.points.length < 2) return null;
    var lo = forecast.points[0], hi = forecast.points[0];
    for (var i = 1; i < forecast.points.length; i++) {
      if (forecast.points[i].v < lo.v) lo = forecast.points[i];
      if (forecast.points[i].v > hi.v) hi = forecast.points[i];
    }
    if (lo.v === hi.v) return null;
    return { min: lo, max: hi };
  }

  /* ---- history shapes ----
   *
   * The recorder folds a window two ways (docs/design.md, Read path):
   * `bucket` for a line, one point per bucket so a chatty series fills a
   * week instead of showing its last hour, and `changes` for a timeline,
   * the runs of a state. The descriptor decides when there is one: an
   * enum or a boolean has runs, not a curve, whatever JS type its values
   * happen to be — an enum coded as integers (ivt490's operating_mode)
   * would otherwise be drawn as a line between its codes and averaged.
   * Undescribed, the value's type is all there is to go on. */

  function historyShape(value, field) {
    var kind = field && field.kind;
    if (kind === 'enum' || kind === 'boolean') return 'timeline';
    return typeof value === 'number' ? 'chart' : 'timeline';
  }

  /* One bucket per drawn column: the chart's viewBox width is the most
   * points it can show apart. */
  function bucketSeconds(hours, width) {
    return Math.max(1, Math.round(hours * 3600 / width));
  }

  /* The runs of a state from change rows: each row opens a run that ends
   * where the next begins or at the window's end. Nothing is known before
   * the first row, so a run never starts before it — the timeline shows a
   * gap there rather than guessing. Times are epoch ms. */
  function timelineRuns(points, fromMs, toMs) {
    var runs = [];
    (points || []).forEach(function (p, i) {
      var start = Math.max(Date.parse(p.ts), fromMs);
      var next = points[i + 1];
      var end = next ? Math.max(Date.parse(next.ts), fromMs) : toMs;
      if (!(end > start)) return;
      runs.push({ value: p.value, start: start, end: end });
    });
    return runs;
  }

  /* What the stat row says under a timeline: how long the state was
   * `true` (for a boolean; a string's runs have no such sum), how many
   * changes the window holds, and what it is now. */
  function timelineStats(runs) {
    var onMs = 0, boolean = runs.length > 0;
    runs.forEach(function (r) {
      if (typeof r.value !== 'boolean') boolean = false;
      else if (r.value) onMs += r.end - r.start;
    });
    return {
      onMs: boolean ? onMs : null,
      changes: Math.max(0, runs.length - 1),
      latest: runs.length ? runs[runs.length - 1].value : null
    };
  }

  /* ---- views (docs/design.md, Dashboard: views are text) ----
   *
   * dashboard.toml's [[view]] list is the nav; without the file the
   * generated views stand in. Health and "Not shown" are chrome the page
   * always draws, never views here. Everything below is a pure function
   * of the model /api/model serves. */

  var GENERATED_LABELS = { now: 'Now', setpoints: 'Setpoints', rooms: 'Rooms' };
  var DEFAULT_VIEWS = ['now', 'setpoints', 'rooms'];

  function viewsOf(model) {
    var views = model && model.views;
    if (!views) {
      return DEFAULT_VIEWS.map(function (k) { return { name: k, label: GENERATED_LABELS[k], kind: k, widgets: [] }; });
    }
    return views.map(function (v) {
      return { name: v.name, label: v.label || titleCase(v.name), kind: v.kind || null, widgets: v.widgets || [] };
    });
  }

  function familyParams(unit) {
    var params = (unit && unit.params) || {};
    return Object.keys(params).filter(function (p) { return params[p].editable_by === 'family'; });
  }

  /* A view's widgets with every group opened out: a group is a card
   * around its members, never a placement or a destination of its own. */
  function flatWidgets(view) {
    var out = [];
    (view.widgets || []).forEach(function (w) {
      if (w.kind === 'group') (w.widgets || []).forEach(function (m) { out.push(m); });
      else out.push(w);
    });
    return out;
  }

  /* What no view places: entities and family params the family cannot
   * reach from the nav. An entity is placed by a widget naming it, its
   * room, a unit that publishes or drives it, a `people` widget when it is
   * a person, or any generated `rooms` (every entity) or `now` (people)
   * view; a param by a `params`/`unit` widget for its unit or a generated
   * `setpoints` view. Deviations and the map place nothing: they are
   * signals, not inventory. */
  function placement(model) {
    var views = viewsOf(model);
    var entities = (model.entities || []).slice();
    var units = model.units || [];
    var byName = {};
    units.forEach(function (u) { byName[u.name] = u; });
    var placedEntity = {}, placedParam = {};
    var allEntities = false, allParams = false, persons = false;
    function placeUnit(name) {
      var u = byName[name];
      if (!u) return;
      familyParams(u).forEach(function (p) { placedParam[name + '.' + p] = true; });
      (u.drives || []).forEach(function (f) { placedEntity[f.entity] = true; });
      entities.forEach(function (e) { if (e.owner === name) placedEntity[e.name] = true; });
    }
    views.forEach(function (v) {
      if (v.kind === 'rooms') allEntities = true;
      if (v.kind === 'setpoints') allParams = true;
      if (v.kind === 'now') persons = true;
      flatWidgets(v).forEach(function (w) {
        if (w.entity) placedEntity[w.entity] = true;
        if (w.kind === 'room') entities.forEach(function (e) { if (e.room === w.room) placedEntity[e.name] = true; });
        if (w.kind === 'unit') placeUnit(w.unit);
        if (w.kind === 'params') familyParams(byName[w.unit]).forEach(function (p) { placedParam[w.unit + '.' + p] = true; });
        if (w.kind === 'people') persons = true;
      });
    });
    var unplacedEntities = allEntities ? [] : entities.filter(function (e) {
      return !placedEntity[e.name] && !(persons && e.capability === 'person');
    });
    var unplacedParams = [];
    if (!allParams) {
      units.forEach(function (u) {
        familyParams(u).forEach(function (p) {
          if (!placedParam[u.name + '.' + p]) unplacedParams.push({ unit: u.name, param: p, spec: u.params[p] });
        });
      });
    }
    return { entities: unplacedEntities, params: unplacedParams };
  }

  /* The unit card's four relations, each read back from the manifest and
   * the grant table rather than declared for the card: family params,
   * the entities it owns, the fields its cmd grants reach, the fields its
   * state subscriptions read. Drives and From are fields — { entity,
   * aspect } as the model carries them (dashboard.py, unit_relations),
   * resolved here to { entity: <the entity>, aspect } — because an
   * automation that commands a lamp and subscribes to it named the same
   * entity in both sections and said nothing about which part of it.
   * Labels are the page's; nothing here is vocabulary. */
  function unitCardPlan(model, unitName) {
    var unit = (model.units || []).filter(function (u) { return u.name === unitName; })[0];
    if (!unit) return null;
    var byName = {};
    (model.entities || []).forEach(function (e) { byName[e.name] = e; });
    var fields = function (rels) {
      return (rels || []).map(function (f) {
        return byName[f.entity] ? { entity: byName[f.entity], aspect: f.aspect || null } : null;
      }).filter(Boolean);
    };
    return {
      unit: unit,
      params: familyParams(unit),
      publishes: (model.entities || []).filter(function (e) { return e.owner === unitName; }),
      drives: fields(unit.drives),
      sources: fields(unit.sources)
    };
  }

  /* Where a deviation's tap should land now that the nav is the file's:
   * a setpoint goes to the first view carrying its unit's params (or a
   * generated Setpoints), the lights-on deviation to a generated Rooms.
   * null means no view shows it — the page falls back to an overlay or to
   * Not shown. */
  function viewFor(target, views) {
    var found = null;
    views.forEach(function (v) {
      if (found) return;
      if (target.type === 'setpoint') {
        var hit = v.kind === 'setpoints' || flatWidgets(v).some(function (w) {
          return (w.kind === 'params' || w.kind === 'unit') && w.unit === target.unit;
        });
        if (hit) found = v.name;
      } else if (target.type === 'rooms' && v.kind === 'rooms') {
        found = v.name;
      }
    });
    return found;
  }

  return {
    PRESENCE_ASPECTS: PRESENCE_ASPECTS,
    viewsOf: viewsOf,
    placement: placement,
    unitCardPlan: unitCardPlan,
    viewFor: viewFor,
    historyShape: historyShape,
    bucketSeconds: bucketSeconds,
    timelineRuns: timelineRuns,
    timelineStats: timelineStats,
    titleCase: titleCase,
    entityKey: entityKey,
    stateValue: stateValue,
    presenceValue: presenceValue,
    personStatus: personStatus,
    presenceEntityFromKey: presenceEntityFromKey,
    unitNameFromHealthKey: unitNameFromHealthKey,
    applyMessage: applyMessage,
    computeDeviations: computeDeviations,
    forecastFor: forecastFor,
    horizonSummary: horizonSummary,
    formatAspect: formatAspect,
    controlFor: controlFor,
    coarseStep: coarseStep,
    SELECT_ABOVE: SELECT_ABOVE,
    aspectPlan: aspectPlan,
    cardPlan: cardPlan,
    sensorCardPlan: sensorCardPlan,
    COMMAND_TIMEOUT_MS: COMMAND_TIMEOUT_MS,
    DEFAULT_COMMAND_TIMEOUT_MS: DEFAULT_COMMAND_TIMEOUT_MS,
    commandTimeoutMs: commandTimeoutMs,
    pendingKey: pendingKey,
    trackCommand: trackCommand,
    pendingFor: pendingFor,
    resolveFromState: resolveFromState,
    resolveFromEvent: resolveFromEvent,
    expirePending: expirePending
  };
});
