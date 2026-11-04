/* The map widget. The map shows every person, and every tracker no person
 * lists, rather than being a per-entity row (see
 * docs/design.md#map-and-people). With `hours` it also draws each one's
 * track over that window.
 *
 * MapLibre draws it with WebGL, from the house's own PMTiles extract in the
 * Protomaps basemap style. The library is loaded on first use, and the
 * extract's reader and style only when the house has an extract. The map
 * object has state (pan/zoom, tile cache) and must not be recreated on
 * every state-delta render. Its container is therefore a detached DOM node
 * kept in mapState and moved into a new slot on each render, instead of
 * being rebuilt from an HTML string like the rest of the view. */
import { byId, html } from '../html.js';
import logic from '../logic.js';
import { stateValue, store } from '../store.js';
import { relTime } from './entity.js';

// The vendored map files (adapters/assets/map/), found from this module's
// own address so the page works under any path prefix.
var MAP_ASSETS = new URL('../../map/', import.meta.url).href;

var mapState = {
  container: null, map: null, maplibre: null, basemaps: null, loading: null, unavailable: null,
  markers: {}, tracks: {}, accuracy: { type: 'FeatureCollection', features: [] }, fitted: false
};

// How often a track's history is read again. The window's start moves
// with the clock, and so does the fade.
var TRACK_REFRESH_MS = 5 * 60e3;
// The number of steps the fade along a track is worked out at.
var TRACK_STEPS = 32;
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
// The zoom a single fix opens at, and the closest the map goes. Vector
// tiles stay sharp past the extract's last level (z15), so the map can go
// in to house level.
var FIX_ZOOM = 16;
var MAX_ZOOM = 19;

var ACCURACY = 'accuracy';

function personLatLon(entity) {
  var lat = stateValue(entity.room, entity.name, 'lat');
  var lon = stateValue(entity.room, entity.name, 'lon');
  if (typeof lat !== 'number' || typeof lon !== 'number') return null;
  return [lat, lon];
}

var darkScheme = window.matchMedia ? window.matchMedia('(prefers-color-scheme: dark)') : null;

function themeName() {
  return darkScheme && darkScheme.matches ? 'dark' : 'light';
}

