/* The static dashboard demo's stand-in for the dashboard unit.
 *
 * The page is the real adapters/dashboard.html with its real assets; this
 * script, loaded ahead of it, answers what the dashboard unit would: the
 * model, the WebSocket's snapshot and deltas, history, forecasts, logs,
 * and commands. There is no house behind it. The data is the browser
 * tests' fixture house (tests/browser/fixtures), re-stamped against now
 * as tests/browser/server.py does, with history generated per request so
 * nothing in it goes stale. A command is "obeyed" in the page alone: the
 * device's readback arrives a moment later, as it would from a real one.
 *
 * Built into the Pages site by scripts/build_demo_site.py, which also
 * writes demo/data.js (window.HOMEOSTAT_DEMO = {model, snapshot}).
 */
(function () {
  'use strict';

  var DATA = window.HOMEOSTAT_DEMO;
  var state = DATA.snapshot.state;
  var sockets = [];
  var commandIds = 0;

  function iso(ms) { return new Date(ms).toISOString().replace(/\.\d{3}Z$/, 'Z'); }

  // A small deterministic noise source per series, so a chart does not
  // reshuffle every time it is redrawn.
  function seeded(text) {
    var h = 2166136261;
    for (var i = 0; i < text.length; i++) { h ^= text.charCodeAt(i); h = Math.imul(h, 16777619); }
    return function () {
      h ^= h << 13; h ^= h >>> 17; h ^= h << 5;
      return ((h >>> 0) % 10000) / 10000;
    };
  }

  function currentValue(entity, aspect) {
    for (var key in state) {
      var parts = key.split('/');
      if (parts[3] === entity && parts[4] === aspect) return { key: key, room: parts[2], value: state[key] };
    }
    return null;
  }

  /* ---- the snapshot, re-stamped against now ---- */

  function freshSnapshot() {
    var now = Date.now();
    var snap = JSON.parse(JSON.stringify(DATA.snapshot));
    snap.state = state;
    Object.keys(snap.forecasts || {}).forEach(function (key) {
      var doc = snap.forecasts[key];
      doc.issued = iso(now - 3600e3);
      doc.points = (doc.points || []).map(function (p, i) {
        return Object.assign({}, p, { t: iso(now + i * 3600e3) });
      });
    });
    Object.keys(snap.holds || {}).forEach(function (key) {
      var doc = snap.holds[key];
      doc.holds = (doc.holds || []).map(function (h) {
        return Object.assign({}, h, { since: iso(now - 300e3), until: iso(now + 1500e3) });
      });
    });
    snap.type = 'snapshot';
    return snap;
  }

  /* ---- history: a plausible past that ends at the present value ---- */

  function history(params) {
    var entity = params.get('entity'), aspect = params.get('aspect');
    var hours = parseFloat(params.get('hours') || '24');
    var now = Date.now();
    var current = currentValue(entity, aspect);
    var points = [];
    if (current) {
      var rand = seeded(entity + '/' + aspect);
      var v = current.value;
      if (typeof v === 'number') {
        var amp = Math.max(0.3, Math.abs(v) * 0.05);
        var step = Math.max(60e3, hours * 3600e3 / 160);
        for (var t = now - hours * 3600e3; t <= now; t += step) {
          var phase = 2 * Math.PI * (t - now) / 86400e3;
          var value = v + amp * Math.sin(phase) + amp * 0.15 * (rand() - 0.5);
          points.push({ ts: iso(t), room: current.room, value: Math.round(value * 100) / 100 });
        }
        points[points.length - 1].value = v;
      } else if (typeof v === 'boolean') {
        // Runs of a few hours, the last one the present state.
        var runs = [], t2 = now, flag = v;
        while (t2 > now - hours * 3600e3) {
          runs.push({ ts: iso(Math.max(t2 - (1 + rand() * 4) * 3600e3, now - hours * 3600e3)), value: flag });
          t2 -= (1 + rand() * 4) * 3600e3;
          flag = !flag;
        }
        runs.reverse().forEach(function (r) { points.push({ ts: r.ts, room: current.room, value: r.value }); });
      } else {
        points.push({ ts: iso(now - hours * 3600e3), room: current.room, value: v });
      }
    }
    return { series: [{ key: 'home/history/state/' + entity + '/' + aspect, points: points }] };
  }

  function forecasts(params) {
    var now = Date.now();
    var key = Object.keys(DATA.snapshot.forecasts || {}).find(function (k) {
      var parts = k.split('/');
      return parts[3] === params.get('entity') && parts[4] === params.get('aspect');
    });
    var base = key ? (DATA.snapshot.forecasts[key].points || [])[0] : null;
    var level = base && typeof base.v === 'number' ? base.v : 1;
    var issues = [6, 3, 1].map(function (age) {
      var issued = now - age * 3600e3;
      var pts = [];
      for (var h = 0; h < 12; h++) {
        pts.push({ t: iso(issued + h * 3600e3), v: Math.round((level + 0.08 * Math.sin(h / 2) + age * 0.02) * 100) / 100 });
      }
      return { schema: 1, issued: iso(issued), source: key ? key.split('/').pop() : 'model', points: pts };
    });
    return { issues: issues };
  }

  /* ---- deltas, as the hub sends them ---- */

  function push(message) {
    sockets.forEach(function (s) { s._deliver(message); });
  }

  function setState(key, value) {
    state[key] = value;
    push({ type: 'state', key: key, value: value });
  }

  function command(body) {
    var key = 'home/state/' + body.room + '/' + body.entity + '/' + body.aspect;
    // The device's readback, a moment after the command.
    setTimeout(function () {
      setState(key, body.value);
      if (body.aspect === 'brightness' && typeof body.value === 'number') {
        setState('home/state/' + body.room + '/' + body.entity + '/on', body.value > 0);
      }
    }, 350 + Math.random() * 400);
    commandIds += 1;
    return { ok: true, id: 'demo' + commandIds };
  }

  function lightsOff() {
    var sent = 0;
    Object.keys(state).forEach(function (key) {
      var parts = key.split('/');
      var entity = (DATA.model.entities || []).find(function (e) { return e.name === parts[3]; });
      if (entity && entity.capability === 'light' && parts[4] === 'on' && state[key] === true) {
        sent += 1;
        setTimeout(function () { setState(key, false); }, 300);
      }
    });
    return { ok: true, sent: sent };
  }

  /* ---- fetch ---- */

  function respond(body, status) {
    return new Response(JSON.stringify(body), {
      status: status || 200, headers: { 'Content-Type': 'application/json' }
    });
  }

  var realFetch = window.fetch.bind(window);
  window.fetch = function (input, init) {
    var url = new URL(typeof input === 'string' ? input : input.url, location.href);
    var path = url.pathname;
    var at = path.lastIndexOf('/api/');
    if (path.slice(-14) === '/tiles.pmtiles') return Promise.resolve(new Response('', { status: 404 }));
    if (at < 0) return realFetch(input, init);
    var route = path.slice(at);
    var body = {};
    try { body = init && init.body ? JSON.parse(init.body) : {}; } catch (e) { body = {}; }
    if (route === '/api/model') return Promise.resolve(respond(Object.assign({}, DATA.model, { tiles: false })));
    if (route === '/api/history') return Promise.resolve(respond(history(url.searchParams)));
    if (route === '/api/forecasts') return Promise.resolve(respond(forecasts(url.searchParams)));
    if (route === '/api/source-events') return Promise.resolve(respond({ events: [] }));
    if (route === '/api/logs') {
      return Promise.resolve(respond({ lines: [
        { stream: 'stdout', line: 'running (a demo house: these lines are made up)' }
      ] }));
    }
    if (route === '/api/cmd') return Promise.resolve(respond(command(body)));
    if (route === '/api/lights/off') return Promise.resolve(respond(lightsOff()));
    if (route === '/api/param') {
      var key = 'home/config/' + body.unit + '/' + body.param;
      setTimeout(function () { push({ type: 'config', key: key, value: body.value }); }, 250);
      return Promise.resolve(respond({ ok: true, value: body.value }));
    }
    // Loudly: the page asked for something the dashboard unit answers and
    // this stand-in does not. tests/browser/run.py (PagesDemo) fails on it,
    // which is what keeps this file in step with the unit.
    console.error('demo: no stand-in for ' + route);
    return Promise.resolve(respond({ error: 'not in the demo' }, 404));
  };

  /* ---- the WebSocket ---- */

  function FakeSocket(url) {
    var self = this;
    this.url = url;
    this.readyState = 0;
    this._hub = /\/ws$/.test(new URL(url, location.href).pathname);
    setTimeout(function () {
      self.readyState = 1;
      if (self.onopen) self.onopen({ type: 'open' });
      if (self._hub) {
        sockets.push(self);
        self._deliver(freshSnapshot());
      }
    }, 50);
  }
  FakeSocket.CONNECTING = 0; FakeSocket.OPEN = 1; FakeSocket.CLOSING = 2; FakeSocket.CLOSED = 3;
  FakeSocket.prototype._deliver = function (message) {
    if (this.readyState === 1 && this.onmessage) this.onmessage({ data: JSON.stringify(message) });
  };
  FakeSocket.prototype.send = function () {};
  FakeSocket.prototype.addEventListener = function (type, fn) { this['on' + type] = fn; };
  FakeSocket.prototype.removeEventListener = function () {};
  FakeSocket.prototype.close = function () {
    this.readyState = 3;
    sockets = sockets.filter(function (s) { return s !== this; }, this);
  };
  window.WebSocket = FakeSocket;

  /* ---- a house that is not standing still ---- */

  setInterval(function () {
    var numeric = Object.keys(state).filter(function (k) {
      return typeof state[k] === 'number' && /temperature|price|humidity|power/.test(k);
    });
    if (!numeric.length) return;
    var key = numeric[Math.floor(Math.random() * numeric.length)];
    var step = /price/.test(key) ? 0.01 : /power/.test(key) ? 2 : 0.1;
    setState(key, Math.round((state[key] + (Math.random() < 0.5 ? -step : step)) * 100) / 100);
  }, 6000);

  /* ---- say what this is ---- */

  document.addEventListener('DOMContentLoaded', function () {
    var note = document.createElement('div');
    note.textContent = 'Demo: a simulated house, running entirely in your browser. Nothing here controls anything.';
    note.setAttribute('role', 'note');
    note.style.cssText = 'position:fixed;left:50%;bottom:12px;transform:translateX(-50%);z-index:9999;' +
      'max-width:calc(100% - 32px);padding:6px 12px;border-radius:8px;font:13px/1.4 system-ui,sans-serif;' +
      'background:rgba(20,20,20,.82);color:#fff;pointer-events:none;text-align:center';
    document.body.appendChild(note);
    // Above the phone layout's tab bar, wherever that is: measured, not a
    // copy of the page's breakpoint.
    function place() {
      var tabs = document.getElementById('tabs');
      var shown = tabs && getComputedStyle(tabs).display !== 'none';
      note.style.bottom = ((shown ? tabs.offsetHeight : 0) + 12) + 'px';
    }
    place();
    window.addEventListener('resize', place);
    setTimeout(place, 500);
  });
})();
