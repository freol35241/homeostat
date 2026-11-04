/* The map widget. The map shows every person, and every tracker no person
 * lists, rather than being a per-entity row (see
 * docs/design.md#map-and-people). With `hours` it also draws each one's
 * track over that window. Leaflet's map object has state (pan/zoom, tile
 * cache) and must not be recreated on every state-delta render. Its
 * container is therefore a detached DOM node kept in mapState and moved
 * into a new slot on each render, instead of being rebuilt from an HTML
 * string like the rest of the view. */
import { byId, html } from '../html.js';
import logic from '../logic.js';
import { stateValue, store } from '../store.js';
import { relTime } from './entity.js';

var mapState = { container: null, map: null, tileLayer: null, markers: {}, circles: {}, tracks: {}, fitted: false };

// How often a track's history is read again. The window's start moves
// with the clock, and so does the fade.
var TRACK_REFRESH_MS = 5 * 60e3;
// The number of fade steps along a track.
var TRACK_BANDS = 8;
// The most fixes a track reads per series: the history endpoint's cap.
var TRACK_LIMIT = 5000;
// The window `lat`, `lon` and `accuracy` are read over: the history
// endpoint's longest. A changes read starts at the first change inside
// its window, and these publish only on change, so a person who has not
// moved since before the track's window has no row in it. Each fix needs
// the value as it stood when the fix was taken.
var TRACK_LOOKBACK_HOURS = 24 * 31;
// The aspects a fix is paired from, beside `fixed_at`.
var FIX_ASPECTS = ['lat', 'lon', 'accuracy'];

function personLatLon(entity) {
  var lat = stateValue(entity.room, entity.name, 'lat');
  var lon = stateValue(entity.room, entity.name, 'lon');
  if (typeof lat !== 'number' || typeof lon !== 'number') return null;
  return [lat, lon];
}

function mapThemeName() {
  return (window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches) ? 'dark' : 'light';
}

// The accent colour as the stylesheet sets it for the current theme.
// Leaflet writes a colour into an SVG attribute, where a CSS variable
// does not resolve.
function accentColor() {
  return getComputedStyle(document.documentElement).getPropertyValue('--accent').trim();
}

function personDivIcon(entity, color) {
  return L.divIcon({
    className: 'person-marker',
    // Leaflet takes a string here (and for a popup's content below), not a Markup
    html: String(html`<div class="person-marker-dot"${color ? html` style="background:${color}"` : ''}></div><div class="person-marker-label">${entity.label}</div>`),
    iconSize: [12, 12],
    iconAnchor: [6, 6]
  });
}

function personPopupHtml(entity) {
  var battery = stateValue(entity.room, entity.name, 'battery');
  var fixedAt = stateValue(entity.room, entity.name, 'fixed_at');
  var parts = [];
  if (typeof battery === 'number') parts.push('battery ' + Math.round(battery) + '%');
  if (typeof fixedAt === 'number') parts.push(relTime(fixedAt * 1000));
  return String(html`<b>${entity.label}</b>${parts.length ? html`<br>${parts.join(' · ')}` : ''}`);
}

function ensureMap() {
  if (mapState.map) return;
  if (!mapState.container) {
    mapState.container = document.createElement('div');
    mapState.container.id = 'person-map';
    mapState.container.className = 'map-wrap';
  }
  // No default setView. The center and zoom are set only once real fixes
  // exist (updateMapMarkers), so no coordinate is hardcoded. Layers added
  // before then wait for it (Leaflet defers them with whenReady).
  mapState.map = L.map(mapState.container);
  if (store.model.tiles) {
    mapState.tileLayer = protomapsL.leafletLayer({ url: '/tiles.pmtiles', theme: mapThemeName() }).addTo(mapState.map);
  }
}

