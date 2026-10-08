/* Dashboard decision logic: the pure functions behind the Now view and the
 * WebSocket store. They are kept apart from the page so `node --test
 * tests/js` can test them. The DOM wiring is in the page's modules
 * (assets/dashboard/). Nothing here touches the DOM, fetches, or uses
 * globals.
 *
 * It is loaded two ways: as a plain script by dashboard.html, which
 * defines window.HomeostatLogic, and with require() by the node test
 * runner.
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

  // Adapters differ on the presence aspect name. z2m passes through
  // "occupancy", and others use "presence". The vocabulary allows either
  // (docs/design.md#the-capability-vocabulary), so accept both.
  var PRESENCE_ASPECTS = ['occupancy', 'presence'];

  function presenceValue(state, entity) {
    for (var i = 0; i < PRESENCE_ASPECTS.length; i++) {
      var v = stateValue(state, entity.room, entity.name, PRESENCE_ASPECTS[i]);
      if (v !== undefined) return v;
    }
    return undefined;
  }

  /* A person on Now: { home, seenAt }. home is the person's `presence`
   * aspect: true, false, or undefined when nothing publishes it. seenAt is
   * the last location fix in epoch ms, when a location adapter reports
   * one. The page writes the text; this only reads the values. */
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

  /* Applies one WebSocket message to the store (the state, health and
   * config maps, and the capped events feed). Returns the message type when
   * applied, and null for anything unrecognized. The caller renders, and
   * tracks per-key side effects, only when a message was applied.
   * `nowSeconds` stamps events whose message has no ts. */
  var EVENTS_CAP = 200;

  function applyMessage(store, msg, nowSeconds) {
    if (!msg || !msg.type) return null;
    if (msg.type === 'snapshot') {
      store.state = msg.state || {};
      store.forecasts = msg.forecasts || {};
      store.holds = msg.holds || {};
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
    if (msg.type === 'hold') {
      store.holds[msg.key] = msg.value;
      return 'hold';
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

  /* ---- chart geometry (docs/design.md#the-page) ----
   *
   * Where each drawn thing lands in the viewBox: the record, the current
   * forecast, the braid of kept issues, and the contributing sources. This
   * is arithmetic over timestamps and value ranges, so it lives here
   * rather than in the page, where tests can check the numbers directly
   * instead of through a DOM.
   */
  // Geometry for a series and, when the house has one, its forecast on the
  // same axis. Both share one time domain and one value scale. A forecast
  // drawn to its own scale beside its history cannot show whether the
  // house is about to get colder (docs/design.md#forecasts).
  // Points sit where their timestamps fall in the window, so a gap in the
  // record stays a gap. Points are spaced evenly only when timestamps are
  // missing. A live point past the window's end (a delta after the fetch)
  // extends the window.
  function chartGeometry(points, width, height, pad, win, forecast, issues, contributors) {
    var vals = points.map(function (p) { return p.value; }).filter(function (v) { return typeof v === 'number'; });
    // A forecast key names its source, so an aspect may have several live
    // forecasts at once. Each gets its own line, with no envelope over
    // them, for the same reason as the braid
    // (docs/design.md#charts-forecasts-and-sources).
    //
    // A forecast issued hours ago still holds what it said about the hours
    // since, and that part is drawn. Laid over the record, it lets the
    // reader compare a provider's forecast with what happened without
    // opening the braid.
    var beliefs = (forecast || []).map(function (b) {
      return {
        source: b.source, points: b.forecast.points, to: b.forecast.to,
        // Older than the span it has left to cover. It is still drawn,
        // because it is the house's current forecast, but it is marked as
        // stale (docs/design.md#charts-forecasts-and-sources).
        stale: forecastFreshness(b.forecast, Date.now()).stale
      };
    });
    var allVals = vals.slice();
    beliefs.forEach(function (b) {
      b.points.forEach(function (p) { allVals.push(p.v); });
    });
    // Every drawn issue shares the scale. A braid on its own y range could
    // not show whether the house was about to get colder.
    (issues || []).forEach(function (f) {
      f.points.forEach(function (pt) { allVals.push(pt.v); });
    });
    // Contributors share the scale for the same reason. A source drawn on
    // its own y range could look like it agrees when it does not.
    (contributors || []).forEach(function (c) {
      (c.points || []).forEach(function (p) {
        if (typeof p.value === 'number') allVals.push(p.value);
      });
    });
    if (allVals.length < 2) return null;
    var min = Math.min.apply(null, allVals);
    var max = Math.max.apply(null, allVals);
    var span = (max - min) || 1;
    var n = points.length;
    var times = points.map(function (p) { return Date.parse(p.ts); });
    var from = win ? win.from : times[0];
    var to = Math.max(win ? win.to : times[n - 1] || 0, times[n - 1] || 0);
    // The horizon extends the axis to the right, and the past keeps the
    // time span it had. Recorded history is only narrowed on screen with
    // the forecast drawn beside it, so the reader can see why.
    beliefs.forEach(function (b) { to = Math.max(to, b.to); });
    (issues || []).forEach(function (f) { to = Math.max(to, f.to); });
    var byTime = times.every(function (t) { return !isNaN(t); }) && to > from;
    var place = function (t, value) {
      var f = (t - from) / (to - from);
      return {
        x: pad + f * (width - 2 * pad),
        y: pad + (1 - (value - min) / span) * (height - 2 * pad)
      };
    };
    var coords = points.map(function (p, i) {
      var f = byTime ? (times[i] - from) / (to - from) : i / (n - 1);
      var x = pad + f * (width - 2 * pad);
      var y = pad + (1 - (p.value - min) / span) * (height - 2 * pad);
      return { x: x, y: y, value: p.value, ts: p.ts };
    });
    var geo = { coords: coords, min: min, max: max, from: from, to: to };
    if (contributors && contributors.length && byTime) {
      // One line per source, with no band across them. An envelope's edge
      // follows whichever sensor is highest at each instant, so it traces
      // a path no sensor took. The braid follows the same rule
      // (docs/design.md#sources).
      geo.contributors = contributors.map(function (c) {
        return {
          name: c.name,
          label: c.label,
          coords: (c.points || [])
            .filter(function (p) { return typeof p.value === 'number'; })
            .map(function (p) {
              var at = place(Date.parse(p.ts), p.value);
              return { x: at.x, y: at.y, value: p.value, ts: p.ts };
            })
            .filter(function (pt) { return !isNaN(pt.x); })
        };
      }).filter(function (c) { return c.coords.length > 1; });
    }
    if (issues && issues.length && byTime) {
      // Each stored issue is drawn as its own line, with no envelope over
      // them. An envelope's edge follows whichever issue is highest at
      // each instant, which is a path no issue predicted
      // (docs/wireframes/forecast-history.svg).
      geo.issues = issues.map(function (f, n) {
        return {
          issued: f.issued,
          source: f.source,
          age: issues.length > 1 ? n / (issues.length - 1) : 1,
          coords: f.points.map(function (pt) {
            var at = place(pt.t, pt.v);
            return { x: at.x, y: at.y, value: pt.v, ts: pt.t };
          })
        };
      });
    }
    if (beliefs.length && byTime) {
      geo.forecast = beliefs.map(function (b) {
        return {
          source: b.source,
          stale: b.stale,
          coords: b.points.map(function (p) {
            var at = place(p.t, p.v);
            return { x: at.x, y: at.y, value: p.v, ts: p.t };
          })
        };
      });
      // The line between the record and the forecast. It is placed at the
      // current time rather than at the last point, because a gap in
      // recording does not mean the present has moved.
      var nowAt = (Date.now() - from) / (to - from);
      if (nowAt > 0 && nowAt < 1) geo.nowX = pad + nowAt * (width - 2 * pad);
    }
    return geo;
  }

  /* ---- arbiter holds (docs/design.md#arbitrated-mode) ----
   *
   * Each arbiter publishes one document of what it is holding. Every
   * forwarded command takes a lease, not only a preemption, so most holds
   * are the house working normally. A hold is only worth reporting when it
   * displaced somebody.
   */
  var CMD_BANDS = ['automation', 'agent', 'family', 'manual'];

  // Deviation rows are built here, so the time formatting they need is
  // here too. Local time with no date, because a hold lasts at most an
  // hour or two.
  function clockOf(ts) {
    var d = new Date(ts);
    return isNaN(d.getTime()) ? '' : d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  }

  // Every live hold across every arbiter, with expired ones dropped. The
  // document states `until` and the reader applies it, as with a
  // forecast's `issued`. The arbiter also republishes at the deadline, so
  // this check is a second guard.
  function liveHolds(holds, now) {
    var out = [];
    Object.keys(holds || {}).forEach(function (key) {
      var doc = holds[key];
      if (!doc || !Array.isArray(doc.holds)) return;
      doc.holds.forEach(function (h) {
        if (!h || typeof h.entity !== 'string' || typeof h.aspect !== 'string') return;
        var until = Date.parse(h.until);
        if (isNaN(until) || until <= now) return;
        out.push({
          room: h.room, entity: h.entity, aspect: h.aspect,
          priority: h.priority, actor: h.actor,
          refused: typeof h.refused === 'number' ? h.refused : 0,
          until: until
        });
      });
    });
    return out.sort(function (a, b) { return a.until - b.until; });
  }

  // The hold in force over one aspect, or null. A control shows it as
  // "held" whether or not it displaced anyone.
  function holdOn(holds, room, entity, aspect, now) {
    return liveHolds(holds, now).filter(function (h) {
      return h.room === room && h.entity === entity && h.aspect === aspect;
    })[0] || null;
  }

  /* Whether a hold displaced somebody. That is the case when the hold's
   * band is above a band at which some unit is granted to command this
   * aspect. `driven` maps "room/entity/aspect" to that band, resolved from
   * the grant table at plan time.
   *
   * This is decided from the grants, not by observing refusals. Waiting
   * until the displaced automation is refused would make the deviation
   * depend on how often that automation publishes, which says something
   * about its author and not about the house. An override of a boost
   * schedule is a takeover from the moment it starts, even if the schedule
   * would not have written again until morning.
   */
  function displaces(hold, driven) {
    var band = (driven || {})[hold.room + '/' + hold.entity + '/' + hold.aspect];
    var below = CMD_BANDS.indexOf(band);
    var holder = CMD_BANDS.indexOf(hold.priority);
    return below !== -1 && holder !== -1 && holder > below;
  }

  /* The Now view's "out of the ordinary" list, in render order. Each
   * record is { tag, title, detail, target, button? }. target names what a
   * tap opens: {type:'unit', unit}, {type:'rooms'},
   * {type:'entity', room, entity}, {type:'setpoint', unit, param} for a
   * family-editable param, or {type:'unit', unit} for an owner param.
   * button is the optional corrective action. */
  function computeDeviations(model, state, health, config, aspects, holds, now) {
    aspects = aspects || {};
    now = now === undefined ? Date.now() : now;
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

    // 2. state: lights on. One row for all of them, with the corrective
    // action. The server fans it out at the manual band, so the family
    // always wins.
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

    // state: connectivity, WAN down
    entities.filter(function (e) { return e.capability === 'router'; }).forEach(function (e) {
      if (stateValue(state, e.room, e.name, 'wan') === false) {
        deviations.push(entityRow(e, e.label + ' — WAN down'));
      }
    });

    // state: a device gone quiet. The owning adapter published
    // available = false. This applies to any capability.
    entities.forEach(function (e) {
      if (stateValue(state, e.room, e.name, 'available') === false) {
        deviations.push(entityRow(e, e.label + ' unresponsive'));
      }
    });

    // state: an aspect the adapter's descriptor marks notable, when it is
    // true (an alarm flag, say). The adapter declares this, not the house
    // configuration.
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

    // 4. arbitration: an aspect held above the band a unit normally drives
    // it at. The house's own control is suspended here, and it resumes by
    // itself when the hold expires. This feed exists to show such cases. A
    // hold that displaced nobody is the family using the house and is not
    // listed. It still shows as `held` on the control.
    var entityByName = {};
    entities.forEach(function (e) { entityByName[e.name] = e; });
    liveHolds(holds, now).forEach(function (hold) {
      if (!displaces(hold, model.driven)) return;
      var e = entityByName[hold.entity];
      var label = e ? e.label || titleCase(hold.entity) : titleCase(hold.entity);
      var detail = 'held by ' + hold.actor + ' until ' + clockOf(hold.until);
      if (hold.refused) {
        detail += ' \u00b7 ' + hold.refused + (hold.refused === 1 ? ' wish' : ' wishes') + ' refused';
      }
      deviations.push({
        tag: 'hold',
        title: label + ' \u2014 ' + hold.aspect,
        detail: detail,
        until: hold.until,
        target: e
          ? { type: 'entity', room: hold.room, entity: hold.entity }
          : { type: 'rooms' }
      });
    });

    return deviations;
  }


  /* ---- aspect descriptors (docs/design.md#aspect-descriptors) ----
   * An adapter may describe an entity's aspects in its discovery record:
   * { schema, groups: [name...], fields: { aspect: { label, kind, group,
   * unit?, values?, valid?, notable?, command? } } }. The page renders the
   * description with the widgets it already has. This section maps
   * descriptor and state to a render plan. */
  var DIAGNOSTICS = 'diagnostics';

  // Display text for one value. The field's kind decides. Without a kind,
  // the value gets one decimal, and a degree sign when the aspect name
  // contains "temperature".
  function formatAspect(aspect, field, value) {
    if (value === undefined || value === null) return '—';
    var kind = field && field.kind;
    if (field && field.values) {
      // an enum's labels; a boolean may have them too ("locked"/"unlocked")
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

  /* ---- declared control grain (docs/design.md#controls-and-the-overlay) ----
   *
   * `dashboard.toml`'s `[[control]]` entries say how coarse a control is.
   * They are keyed by what the control changes (an entity's aspect, or a
   * unit's parameter), so one grain applies wherever that control is
   * drawn: room card, view or overlay. The derived step (a twentieth of
   * the range) is a guess, and the house may know better. Returns null
   * when nothing is declared, and the derived step is used.
   */
  function declaredStep(controls, target) {
    var match = (controls || []).filter(function (c) {
      if (!c || typeof c.step !== 'number' || !(c.step > 0)) return false;
      return target.entity !== undefined
        ? c.entity === target.entity && c.aspect === target.aspect
        : c.unit === target.unit && c.param === target.param;
    })[0];
    return match ? match.step : null;
  }

  // The control a described command renders as. These are the same
  // shapes as the param controls:
  //   - an enum is a segmented control, or a select above SELECT_ABOVE
  //     values;
  //   - a temperature with a step is a dial. Where a card has no room, the
  //     page draws its compact form, a stepper;
  //   - any other float with a step is a stepper;
  //   - any other number is a slider, with a coarse step for its ± buttons.
  // A command the family may not edit shows its value with a tier badge
  // instead. `commandable` is the dashboard's own grant on the capability.
  // Without it the control is disabled, as for every other widget.
  function controlFor(field, commandable, step) {
    var cmd = field && field.command;
    if (!cmd) return null;
    var tier = cmd.editable_by || 'owner';
    if (tier !== 'family') return { kind: 'readonly', tier: tier };
    var c = cmd.constraint || {};
    // step/min/max are used in attributes and arithmetic. A non-number
    // there means the descriptor is malformed, so no control is drawn. The
    // server refuses the command too.
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
        // A dial is an arc from min to max. Without both bounds there is
        // no arc to draw, so a stepper is used.
        var bounded = typeof c.min === 'number' && typeof c.max === 'number' && c.max > c.min;
        var kind = field.kind === 'temperature' && bounded ? 'dial' : 'stepper';
        return { kind: kind, step: cmd.step, min: c.min, max: c.max, disabled: !commandable };
      }
      var min = c.min !== undefined ? c.min : 0, max = c.max !== undefined ? c.max : 100;
      // A declared step applies to both the drag and the ± buttons. A
      // slider quantised to 5 beside buttons that move by 5.7 looks like a
      // bug. A quantised drag also means a slip of the finger lands one
      // notch off rather than at an arbitrary value.
      return {
        kind: 'slider', min: min, max: max,
        step: step || (cmd.type === 'int' ? 1 : 'any'),
        coarse: step || coarseStep(min, max, cmd.type === 'int' ? 1 : 0),
        disabled: !commandable
      };
    }
    return null;
  }

  /* A slider's ± step: a twentieth of the range, no finer than the value's
   * own step, rounded to a round number (5 on a percent, 1 on a small
   * integer range). */
  function coarseStep(min, max, atLeast) {
    var raw = (max - min) / 20;
    var nice = raw >= 5 ? 5 * Math.round(raw / 5) : raw >= 1 ? Math.round(raw) : Math.round(raw * 10) / 10;
    return Math.max(nice, atLeast, 0.1);
  }

  /* Sections of rows for an entity's detail, in render order. First come
   * the descriptor's groups as listed, then a diagnostics section for
   * every present aspect it does not describe. An undescribed entity gets
   * one 'state' section with a flat list. A described field's `valid`
   * pointer names the boolean aspect that marks the value stale. That
   * aspect sets the row's `stale` flag and is not listed itself.
   * Two aspects are part of the schema and need no descriptor.
   * `available` (device liveness, docs/design.md#availability) renders as
   * a boolean in the descriptor's `status` group, if it has one. Any
   * `{aspect}_valid` beside an undescribed `{aspect}` is handled like a
   * declared `valid` pointer.
   * Rows: { aspect, label, value, display, stale, numeric, control }. */
  function aspectPlan(entity, state, descriptor, commandable, controls) {
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
        control: controlFor(
          field, commandable, declaredStep(controls, { entity: entity.name, aspect: aspect })
        )
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

  /* The room-card row for a described entity: at most two headline
   * readings and the family-editable controls. The headline readings are
   * a convention, not part of the vocabulary. They are the first two rows
   * without a control in the first group that has any, so the adapter's
   * ordering decides. Card labels drop a trailing "(CODE)" that the
   * overlay keeps: "feed line (GT1)" becomes "feed line". */
  function cardPlan(entity, state, descriptor, commandable, controls) {
    var sections = aspectPlan(entity, state, descriptor, commandable, controls);
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

  /* The sparkline rows of a sensor's room card: every numeric row without
   * a control outside diagnostics, in the descriptor's order. A
   * thermometer's card therefore lists temperature and humidity but not
   * its link quality. The sensor widget keeps its sparklines instead of
   * using the described card. An undescribed sensor gets its flat state
   * list, sorted. These are the overlay's rows without the collapsed
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

  /* ---- the text behind a view ----
   *
   * A view is text in the house repo (dashboard.toml), and the page can
   * show it. Someone pointing at the screen can then name what they see,
   * to a person or to an agent editing the repo, in the file's own words.
   * The text is rendered from the parsed view that /api/model carries, in
   * the file's style: one inline table per widget, with a group's members
   * indented under it. Comments and spacing are not reproduced, but the
   * content is. It is read-only, and the dashboard does not write to the
   * house. */
  var WIDGET_KEY_ORDER = ['kind', 'entity', 'aspect', 'room', 'unit', 'label', 'hours'];

  function tomlValue(v) {
    // A JSON string literal is a valid TOML basic string.
    if (typeof v === 'string') return JSON.stringify(v);
    return String(v);
  }

  function orderedKeys(obj, first) {
    var keys = Object.keys(obj).filter(function (k) { return k !== 'widgets' && obj[k] !== null && obj[k] !== undefined; });
    return keys.sort(function (a, b) {
      var ia = first.indexOf(a), ib = first.indexOf(b);
      return (ia < 0 ? first.length : ia) - (ib < 0 ? first.length : ib) || (a < b ? -1 : a > b ? 1 : 0);
    });
  }

  function widgetToml(w, indent) {
    var fields = orderedKeys(w, WIDGET_KEY_ORDER).map(function (k) { return k + ' = ' + tomlValue(w[k]); });
    if (!w.widgets) return indent + '{ ' + fields.join(', ') + ' }';
    return indent + '{ ' + fields.concat(['widgets = [']).join(', ') + '\n' +
      w.widgets.map(function (m) { return widgetToml(m, indent + '  ') + ',\n'; }).join('') +
      indent + '] }';
  }

  /* The `[[view]]` block for `name`. For a house without the file, it is
   * the block that would keep a generated view as it is. Null for a name
   * no view has. Health and Not shown are part of the page frame, not
   * views. */
  function viewText(model, name) {
    var views = model && model.views;
    if (!views) {
      if (DEFAULT_VIEWS.indexOf(name) === -1) return null;
      return '# This house has no dashboard.toml: the dashboard draws its\n' +
        '# generated views. To arrange them, create dashboard.toml at the\n' +
        '# house root; this keeps this one as it is:\n\nschema = 1\n\n' +
        '[[view]]\nname = ' + tomlValue(name) + '\nkind = ' + tomlValue(name) + '\n';
    }
    var view = views.filter(function (v) { return v.name === name; })[0];
    if (!view) return null;
    var lines = ['[[view]]'];
    orderedKeys(view, ['name', 'label', 'kind']).forEach(function (k) {
      lines.push(k + ' = ' + tomlValue(view[k]));
    });
    if (view.widgets) {
      lines.push('widgets = [');
      view.widgets.forEach(function (w) { lines.push(widgetToml(w, '  ') + ','); });
      lines.push(']');
    }
    return lines.join('\n') + '\n';
  }

  /* ---- where the page is served from ----
   *
   * The dashboard unit's /api/model carries `about`. It has the core's
   * version and the commit it was built from (when the build was given
   * one), the house commit last applied, and the dashboard's own SDK
   * version, which is the release this copy of the page came from. The
   * page shows the core and house lines, with links. It shows the
   * dashboard's version only when it differs from the core's. */
  var REPO_URL = 'https://github.com/freol35241/homeostat';
  var ABOUT_LINKS = [
    { label: 'Source', href: REPO_URL },
    { label: 'Releases', href: REPO_URL + '/releases' },
    { label: 'Docs', href: REPO_URL + '#readme' },
    { label: 'Report an issue', href: REPO_URL + '/issues' }
  ];

  // Python writes a prerelease without the hyphen that semver puts before
  // it (0.14.0rc1, 0.14.0-rc1). Both name the same release.
  function sameRelease(a, b) {
    var norm = function (v) { return String(v).replace(/-(a|b|rc|alpha|beta)/, '$1'); };
    return norm(a) === norm(b);
  }

  function aboutLines(about) {
    about = about || {};
    var core = about.homeostat || {};
    var lines = [];
    if (core.version) {
      var line = { label: 'homeostat', text: core.version, href: REPO_URL + '/releases/tag/v' + core.version };
      if (core.commit) {
        line.commit = String(core.commit).slice(0, 7);
        line.commitHref = REPO_URL + '/commit/' + core.commit;
      }
      lines.push(line);
    }
    var page = (about.dashboard || {}).version;
    if (page && !(core.version && sameRelease(page, core.version))) {
      lines.push({
        label: 'dashboard', text: page, href: REPO_URL + '/releases/tag/v' + page,
        note: core.version ? 'not the core\'s release' : null
      });
    }
    var house = (about.house || {}).commit;
    if (house) {
      var dirty = /-dirty$/.test(house);
      lines.push({ label: 'house', text: house.replace(/-dirty$/, '').slice(0, 7) + (dirty ? ' + uncommitted changes' : '') });
    }
    return lines;
  }

  /* ---- pending commands ----
   *
   * A command is a request, not a write. It passes through arbitration,
   * the adapter's validation and finally the device's own readback, and
   * any of these can end it. If the page shows nothing between the tap
   * and stage 4, a slow device looks like a broken button, and the user
   * taps again.
   *
   * Showing the new value straight away would be wrong. Posting an
   * out-of-range value returns ok and is then dropped at stage 3, so the
   * control would show a value the house never took. The page shows the
   * stages instead, and the envelope's correlation id ties an event back
   * to the command it ended. The page shows the request as a request,
   * "asked 22.5", beside what the device still reports.
   *
   * An entry moves draft → sending → pending → (resolved). A draft is a
   * stepper the user is still tapping. The page holds it for a moment and
   * then sends one command for where the taps ended. Three taps of + are
   * one command for +1.5, rather than three commands each computed from a
   * readback that has not moved yet. */

  /* How long to wait for a readback before calling it unconfirmed. The
   * wire has no readback cadence, and the capability is the only thing the
   * browser knows about a device's class, so the wait is scaled by
   * capability. A z2m lamp answers in about a second, a lock waits on a
   * motor, and a burner behind a polling bridge can take half a minute.
   * The values are generous, because a timeout that fires early reports a
   * failure that did not happen. */
  var COMMAND_TIMEOUT_MS = {
    light: 8000,
    switch: 8000,
    lock: 15000,
    climate: 15000,
    burner: 60000
  };
  var DEFAULT_COMMAND_TIMEOUT_MS = 20000;

  /* The adapter knows more than the capability does
   * (docs/design.md#aspect-descriptors). A descriptor may declare
   * `readback_s`, the longest the device takes to report a command back,
   * for one command or for the whole entity. The per-command value takes
   * precedence, then the entity's, then the guess above. A value that is
   * not a positive number up to ten minutes is ignored, because a command
   * pending for an hour is worse than one judged on the guess. */
  var MAX_DECLARED_READBACK_S = 600;

  function declaredReadbackS(descriptor, aspect) {
    var field = descriptor && descriptor.fields && descriptor.fields[aspect];
    var candidates = [field && field.command && field.command.readback_s, descriptor && descriptor.readback_s];
    for (var i = 0; i < candidates.length; i++) {
      var s = candidates[i];
      if (typeof s === 'number' && isFinite(s) && s > 0 && s <= MAX_DECLARED_READBACK_S) return s;
    }
    return null;
  }

  function commandTimeoutMs(capability, descriptor, aspect) {
    var declared = declaredReadbackS(descriptor, aspect);
    if (declared !== null) return declared * 1000;
    return COMMAND_TIMEOUT_MS[capability] || DEFAULT_COMMAND_TIMEOUT_MS;
  }

  function pendingKey(room, entity, aspect) {
    return room + '/' + entity + '/' + aspect;
  }

  /* One command per (room, entity, aspect). A second tap on the same
   * control replaces the first, which is what the user means. The
   * replaced request is still remembered. Its value is added to `asked`,
   * so a readback of that value on the way is treated as progress rather
   * than as the device settling somewhere else. `before` keeps what the
   * device reported before the first tap of the sequence.
   * `stage` is 'pending' (the id is known) unless given: 'draft' while the
   * taps continue, and 'sending' while the POST is in flight. `seq`
   * identifies the POST, so its reply matches only the request it answers
   * (commandSent). `tolerance` is how far a readback may be from the
   * request and still count as a match. The brightness scale is finer
   * than the percent the control shows, and a bulb that rounds by one
   * step has done what it was asked. */
  function trackCommand(pending, cmd, nowMs, stage) {
    var key = pendingKey(cmd.room, cmd.entity, cmd.aspect);
    var prev = pending[key];
    pending[key] = {
      id: cmd.id || null,
      seq: cmd.seq || null,
      value: cmd.value,
      before: prev ? prev.before : cmd.before,
      asked: prev ? prev.asked.concat([prev.value]) : [],
      moved: prev ? prev.moved : false,
      tolerance: cmd.tolerance || 0,
      at: nowMs,
      timeoutMs: commandTimeoutMs(cmd.capability, cmd.descriptor, cmd.aspect),
      outcome: stage || 'pending'
    };
    return pending;
  }

  function pendingFor(pending, room, entity, aspect) {
    return pending[pendingKey(room, entity, aspect)] || null;
  }

  /* Where the next step of a stepper starts: the request in flight if
   * there is one, and otherwise the device's report. Stepping from a
   * report that has not moved yet would make a second tap repeat the
   * first. */
  function commandBase(pending, room, entity, aspect, current) {
    var entry = pendingFor(pending, room, entity, aspect);
    return entry ? entry.value : current;
  }

  function sameValue(a, b, tolerance) {
    if (typeof a === 'number' && typeof b === 'number') {
      return Math.abs(a - b) <= Math.max(tolerance || 0, 1e-6 * Math.max(1, Math.abs(a)));
    }
    return a === b;
  }

  /* The POST came back, and the request now waits on the bus. If the
   * dashboard unit found nothing subscribed to the key, the command went
   * nowhere, and the page reports that at once instead of waiting. A reply
   * for a request the user has since replaced is ignored, because the
   * newer request has its own POST. Replies are matched by `seq`, not by
   * value. On, off, on are three POSTs, and the first reply must not
   * match the third request. */
  function commandSent(pending, key, seq, reply) {
    var entry = pending[key];
    if (!entry || entry.outcome !== 'sending' || entry.seq !== seq) return null;
    if (!reply || !reply.id) {
      // An older dashboard unit that returns no id cannot be tracked. A
      // pending state that can never resolve is worse than none.
      delete pending[key];
      return null;
    }
    if (reply.heard === false) {
      delete pending[key];
      return { key: key, outcome: 'unheard', value: entry.value };
    }
    entry.id = reply.id;
    entry.outcome = 'pending';
    return null;
  }

  /* Stage 4: the device reported the commanded aspect. The reported value
   * decides the outcome:
   *   - The value asked for: confirmed. The exception is when it is also
   *     the value the device held before the sequence and nothing has
   *     moved since. After tapping on, then off, a bridge republishing the
   *     old "off" is not an answer to "off", because the "on" may still
   *     land. The timeout decides that case (expirePending).
   *   - The value it held before, or one the user asked for on the way:
   *     not an answer. A bridge that republishes on every poll sends the
   *     old value until the device moves. Counting that as confirmation
   *     would clear the control before anything happened.
   *   - Anything else: the device chose its own value (it clamped, or
   *     rounded to its resolution). The outcome is "adjusted", and the
   *     page says so.
   * A draft has not been sent, so nothing can answer it yet. */
  function resolveFromState(pending, key, value) {
    var parts = String(key).split('/');
    if (parts[0] !== 'home' || parts[1] !== 'state' || parts.length < 5) return null;
    var pk = pendingKey(parts[2], parts[3], parts[4]);
    var entry = pending[pk];
    if (!entry || (entry.outcome !== 'pending' && entry.outcome !== 'sending')) return null;
    var tol = entry.tolerance;
    var stale = !entry.moved && entry.asked.length > 0 && sameValue(entry.before, value, tol);
    if (sameValue(entry.value, value, tol) && !stale) {
      delete pending[pk];
      return { key: pk, outcome: 'confirmed', value: entry.value };
    }
    if (!sameValue(entry.before, value, tol)) entry.moved = true;
    var known = entry.asked.concat([entry.before]).some(function (v) { return sameValue(v, value, tol); });
    if (known) {
      entry.seen = value;
      return null;
    }
    delete pending[pk];
    return { key: pk, outcome: 'adjusted', value: entry.value, seen: value };
  }

  /* Stages 2 and 3. Both arrive as health events addressed by cmd_id.
   * `refuse` is not a failure. The command was well-formed and lost to a
   * higher band, and the user should do something other than retry. An
   * event without a cmd_id belongs to no command being tracked. */
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
          value: entry.value,
          by: event.holder_priority || 'another band',
          actor: event.holder_actor || null
        };
      }
      return { key: keys[i], outcome: 'rejected', value: entry.value, reason: event.reason || 'dropped' };
    }
    return null;
  }

  /* Nothing answered, which is not success. The outcome carries the last
   * value the device reported, if any, so the page can say "still reports
   * 21.0" rather than only "no answer". The exception is a device that
   * reported the asked value all along (on, then off, and the "on" never
   * landed). After the wait it is where it was asked to be, so the
   * command counts as confirmed. */
  function expirePending(pending, nowMs) {
    var out = [];
    Object.keys(pending).forEach(function (k) {
      var entry = pending[k];
      if (entry && (entry.outcome === 'pending' || entry.outcome === 'sending') &&
          nowMs - entry.at > entry.timeoutMs) {
        delete pending[k];
        if (entry.seen !== undefined && sameValue(entry.value, entry.seen, entry.tolerance)) {
          out.push({ key: k, outcome: 'confirmed', value: entry.value });
        } else {
          out.push({ key: k, outcome: 'unconfirmed', value: entry.value, seen: entry.seen });
        }
      }
    });
    return out;
  }

  /* How long an ended command's outcome stays on its control. A
   * confirmation needs only a glance. Any other outcome stays long enough
   * to be read, or until the next tap on that control replaces it. */
  var OUTCOME_SHOWN_MS = { confirmed: 4000 };
  var DEFAULT_OUTCOME_SHOWN_MS = 15000;

  function noteOutcome(recent, outcome, nowMs) {
    recent[outcome.key] = Object.assign({ at: nowMs }, outcome);
    return recent;
  }

  /* Drops every outcome whose time is up, whether or not its control is
   * on screen (recentFor only prunes what it is asked about). Returns
   * whether anything was dropped, so the caller redraws only then. */
  function pruneRecent(recent, nowMs) {
    var gone = false;
    Object.keys(recent).forEach(function (k) {
      if (!recentFor(recent, k, nowMs)) gone = true;
    });
    return gone;
  }

  function recentFor(recent, key, nowMs) {
    var entry = recent[key];
    if (!entry) return null;
    var shown = OUTCOME_SHOWN_MS[entry.outcome] || DEFAULT_OUTCOME_SHOWN_MS;
    if (nowMs - entry.at > shown) {
      delete recent[key];
      return null;
    }
    return entry;
  }

  /* ---- forecasts (docs/design.md#forecasts) ----
   *
   * The house's current forecast for a series, carried live beside its
   * present value. The wire shape is {schema, issued, points:[{t, v, d?}]}.
   * `d` is a point's extent in seconds, and is absent for an instant. */

  // The key prefix every source's forecast for one aspect sits under.
  function forecastPrefix(room, entity, aspect) {
    return 'home/forecast/' + room + '/' + entity + '/' + aspect + '/';
  }

  // Every source with a live forecast for one aspect, sorted so a chart
  // and its legend keep the same order between renders. A forecast key
  // names its source (docs/design.md#forecasts), so several providers
  // appear side by side and do not overwrite each other.
  function forecastSourcesFor(forecasts, room, entity, aspect) {
    var prefix = forecastPrefix(room, entity, aspect);
    return Object.keys(forecasts || {})
      .filter(function (k) {
        // The source is the last segment. A deeper key is not this
        // aspect's forecast.
        return k.indexOf(prefix) === 0 &&
          k.length > prefix.length &&
          k.indexOf('/', prefix.length) === -1;
      })
      .map(function (k) { return k.slice(prefix.length); })
      .sort();
  }

  // One source's current forecast, decoded, or null.
  function forecastFor(forecasts, room, entity, aspect, source) {
    return decodeForecast(
      forecasts && forecasts[forecastPrefix(room, entity, aspect) + source]
    );
  }

  // Every live forecast for one aspect, decoded, in source order. Each is
  // drawn as its own line with no envelope over them, as in the braid. An
  // envelope's edge follows whichever source is highest at each instant,
  // which is a path no source predicted.
  function forecastsFor(forecasts, room, entity, aspect) {
    var out = [];
    forecastSourcesFor(forecasts, room, entity, aspect).forEach(function (source) {
      var decoded = forecastFor(forecasts, room, entity, aspect, source);
      if (decoded) out.push({ source: source, forecast: decoded });
    });
    return out;
  }

  // One forecast document, live from the bus or stored by the recorder.
  // Both have the same shape, because the store replies in the wire
  // format. Points become millisecond timestamps here, so the chart can
  // place them on the same axis as recorded history and callers do not
  // have to parse them again.
  function decodeForecast(doc) {
    if (!doc || !doc.points || !doc.points.length) return null;
    var issued = Date.parse(doc.issued);
    var points = [];
    for (var i = 0; i < doc.points.length; i++) {
      var p = doc.points[i];
      var t = Date.parse(p.t);
      if (isNaN(t) || typeof p.v !== 'number') return null;  // malformed: show nothing
      points.push({ t: t, v: p.v, d: typeof p.d === 'number' ? p.d : null });
    }
    // The horizon runs to the end of a final interval, not its start.
    // Otherwise a coarse trailing window would be drawn as a dot.
    var last = points[points.length - 1];
    return {
      issued: isNaN(issued) ? null : issued,
      // Passed through from the recorder, which tags each stored issue
      // with the source of its key. Without it, a braid over several
      // providers would not show which issue came from which, and "these
      // issues disagree" would look the same as "these providers
      // disagree".
      source: typeof doc.source === 'string' ? doc.source : null,
      points: points,
      from: points[0].t,
      to: last.d ? last.t + last.d * 1000 : last.t
    };
  }

  // What a tile says about a horizon: where the series is going, not just
  // where it is. An extreme is only worth showing if the series moves, so
  // a flat horizon reports nothing rather than "min = max".
  function horizonSummary(forecast, fromTs) {
    if (!forecast) return null;
    // Only points ahead of `fromTs` count. An issue made hours ago still
    // holds what it said about the hours since. An extreme in that part
    // is already in the past, so it is not shown as where the series is
    // going.
    var pts = forecast.points.filter(function (p) {
      return fromTs === undefined || fromTs === null || p.t > fromTs;
    });
    if (pts.length < 2) return null;
    var lo = pts[0], hi = pts[0];
    for (var i = 1; i < pts.length; i++) {
      if (pts[i].v < lo.v) lo = pts[i];
      if (pts[i].v > hi.v) hi = pts[i];
    }
    if (lo.v === hi.v) return null;
    return { min: lo, max: hi };
  }

  // How old a forecast is, and whether any of it is still in the future.
  // Staleness is judged from `issued` alone (docs/design.md#forecasts),
  // and the maximum age is the consumer's choice, not a TTL in the core.
  // This is the dashboard's policy:
  //   expired: the horizon has run out, so nothing is left ahead to draw.
  //   stale:   older than the span it still has left to cover. This
  //            scales with the forecast instead of being a constant per
  //            aspect. A day-ahead curve issued at 13:00 is fresh all
  //            evening and stale by the next afternoon, when its successor
  //            is long overdue. A ten-minute-old two-day forecast is not
  //            stale.
  // A document whose `issued` did not parse has no age. It can expire but
  // is never called stale, because the page does not guess a fact the
  // producer did not state.
  function forecastFreshness(forecast, now) {
    if (!forecast) return null;
    var remaining = forecast.to - now;
    var age = forecast.issued === null ? null : now - forecast.issued;
    return {
      issued: forecast.issued,
      age: age,
      remaining: remaining,
      expired: remaining <= 0,
      stale: remaining > 0 && age !== null && age > remaining
    };
  }

  /* ---- declared sources (docs/design.md#sources) ----
   *
   * What a computed value is derived from, in the form the overlay needs.
   * The entity file declares contributors for the whole entity, not per
   * aspect, so each contributor is shown under the aspect it contributes
   * to. A fused temperature derived from `temperature` readings shows them
   * under `temperature`, and an unrelated `humidity` chart is unchanged.
   */
  function contributorsFor(entities, entityName, aspect) {
    var owner = (entities || []).filter(function (e) { return e.name === entityName; })[0];
    if (!owner || !owner.sources) return [];
    var byName = {};
    (entities || []).forEach(function (e) { byName[e.name] = e; });
    var out = [];
    Object.keys(owner.sources).sort().forEach(function (name) {
      var src = owner.sources[name];
      if (!src || src.aspect !== aspect) return;
      var e = byName[src.entity];
      // A contributor missing from the model cannot be drawn. The plan
      // refuses an unknown one (`source-unknown-entity`), so this means the
      // page is newer than the model, not that the house is wrong.
      if (!e) return;
      out.push({
        name: name,
        entity: src.entity,
        aspect: src.aspect,
        label: e.label || titleCase(src.entity),
        // The contributor's own caveat. The aspect descriptor cannot say
        // it because it does not apply to every source. Examples are one
        // sensor in the sun, or a reading with an offset the house itself
        // writes, which must not be fused back in.
        note: typeof src.note === 'string' ? src.note : null
      });
    });
    return out;
  }

  /* Which declared sources were actually folded in, and when that last
   * changed (docs/design.md#which-sources-a-computation-actually-used).
   *
   * Events arrive oldest first and only on a change, so the last one
   * before the window closes is the current state. A source with no
   * events is treated as contributing, as its declaration says. No events
   * means nothing has reported otherwise, not that the state is unknown.
   */
  function sourceUsage(events, contributors) {
    var state = {};
    (contributors || []).forEach(function (c) {
      state[c.name] = { used: true, since: null };
    });
    (events || []).forEach(function (e) {
      if (!e || typeof e.source !== 'string') return;
      // An event for a source this entity no longer declares has no line
      // to annotate.
      if (!Object.prototype.hasOwnProperty.call(state, e.source)) return;
      state[e.source] = { used: !!e.used, since: e.ts || null };
    });
    return state;
  }

  // Stored issues in the form the chart needs: each decoded like a live
  // forecast, newest last, and only those with something to draw. The
  // wire already sends them oldest first, but sorting here means callers
  // do not depend on that.
  function decodeIssues(issues) {
    var out = [];
    (issues || []).forEach(function (doc) {
      var decoded = decodeForecast(doc);
      if (decoded) out.push(decoded);
    });
    out.sort(function (a, b) { return (a.issued || a.from) - (b.issued || b.from); });
    return out;
  }

  // A stored issue from before forecast keys had a source segment carries
  // the recorder's reserved name for an unrecorded source
  // (adapters/recorder.py, LEGACY_SOURCE). Showing it as-is would put a
  // provider called `_unknown` beside the real ones, which is the made-up
  // provenance the recorder avoids writing.
  function sourceLabel(source) {
    return source === '_unknown' ? 'source not recorded' : source;
  }

  // What every stored issue said about one instant, as one column of the
  // chart (docs/wireframes/forecast-history.svg). The spread is the value
  // of interest. The count says how much the spread can be trusted.
  function columnAt(issues, when) {
    var lo = null, hi = null, n = 0;
    for (var i = 0; i < issues.length; i++) {
      var v = valueAt(issues[i], when);
      if (v === null) continue;
      n += 1;
      if (lo === null || v < lo) lo = v;
      if (hi === null || v > hi) hi = v;
    }
    return n ? { count: n, min: lo, max: hi } : null;
  }

  // One issue's value at an instant, using the SDK's extent rule. A point
  // with `d` covers [t, t+d), and an instant covers only itself. Between
  // two instants the value is interpolated. Outside every point's range
  // the result is null rather than a guess.
  function valueAt(forecast, when) {
    var pts = forecast.points;
    if (!pts.length || when < pts[0].t || when > forecast.to) return null;
    var lo = 0;
    for (var i = 0; i < pts.length; i++) {
      if (pts[i].t <= when) lo = i; else break;
    }
    var left = pts[lo];
    if (left.d) return when < left.t + left.d * 1000 ? left.v : null;
    if (left.t === when) return left.v;
    var right = pts[lo + 1];
    if (!right) return null;
    return left.v + (right.v - left.v) * ((when - left.t) / (right.t - left.t));
  }

  /* ---- history shapes ----
   *
   * The recorder summarises a window in two ways
   * (docs/design.md#read-path). `bucket` is for a line chart, with one
   * point per bucket, so a frequently updated series fills a week instead
   * of showing only its last hour. `changes` is for a timeline of the
   * runs of a state. When there is a descriptor, it decides. An enum or a
   * boolean is drawn as runs, not a curve, whatever the JS type of its
   * values. An enum coded as integers (ivt490's operating_mode) would
   * otherwise be drawn as a line between its codes and averaged. Without
   * a descriptor, the value's type decides. */

  function historyShape(value, field) {
    var kind = field && field.kind;
    if (kind === 'enum' || kind === 'boolean') return 'timeline';
    return typeof value === 'number' ? 'chart' : 'timeline';
  }

  /* One bucket per drawn column. The chart's viewBox width is the most
   * points it can show separately. */
  function bucketSeconds(hours, width) {
    return Math.max(1, Math.round(hours * 3600 / width));
  }

  /* The runs of a state from change rows. Each row starts a run that ends
   * where the next begins, or at the window's end. Nothing is known before
   * the first row, so no run starts before it, and the timeline shows a
   * gap there. Times are epoch ms. */
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

  /* The stat row under a timeline: how long the state was `true` (only
   * for a boolean), how many changes the window holds, and the current
   * value. */
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

  /* ---- views (docs/design.md#views-are-text) ----
   *
   * dashboard.toml's [[view]] list is the nav. Without the file, the
   * generated views are used. Health and "Not shown" are part of the page
   * frame and are always drawn. They are not views here. Everything below
   * is a pure function of the model that /api/model serves. */

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

  /* A view's widgets with every group expanded. A group is only a card
   * around its members, not a placement or a destination of its own. */
  function flatWidgets(view) {
    var out = [];
    (view.widgets || []).forEach(function (w) {
      if (w.kind === 'group') (w.widgets || []).forEach(function (m) { out.push(m); });
      else out.push(w);
    });
    return out;
  }

  /* What no view places: entities and family params the family cannot
   * reach from the nav.
   * An entity is placed by any of: a widget naming it, its room, a unit
   * that publishes or drives it, a `people` widget when it is a person, or
   * a generated `rooms` view (every entity) or `now` view (people).
   * A param is placed by a `params` or `unit` widget for its unit, or by a
   * generated `setpoints` view.
   * Deviations and the map place nothing, because they show signals, not
   * an inventory. */
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

  /* The unit card's four relations. Each is read from the manifest and
   * the grant table rather than declared for the card: family params, the
   * entities it owns, the fields its cmd grants reach, and the fields its
   * state subscriptions read.
   * Drives and From are fields. The model carries them as
   * { entity, aspect } (dashboard.py, unit_relations), and they are
   * resolved here to { entity: <the entity>, aspect }. They are fields
   * because an automation that commands a lamp and subscribes to it names
   * the same entity in both sections, and only the aspect tells them
   * apart. The labels belong to the page and are not vocabulary. */
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

  /* Which of the nav's views a deviation's tap should open. A setpoint
   * opens the first view with its unit's params (or a generated
   * Setpoints). The lights-on deviation opens a generated Rooms. null
   * means no view shows it, and the page falls back to an overlay or to
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
    liveHolds: liveHolds,
    holdOn: holdOn,
    displaces: displaces,
    forecastFor: forecastFor,
    forecastSourcesFor: forecastSourcesFor,
    forecastsFor: forecastsFor,
    decodeForecast: decodeForecast,
    contributorsFor: contributorsFor,
    sourceUsage: sourceUsage,
    decodeIssues: decodeIssues,
    sourceLabel: sourceLabel,
    columnAt: columnAt,
    valueAt: valueAt,
    chartGeometry: chartGeometry,
    horizonSummary: horizonSummary,
    forecastFreshness: forecastFreshness,
    formatAspect: formatAspect,
    controlFor: controlFor,
    declaredStep: declaredStep,
    coarseStep: coarseStep,
    SELECT_ABOVE: SELECT_ABOVE,
    aspectPlan: aspectPlan,
    cardPlan: cardPlan,
    sensorCardPlan: sensorCardPlan,
    COMMAND_TIMEOUT_MS: COMMAND_TIMEOUT_MS,
    DEFAULT_COMMAND_TIMEOUT_MS: DEFAULT_COMMAND_TIMEOUT_MS,
    commandTimeoutMs: commandTimeoutMs,
    viewText: viewText,
    ABOUT_LINKS: ABOUT_LINKS,
    aboutLines: aboutLines,
    pendingKey: pendingKey,
    trackCommand: trackCommand,
    pendingFor: pendingFor,
    commandBase: commandBase,
    commandSent: commandSent,
    resolveFromState: resolveFromState,
    resolveFromEvent: resolveFromEvent,
    expirePending: expirePending,
    noteOutcome: noteOutcome,
    recentFor: recentFor,
    pruneRecent: pruneRecent
  };
});
