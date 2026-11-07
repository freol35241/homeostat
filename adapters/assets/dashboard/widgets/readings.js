/* The tile and chart widgets: an entity's readings, now and over time. */
import { ensureHistory, fetchHistory } from '../api.js';
import { buildBars, buildChart, buildTimeline, formatChartClock, horizonCaption, issuedCaption, minMaxCaption } from '../charts.js';
import { html } from '../html.js';
import logic from '../logic.js';
import { descriptorField, liveForecastsOf, localState, scheduleRender, stateValue, store } from '../store.js';

// Signal tiles: one per reading of the entity, or only the named reading.
// The descriptor decides which readings appear and in what order. They are
// the same rows as the entity's sensor card (dashboard-logic.js,
// sensorCardPlan).
export function widgetTiles(entity, aspect) {
  var all = logic.sensorCardPlan(entity, store.state, store.aspects[entity.name]);
  var rows = aspect ? all.filter(function (r) { return r.aspect === aspect; }) : all;
  return html`${rows.map(function (r) {
    var sk = entity.name + '|' + r.aspect;
    var intervalS = logic.intervalOf(entity, r.aspect);
    if (intervalS) return intervalTile(entity, r, all.length > 1, intervalS);
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
    // The reading is named whenever the entity has more than one, so a
    // tile placed with `aspect` still says which reading it shows.
    var tileLabel = entity.label + (all.length > 1 ? ' \u00b7 ' + r.label : '');
    return html`<div class="card tile clickable" data-action="history-detail" data-room="${entity.room}" data-entity="${entity.name}" data-aspect="${r.aspect}">
      <div class="card-label" title="${entity.label}">${tileLabel.toUpperCase()}</div>
      <div class="big${r.stale ? ' stale' : ''}">${r.display}</div>${spark}
      <div class="caption">${caption}${aged ? html`<span class="stale-mark">stale</span>` : ''}</div></div>`;
  })}`;
}

// The name of one interval in a caption: "latest hour" reads better than
// "latest 1h".
function intervalName(intervalS) {
  if (intervalS === 3600) return 'hour';
  if (intervalS === 86400) return 'day';
  return '';
}

/* A tile for an amount per interval, such as hourly rain. The big number
 * is the total over the last 24 h, because "has it rained?" is what a
 * glance asks, and the latest hour is 0 most of the time. The 24 h end
 * where the newest recorded interval ends, which is before now, and the
 * caption says when. The latest interval is the small line. */
function intervalTile(entity, r, named, intervalS) {
  var sk = entity.name + '|' + r.aspect;
  ensureHistory(entity.name, r.aspect, intervalS);
  var s = localState.sparklines[sk];
  var loaded = !!(s && s.loaded);
  var field = descriptorField(entity, r.aspect);
  var now = Date.now();
  var day = { from: now - 864e5, to: now };
  // One forecast has room on a tile, as for a line. With several, the
  // overlay tells them apart.
  var forecast = liveForecastsOf(entity, r.aspect);
  var spark = loaded
    ? buildBars('tile-' + sk, logic.intervalBars(s.points, intervalS, day, forecast.length === 1 ? forecast[0].forecast : null),
      { height: 36, sizeClass: 'chart-tile', aspect: r.aspect, window: day })
    : '';
  var recent = loaded ? logic.recentTotal(s.points, intervalS, now) : null;
  var big = recent ? logic.formatAspect(r.aspect, field, recent.total) : r.display;
  var caption = recent
    ? ['last 24 h to ' + formatChartClock(recent.end),
      'latest ' + (intervalName(intervalS) ? intervalName(intervalS) + ' ' : '') + logic.formatAspect(r.aspect, field, recent.latest),
      recent.covered < recent.expected ? recent.covered + ' of ' + recent.expected + ' recorded' : ''
    ].filter(Boolean).join(' \u00b7 ')
    : '';
  var tileLabel = entity.label + (named ? ' \u00b7 ' + r.label : '');
  return html`<div class="card tile clickable" data-action="history-detail" data-room="${entity.room}" data-entity="${entity.name}" data-aspect="${r.aspect}">
    <div class="card-label" title="${entity.label}">${tileLabel.toUpperCase()}</div>
    <div class="big${r.stale ? ' stale' : ''}">${big}</div>${spark}
    <div class="caption">${caption}${recent && recent.stale ? html`<span class="stale-mark">stale</span>` : ''}</div></div>`;
}

// One aspect's history over a window, as a card.
export function widgetChart(entity, aspect, hours) {
  var key = entity.name + '|' + aspect + '|' + hours;
  var field = descriptorField(entity, aspect);
  // The same rule as the detail overlay. A described enum or boolean is
  // drawn as runs, so the card draws a timeline and asks the recorder for
  // changes.
  var intervalS = logic.intervalOf(entity, aspect);
  var shape = logic.historyShape(stateValue(entity.room, entity.name, aspect), field, intervalS);
  var s = localState.charts[key];
  if (!s) {
    localState.charts[key] = { points: [], loaded: false, loading: true };
    fetchHistory(entity.name, aspect, hours, shape, null, intervalS).then(function (loaded) {
      localState.charts[key] = loaded;
      scheduleRender();
    });
    s = localState.charts[key];
  }
  var value = stateValue(entity.room, entity.name, aspect);
  var beliefs = shape === 'timeline' ? [] : liveForecastsOf(entity, aspect);
  var bars = shape === 'bars' && s.loaded
    ? logic.intervalBars(s.points, intervalS, s.window, beliefs.length === 1 ? beliefs[0].forecast : null)
    : [];
  var chart = !s.loaded ? ''
    : shape === 'timeline'
      ? buildTimeline('chart-' + key, s.points, { height: 64, sizeClass: 'chart-timeline', aspect: aspect, field: field, window: s.window })
      : shape === 'bars'
        ? buildBars('chart-' + key, bars, { height: 120, sizeClass: 'chart-card', aspect: aspect, gridlines: true, yLabels: true, window: s.window })
        : buildChart('chart-' + key, s.points, { height: 120, sizeClass: 'chart-card', aspect: aspect, area: true, gridlines: true, yLabels: true, window: s.window, forecast: beliefs });
  // An amount per interval reads as the window's recorded total, which is
  // what the bars add up to. Its latest value is one interval's.
  var big = shape === 'bars'
    ? logic.formatAspect(aspect, field, bars.reduce(function (a, b) { return a + b.observed; }, 0))
    : logic.formatAspect(aspect, field, value);
  // The same rule as a tile: a drawn forecast says when it was issued. One
  // line for one source. With several, the overlay names each.
  var beliefCaption = beliefs.length === 1
    ? html`<div class="caption">${issuedCaption(beliefs[0].forecast)}
      ${logic.forecastFreshness(beliefs[0].forecast, Date.now()).stale
        ? html`<span class="stale-mark">stale</span>` : ''}</div>`
    : '';
  return html`<div class="card tile clickable span-all" data-action="history-detail" data-room="${entity.room}" data-entity="${entity.name}" data-aspect="${aspect}">
    <div class="card-label">${(entity.label + ' · ' + ((field && field.label) || aspect)).toUpperCase()} <span class="faint">${hours >= 48 ? Math.round(hours / 24) + 'd' : hours + 'h'}</span></div>
    <div class="big">${big}</div>${chart}${beliefCaption}</div>`;
}