function updateMapMarkers(persons, opts) {
  var map = mapState.map;
  var bounds = [];
  var seen = {};
  persons.forEach(function (e) {
    var key = e.room + '/' + e.name;
    seen[key] = true;
    var color = logic.hexColor((opts.colors || {})[e.name]);
    var latlon = personLatLon(e);
    if (!opts.hours) dropTrack(key);
    // An entity that has never had a position has no track to read.
    else if (latlon) updateTrack(key, e, opts.hours, color || accentColor());
    if (!latlon) {
      if (mapState.markers[key]) { map.removeLayer(mapState.markers[key]); delete mapState.markers[key]; }
      if (mapState.circles[key]) { map.removeLayer(mapState.circles[key]); delete mapState.circles[key]; }
      return;
    }
    bounds.push(latlon);
    var marker = mapState.markers[key];
    if (!marker) {
      marker = L.marker(latlon, { icon: personDivIcon(e, color) }).addTo(map);
      marker.color = color;
      mapState.markers[key] = marker;
    } else {
      marker.setLatLng(latlon);
      // One map serves every view, and two views can colour it apart.
      if (marker.color !== color) { marker.setIcon(personDivIcon(e, color)); marker.color = color; }
    }
    marker.bindPopup(personPopupHtml(e));

    var accuracy = stateValue(e.room, e.name, 'accuracy');
    var circle = mapState.circles[key];
    var ring = color || accentColor();
    if (typeof accuracy === 'number') {
      if (!circle) {
        mapState.circles[key] = L.circle(latlon, { radius: accuracy, color: ring, weight: 1, fillOpacity: .1 }).addTo(map);
      } else {
        circle.setLatLng(latlon);
        circle.setRadius(accuracy);
        circle.setStyle({ color: ring });
      }
    } else if (circle) {
      map.removeLayer(circle);
      delete mapState.circles[key];
    }
  });

  Object.keys(mapState.markers).forEach(function (key) {
    if (seen[key]) return;
    map.removeLayer(mapState.markers[key]);
    delete mapState.markers[key];
    if (mapState.circles[key]) { map.removeLayer(mapState.circles[key]); delete mapState.circles[key]; }
  });
  Object.keys(mapState.tracks).forEach(function (key) {
    if (!seen[key]) dropTrack(key);
  });

  // Fit once, when fixes first appear. Doing it on every later delta would
  // undo the user's panning.
  if (bounds.length && !mapState.fitted) {
    if (bounds.length === 1) map.setView(bounds[0], 16);
    else map.fitBounds(bounds, { maxZoom: 16, padding: [24, 24] });
    mapState.fitted = true;
  }
}

/* ---- tracks ---- */

/* One entity's track: its fixes over the window, read from the recorder
 * and extended by each live fix, drawn as fading bands
 * (dashboard-logic.js, trackBands). The fixes are kept per entity across
 * renders, and the lines are redrawn only when they change. */
function updateTrack(key, entity, hours, color) {
  var track = mapState.tracks[key];
  var now = Date.now();
  if (!track || track.hours !== hours) {
    dropTrack(key);
    track = mapState.tracks[key] = { hours: hours, fixes: [], layers: [], readAt: 0, reading: false, drawn: null };
  }
  if (!track.reading && now - track.readAt > TRACK_REFRESH_MS) readTrack(track, entity, hours);
  // A fix that arrived live, after the history was read.
  var fixedAt = stateValue(entity.room, entity.name, 'fixed_at');
  var latlon = personLatLon(entity);
  var last = track.fixes[track.fixes.length - 1];
  if (latlon && typeof fixedAt === 'number' && (!last || fixedAt * 1000 > last.t)) {
    var accuracy = stateValue(entity.room, entity.name, 'accuracy');
    track.fixes.push({ lat: latlon[0], lon: latlon[1], accuracy: typeof accuracy === 'number' ? accuracy : 0, t: fixedAt * 1000 });
  }
  var signature = color + '|' + track.readAt + '|' + track.fixes.length;
  if (track.drawn !== signature) drawTrack(track, color, now);
  track.drawn = signature;
}

