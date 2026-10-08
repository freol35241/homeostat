/* The tile and chart widgets: an entity's readings, now and over time. */
import { ensureHistory, fetchHistory } from '../api.js';
import { buildChart, buildTimeline, horizonCaption, issuedCaption, minMaxCaption } from '../charts.js';
import { html } from '../html.js';
import logic from '../logic.js';
import { descriptorField, liveForecastsOf, localState, scheduleRender, stateValue, store } from '../store.js';

// Signal tiles: one per reading of the entity, or only the named reading.
// The descriptor decides which readings appear and in what order. They are
// the same rows as the entity's sensor card (dashboard-logic.js,
// sensorCardPlan).
export function widgetTiles(entity, aspect) {
  var rows = logic.sensorCardPlan(entity, store.state, store.aspects[entity.name]);
  if (aspect) rows = rows.filter(function (r) { return r.aspect === aspect; });
  return html`${rows.map(function (r) {
    var sk = entity.name + '|' + r.aspect;
    ensureHistory(entity.name, r.aspect);
    var s = localState.sparklines[sk];
    var forecast = liveForecastsOf(entity, r.aspect);
    var spark = s && s.loaded ? buildChart('tile-' + sk, s.points, { height: 36, sizeClass: 'chart-tile', aspect: r.aspect, area: true, window: s.window, forecast: forecast }) : '';
    // A horizon says where a reading is going, and today's range says
    // where it has been. When there is a forecast it takes the line,
    // because the reader can still act on it. A tile is for the family and
    // has room for one forecast. With several sources it falls back to the
    // range instead of picking one provider. Comparing them is for the
    // owner, in the overlay.
    // A forecast caption always says when it was issued, and a stale one
    // is marked. The reader should not have to work out its age from a
    // phone in a few seconds.
    var ahead = forecast.length === 1
      ? horizonCaption(r.aspect, descriptorField(entity, r.aspect), forecast[0].forecast, Date.now())
      : '';
    var aged = forecast.length === 1 && logic.forecastFreshness(forecast[0].forecast, Date.now()).stale;
    var caption = forecast.length === 1
      ? [ahead, issuedCaption(forecast[0].forecast)].filter(Boolean).join(' \u00b7 ')
      : '';
    // With no forecast to draw, or several that a tile has no room to tell
    // apart, the caption is today's range.
    if (!caption && s && s.loaded) caption = minMaxCaption(s.points);
    // The separator is the character, not the HTML entity. The label is
    // text and is escaped as a whole, so "&middot;" would print as-is.
    var tileLabel = entity.label + (rows.length > 1 ? ' \u00b7 ' + r.label : '');
    return html`<div class="card tile clickable" data-action="history-detail" data-room="${entity.room}" data-entity="${entity.name}" data-aspect="${r.aspect}">
      <div class="card-label" title="${entity.label}">${tileLabel.toUpperCase()}</div>
      <div class="big${r.stale ? ' stale' : ''}">${r.display}</div>${spark}
      <div class="caption">${caption}${aged ? html`<span class="stale-mark">stale</span>` : ''}</div></div>`;
  })}`;
}

// One aspect's history over a window, as a card.
export function widgetChart(entity, aspect, hours) {
  var key = entity.name + '|' + aspect + '|' + hours;
  var field = descriptorField(entity, aspect);
  // The same rule as the detail overlay. A described enum or boolean is
  // drawn as runs, so the card draws a timeline and asks the recorder for
  // changes.
  var shape = logic.historyShape(stateValue(entity.room, entity.name, aspect), field);
  var s = localState.charts[key];
  if (!s) {
    localState.charts[key] = { points: [], loaded: false, loading: true };
    fetchHistory(entity.name, aspect, hours, shape).then(function (loaded) {
      localState.charts[key] = loaded;
      scheduleRender();
    });
    s = localState.charts[key];
  }
  var value = stateValue(entity.room, entity.name, aspect);
  var beliefs = shape === 'timeline' ? [] : liveForecastsOf(entity, aspect);
  var chart = !s.loaded ? ''
    : shape === 'timeline'
      ? buildTimeline('chart-' + key, s.points, { height: 64, sizeClass: 'chart-timeline', aspect: aspect, field: field, window: s.window })
      : buildChart('chart-' + key, s.points, { height: 120, sizeClass: 'chart-card', aspect: aspect, area: true, gridlines: true, yLabels: true, window: s.window, forecast: beliefs });
  // The same rule as a tile: a drawn forecast says when it was issued. One
  // line for one source. With several, the overlay names each.
  var beliefCaption = beliefs.length === 1
    ? html`<div class="caption">${issuedCaption(beliefs[0].forecast)}
      ${logic.forecastFreshness(beliefs[0].forecast, Date.now()).stale
        ? html`<span class="stale-mark">stale</span>` : ''}</div>`
    : '';
  return html`<div class="card tile clickable span-all" data-action="history-detail" data-room="${entity.room}" data-entity="${entity.name}" data-aspect="${aspect}">
    <div class="card-label">${(entity.label + ' · ' + ((field && field.label) || aspect)).toUpperCase()} <span class="faint">${hours >= 48 ? Math.round(hours / 24) + 'd' : hours + 'h'}</span></div>
    <div class="big">${logic.formatAspect(aspect, field, value)}</div>${chart}${beliefCaption}</div>`;
}