function cssVar(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

// The accent colour as the stylesheet sets it for the current theme. The
// map draws in its own canvas, where a CSS variable does not resolve, and
// a track's fade needs the colour's parts.
function accentColor() {
  return logic.hexColor(cssVar('--accent')) || '#808080';
}

// MapLibre 6 needs WebGL2.
function webgl2Available() {
  try {
    return !!document.createElement('canvas').getContext('webgl2');
  } catch (e) {
    return false;
  }
}

// pmtiles ships as a classic script that defines `pmtiles`. Its module
// build imports a decompressor by package name, which a page cannot
// resolve.
function loadScript(src) {
  return new Promise(function (resolve, reject) {
    var script = document.createElement('script');
    script.src = src;
    script.onload = resolve;
    script.onerror = function () { reject(new Error('could not load ' + src)); };
    document.head.appendChild(script);
  });
}

// The label language: the browser's first one the basemap names in Latin
// script, the only glyphs vendored (adapters/assets/README.md), else
// English.
function labelLanguage(basemaps) {
  var latin = {};
  (basemaps.language_script_pairs || []).forEach(function (p) { if (p.script === 'Latin') latin[p.lang] = true; });
  var wanted = (navigator.languages && navigator.languages.length ? navigator.languages : [navigator.language || 'en'])
    .map(function (l) { return String(l).slice(0, 2).toLowerCase(); });
  for (var i = 0; i < wanted.length; i++) if (latin[wanted[i]]) return wanted[i];
  return 'en';
}

// The style for the current theme: the Protomaps basemap over the
// house's extract, or a plain background without one.
function mapStyle() {
  var background = { id: 'background', type: 'background', paint: { 'background-color': cssVar('--bg') || '#ffffff' } };
  var basemaps = mapState.basemaps;
  if (!basemaps) return { version: 8, sources: {}, layers: [background] };
  var flavor = themeName();
  return {
    version: 8,
    glyphs: MAP_ASSETS + 'fonts/{fontstack}/{range}.pbf',
    sprite: MAP_ASSETS + 'sprites/' + flavor,
    sources: {
      protomaps: {
        type: 'vector',
        url: 'pmtiles://' + new URL('/tiles.pmtiles', location.href).href,
        // OpenStreetMap's licence asks for this credit wherever its data
        // is shown, and Protomaps asks for theirs.
        attribution: '<a href="https://protomaps.com">Protomaps</a> © ' +
          '<a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a>'
      }
    },
    layers: basemaps.layers('protomaps', basemaps.namedFlavor(flavor), { lang: labelLanguage(basemaps) })
  };
}

// The map, once loaded, or null with `mapState.unavailable` saying why.
function ensureMap() {
  if (mapState.loading) return mapState.loading;
  if (!webgl2Available()) {
    mapState.unavailable = 'This browser cannot draw the map: WebGL2 is unavailable.';
    mapState.loading = Promise.resolve(null);
    return mapState.loading;
  }
  var tiles = !!store.model.tiles;
  mapState.loading = Promise.all([
    import(MAP_ASSETS + 'maplibre-gl.mjs'),
    tiles ? import(MAP_ASSETS + 'basemaps.mjs') : null,
    tiles ? loadScript(MAP_ASSETS + 'pmtiles.js') : null
  ]).then(function (loaded) {
    var maplibre = mapState.maplibre = loaded[0];
    mapState.basemaps = loaded[1];
    if (tiles) maplibre.addProtocol('pmtiles', new window.pmtiles.Protocol().tile);
    // The note a failed earlier load left in the container.
    mapState.container.classList.remove('map-unavailable');
    mapState.container.textContent = '';
    // The centre is a placeholder until the first fixes set it
    // (updateMapMarkers).
    var map = new maplibre.Map({
      container: mapState.container, style: mapStyle(), center: [0, 0], zoom: 1,
      maxZoom: MAX_ZOOM, attributionControl: { compact: true }
    });
    map.addControl(new maplibre.NavigationControl({ showCompass: false }), 'top-left');
    // A label in a script whose glyphs are not vendored, or a tile the
    // extract does not cover, fails one request and the map draws on
    // without it. MapLibre logs every error unless the page listens, and
    // the page's own console errors are kept for its own faults.
    map.on('error', function (e) { console.warn('map: ' + ((e.error && e.error.message) || e.type)); });
    // A style change drops every source and layer the page added, so they
    // are added again from what mapState holds.
    // `diff: false` reloads the style whole, which fires `style.load`. A
    // diffed change would drop the page's layers without it.
    map.on('style.load', addOverlays);
    if (darkScheme && darkScheme.addEventListener) {
      darkScheme.addEventListener('change', function () { map.setStyle(mapStyle(), { diff: false }); });
    }
    mapState.map = map;
    return map;
  }).catch(function (err) {
    // A file that did not load, such as over a dropped connection. The
    // next render tries again.
    console.warn('map: ' + err);
    mapState.unavailable = 'The map could not be loaded.';
    mapState.loading = null;
    return null;
  });
  return mapState.loading;
}

/* ---- what the page draws over the basemap ---- */

function addOverlays() {
  var map = mapState.map;
  if (!map.getSource(ACCURACY)) map.addSource(ACCURACY, { type: 'geojson', data: mapState.accuracy });
  if (!map.getLayer(ACCURACY + '-fill')) {
    map.addLayer({ id: ACCURACY + '-fill', type: 'fill', source: ACCURACY,
      paint: { 'fill-color': ['get', 'color'], 'fill-opacity': 0.1 } });
    map.addLayer({ id: ACCURACY + '-line', type: 'line', source: ACCURACY,
      paint: { 'line-color': ['get', 'color'], 'line-width': 1 } });
  }
  Object.keys(mapState.tracks).forEach(function (key) { applyTrack(key, mapState.tracks[key]); });
  // A theme switch also changes the accent, so the lines and circles are
  // drawn again in the new one.
  mountMapAgain();
}

function setAccuracy(features) {
  mapState.accuracy = { type: 'FeatureCollection', features: features };
  var source = mapState.map.getSource(ACCURACY);
  if (source) source.setData(mapState.accuracy);
}

function markerElement(entity) {
  var el = document.createElement('div');
  el.className = 'person-marker';
  el.innerHTML = String(html`<div class="person-marker-dot"></div><div class="person-marker-label">${entity.label}</div>`);
  return el;
}

function personPopupHtml(entity) {
  var battery = stateValue(entity.room, entity.name, 'battery');
  var fixedAt = stateValue(entity.room, entity.name, 'fixed_at');
  var parts = [];
  if (typeof battery === 'number') parts.push('battery ' + Math.round(battery) + '%');
  if (typeof fixedAt === 'number') parts.push(relTime(fixedAt * 1000));
  return String(html`<b>${entity.label}</b>${parts.length ? html`<br>${parts.join(' · ')}` : ''}`);
}

function updateMapMarkers(persons, opts) {
  var map = mapState.map;
  var maplibre = mapState.maplibre;
  var bounds = [];
  var seen = {};
  var accuracy = [];
  persons.forEach(function (e) {
    var key = e.room + '/' + e.name;
    seen[key] = true;
    var color = logic.hexColor((opts.colors || {})[e.name]);
    var latlon = personLatLon(e);
    if (!opts.hours) dropTrack(key);
    // An entity that has never had a position has no track to read.
    else if (latlon) updateTrack(key, e, opts.hours, color || accentColor());
    if (!latlon) {
      if (mapState.markers[key]) { mapState.markers[key].remove(); delete mapState.markers[key]; }
      return;
    }
    bounds.push([latlon[1], latlon[0]]);
    var lngLat = [latlon[1], latlon[0]];
    var marker = mapState.markers[key];
    if (!marker) {
      // A marker needs its position before it joins the map.
      marker = new maplibre.Marker({ element: markerElement(e) })
        .setLngLat(lngLat)
        .setPopup(new maplibre.Popup({ offset: 12, closeButton: false }))
        .addTo(map);
      mapState.markers[key] = marker;
    }
    marker.setLngLat(lngLat);
    marker.getPopup().setHTML(personPopupHtml(e));
    // One map serves every view, and two views can colour it apart.
    marker.getElement().querySelector('.person-marker-dot').style.background = color || '';

    var radius = stateValue(e.room, e.name, 'accuracy');
    if (typeof radius === 'number' && radius > 0) {
      accuracy.push({
        type: 'Feature',
        properties: { color: color || accentColor() },
        geometry: { type: 'Polygon', coordinates: [logic.circlePolygon(latlon[0], latlon[1], radius, 48)] }
      });
    }
  });
  setAccuracy(accuracy);

  Object.keys(mapState.markers).forEach(function (key) {
    if (seen[key]) return;
    mapState.markers[key].remove();
    delete mapState.markers[key];
  });
  Object.keys(mapState.tracks).forEach(function (key) {
    if (!seen[key]) dropTrack(key);
  });

  // Fit once, when fixes first appear. Doing it on every later delta would
  // undo the user's panning.
  if (bounds.length && !mapState.fitted) {
    if (bounds.length === 1) {
      map.jumpTo({ center: bounds[0], zoom: FIX_ZOOM });
    } else {
      var box = new maplibre.LngLatBounds(bounds[0], bounds[0]);
      bounds.forEach(function (b) { box.extend(b); });
      map.fitBounds(box, { maxZoom: FIX_ZOOM, padding: 24, duration: 0 });
    }
    mapState.fitted = true;
  }
}

/* ---- tracks ---- */

/* One entity's track: its fixes over the window, read from the recorder
 * and extended by each live fix, drawn as one line that fades with age
 * (dashboard-logic.js, trackGradient). The fixes are kept per entity
 * across renders, and the line is redrawn only when they change. */
function updateTrack(key, entity, hours, color) {
  var track = mapState.tracks[key];
  var now = Date.now();
  if (!track || track.hours !== hours) {
    dropTrack(key);
    track = mapState.tracks[key] = { hours: hours, fixes: [], line: null, readAt: 0, reading: false, drawn: null };
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
  if (track.drawn !== signature) {
    drawTrack(key, track, color, now);
    track.drawn = signature;
  }
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

function drawTrack(key, track, color, now) {
  var from = now - track.hours * 3600e3;
  var fixes = logic.thinTrack(track.fixes.filter(function (f) { return f.t >= from; }));
  var gradient = logic.trackGradient(fixes, from, now, TRACK_STEPS);
  track.line = null;
  if (gradient) {
    var paint = ['interpolate', ['linear'], ['line-progress']];
    gradient.stops.forEach(function (s) { paint.push(s[0], logic.hexRgba(color, s[1])); });
    track.line = {
      data: { type: 'Feature', properties: {}, geometry: { type: 'LineString', coordinates: gradient.coordinates } },
      paint: paint
    };
  }
  applyTrack(key, track);
}

// Puts a track's line on the map, or takes it off when it has none. Until
// the style has loaded there is nowhere to put it; `addOverlays` does it
// then.
function applyTrack(key, track) {
  var map = mapState.map;
  var id = 'track:' + key;
  if (!map.getSource(ACCURACY)) return;
  if (!track.line) {
    if (map.getLayer(id)) map.removeLayer(id);
    if (map.getSource(id)) map.removeSource(id);
    return;
  }
  var source = map.getSource(id);
  if (source) source.setData(track.line.data);
  // `lineMetrics` gives the line its progress, which the gradient runs on.
  else map.addSource(id, { type: 'geojson', lineMetrics: true, data: track.line.data });
  if (map.getLayer(id)) {
    map.setPaintProperty(id, 'line-gradient', track.line.paint);
  } else {
    // Under the accuracy circles, so a track never hides where someone is.
    map.addLayer({
      id: id, type: 'line', source: id,
      layout: { 'line-cap': 'round', 'line-join': 'round' },
      paint: { 'line-width': 3, 'line-gradient': track.line.paint }
    }, ACCURACY + '-fill');
  }
}

function dropTrack(key) {
  if (!mapState.tracks[key]) return;
  delete mapState.tracks[key];
  var map = mapState.map;
  var id = 'track:' + key;
  if (map.getLayer(id)) map.removeLayer(id);
  if (map.getSource(id)) map.removeSource(id);
}

// The persons and options the map was last mounted with, so a history
// read or the library that lands later can draw without waiting for the
// next render.
var lastMount = null;

function mountMapAgain() {
  if (lastMount) updateMapMarkers(lastMount.persons, lastMount.opts);
}

// `opts` is the map widget's own fields: `hours` for tracks and
// `colors` by entity name.
export function mountMap(persons, opts) {
  var slot = byId('map-slot');
  if (!slot) return;
  if (!mapState.container) {
    mapState.container = document.createElement('div');
    mapState.container.id = 'person-map';
    mapState.container.className = 'map-wrap';
  }
  slot.appendChild(mapState.container);
  lastMount = { persons: persons, opts: opts || {} };
  if (mapState.map) {
    mapState.map.resize();
    mountMapAgain();
    return;
  }
  ensureMap().then(function (map) {
    if (!map) {
      mapState.container.classList.add('map-unavailable');
      mapState.container.textContent = mapState.unavailable;
      return;
    }
    map.resize();
    mountMapAgain();
  });
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
