/* The map widget. The map is a view over every person entity, not a
 * per-entity row (see docs/design.md#map-and-people). Leaflet's map object
 * is stateful (pan/zoom, tile cache) and must not be recreated on every
 * state-delta render, so its container is a detached DOM node kept in
 * mapState and moved into a fresh slot each render, rather than rebuilt
 * from an HTML string like the rest of the view. */
import { byId, html } from '../html.js';
import { stateValue, store } from '../store.js';
import { relTime } from './entity.js';

var mapState = { container: null, map: null, tileLayer: null, markers: {}, circles: {}, fitted: false };

function personLatLon(entity) {
  var lat = stateValue(entity.room, entity.name, 'lat');
  var lon = stateValue(entity.room, entity.name, 'lon');
  if (typeof lat !== 'number' || typeof lon !== 'number') return null;
  return [lat, lon];
}

function mapThemeName() {
  return (window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches) ? 'dark' : 'light';
}

function personDivIcon(entity) {
  return L.divIcon({
    className: 'person-marker',
    // Leaflet takes a string here (and a popup's content below), not markup
    html: String(html`<div class="person-marker-dot"></div><div class="person-marker-label">${entity.label}</div>`),
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
  // No default setView: a center/zoom is set only once real fixes exist
  // (updateMapMarkers), so no coordinate is ever hardcoded. Layers added
  // before that point just wait for it (Leaflet defers via whenReady).
  mapState.map = L.map(mapState.container);
  if (store.model.tiles) {
    mapState.tileLayer = protomapsL.leafletLayer({ url: '/tiles.pmtiles', theme: mapThemeName() }).addTo(mapState.map);
  }
}

function updateMapMarkers(persons) {
  var map = mapState.map;
  var bounds = [];
  var seen = {};
  persons.forEach(function (e) {
    var key = e.room + '/' + e.name;
    seen[key] = true;
    var latlon = personLatLon(e);
    if (!latlon) {
      if (mapState.markers[key]) { map.removeLayer(mapState.markers[key]); delete mapState.markers[key]; }
      if (mapState.circles[key]) { map.removeLayer(mapState.circles[key]); delete mapState.circles[key]; }
      return;
    }
    bounds.push(latlon);
    var marker = mapState.markers[key];
    if (!marker) {
      marker = L.marker(latlon, { icon: personDivIcon(e) }).addTo(map);
      mapState.markers[key] = marker;
    } else {
      marker.setLatLng(latlon);
    }
    marker.bindPopup(personPopupHtml(e));

    var accuracy = stateValue(e.room, e.name, 'accuracy');
    var circle = mapState.circles[key];
    if (typeof accuracy === 'number') {
      if (!circle) {
        mapState.circles[key] = L.circle(latlon, { radius: accuracy, color: 'var(--accent)', weight: 1, fillOpacity: .1 }).addTo(map);
      } else {
        circle.setLatLng(latlon);
        circle.setRadius(accuracy);
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

  // Fit once, when fixes first appear — repeating this on every later
  // delta would fight anyone panning the map.
  if (bounds.length && !mapState.fitted) {
    if (bounds.length === 1) map.setView(bounds[0], 16);
    else map.fitBounds(bounds, { maxZoom: 16, padding: [24, 24] });
    mapState.fitted = true;
  }
}

export function mountMap(persons) {
  var slot = byId('map-slot');
  if (!slot) return;
  ensureMap();
  slot.appendChild(mapState.container);
  updateMapMarkers(persons);
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