// One series over the last `hours`, as changes. Changes, not buckets:
// averaging lat and lon apart would cut the corners off a route.
function readChanges(entity, aspect, hours) {
  return fetch('/api/history?entity=' + encodeURIComponent(entity.name) + '&aspect=' + aspect +
               '&hours=' + hours + '&changes=1&limit=' + TRACK_LIMIT)
    .then(function (r) { return r.json(); })
    .then(function (data) {
      var points = [];
      (data.series || []).forEach(function (s) { points = points.concat(s.points || []); });
      return points;
    })
    .catch(function () { return []; });
}

function readTrack(track, entity, hours) {
  track.reading = true;
  // `fixed_at` first. Each read ends at the server's own now, and a fix's
  // `fixed_at` lands after its `lat` and `lon`. Read after it, they hold
  // every fix the `fixed_at` reply has.
  readChanges(entity, 'fixed_at', hours).then(function (fixedAt) {
    return Promise.all(FIX_ASPECTS.map(function (aspect) {
      return readChanges(entity, aspect, TRACK_LOOKBACK_HOURS);
    })).then(function (results) {
      var series = { fixed_at: fixedAt };
      FIX_ASPECTS.forEach(function (aspect, i) { series[aspect] = results[i]; });
      return series;
    });
  }).then(function (series) {
    var now = Date.now();
    var fixes = logic.trackFixes(series, now - hours * 3600e3);
    // Live fixes newer than the read stay.
    var newest = fixes.length ? fixes[fixes.length - 1].t : -Infinity;
    track.fixes = fixes.concat(track.fixes.filter(function (f) { return f.t > newest; }));
    track.readAt = now;
    track.reading = false;
    if (mapState.map) mountMapAgain();
  });
}

function drawTrack(track, color, now) {
  track.layers.forEach(function (layer) { mapState.map.removeLayer(layer); });
  var from = now - track.hours * 3600e3;
  var fixes = logic.thinTrack(track.fixes.filter(function (f) { return f.t >= from; }));
  track.layers = logic.trackBands(fixes, from, now, TRACK_BANDS).map(function (band) {
    // Butt caps, because the bands are translucent and a round cap would
    // overlap the next band's and darken each joint.
    return L.polyline(band.latlngs, {
      color: color, opacity: band.opacity, weight: 3, lineCap: 'butt',
      interactive: false, className: 'person-track'
    }).addTo(mapState.map);
  });
}

function dropTrack(key) {
  var track = mapState.tracks[key];
  if (!track) return;
  track.layers.forEach(function (layer) { mapState.map.removeLayer(layer); });
  delete mapState.tracks[key];
}

// The persons and options the map was last mounted with, so a history
// read that lands later can draw without waiting for the next render.
var lastMount = null;

function mountMapAgain() {
  if (lastMount) updateMapMarkers(lastMount.persons, lastMount.opts);
}

// `opts` is the map widget's own fields: `hours` for tracks and
// `colors` by entity name.
export function mountMap(persons, opts) {
  var slot = byId('map-slot');
  if (!slot) return;
  ensureMap();
  slot.appendChild(mapState.container);
  lastMount = { persons: persons, opts: opts || {} };
  updateMapMarkers(persons, lastMount.opts);
  mapState.map.invalidateSize();
}

export function renderMapCard(persons) {
  var noFix = persons.filter(function (e) { return !personLatLon(e); });
  var hint = store.model.tiles ? '' : html`<div class="map-hint">no tile extract configured</div>`;
  var noFixHtml = noFix.length
    ? html`<div class="map-nofix">${noFix.map(function (e) { return e.label; }).join(', ')} &mdash; no fix yet</div>`
    : '';
  return html`<div class="card span-all"><div class="card-label">Map</div>
    <div id="map-slot"></div>${hint}${noFixHtml}</div>`;
}
