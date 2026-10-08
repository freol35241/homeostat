/* The tile and chart widgets: an entity's readings, now and over time. */
import { ensureHistory, fetchHistory } from '../api.js';
import { buildChart, buildTimeline, horizonCaption, issuedCaption, minMaxCaption } from '../charts.js';
import { html } from '../html.js';
import logic from '../logic.js';
import { descriptorField, liveForecastsOf, localState, scheduleRender, stateValue, store } from '../store.js';

// Signal tiles: one per reading of the entity, or the one reading named.
// Which readings, in what order, is the descriptor's say — the same rows
// as the entity's sensor card (dashboard-logic.js, sensorCardPlan).
export function widgetTiles(entity, aspect) {
  var rows = logic.sensorCardPlan(entity, store.state, store.aspects[entity.name]);
  if (aspect) rows = rows.filter(function (r) { return r.aspect === aspect; });
  return html`${rows.map(function (r) {
    var sk = entity.name + '|' + r.aspect;
    ensureHistory(entity.name, r.aspect);
    var s = localState.sparklines[sk];
    var forecast = liveForecastsOf(entity, r.aspect);
    var spark = s && s.loaded ? buildChart('tile-' + sk, s.points, { height: 36, sizeClass: 'chart-tile', aspect: r.aspect, area: true, window: s.window, forecast: forecast }) : '';
    // A horizon says where a reading is going; today's range says where
    // it has been. The forecast wins the line when there is one, because
    // a number you can still act on beats one you cannot. A tile is
    // family-tier and has room for ONE claim, so with several sources it
    // falls back to the range rather than picking a provider to believe
    // — comparing them is owner work and lives in the overlay.
    // A belief carries when it was said, always; a stale one is marked,
    // because a caption the reader has to do arithmetic on is not a
    // caption a phone can be read from in three seconds.
    var ahead = forecast.length === 1
      ? horizonCaption(r.aspect, descriptorField(entity, r.aspect), forecast[0].forecast, Date.now())
      : '';
    var aged = forecast.length === 1 && logic.forecastFreshness(forecast[0].forecast, Date.now()).stale;
    var caption = forecast.length === 1
      ? [ahead, issuedCaption(forecast[0].forecast)].filter(Boolean).join(' \u00b7 ')
      : '';
    // With no belief to draw — or several, which a tile has no room to
    // tell apart — the caption is today's range.
    if (!caption && s && s.loaded) caption = minMaxCaption(s.points);
    // The separator is the character, not the entity: the label is text,
    // escaped whole, so "&middot;" here would print as itself.
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
  // The same rule as the detail overlay: a described enum or boolean is
  // runs, so the card draws a timeline and asks the recorder for changes.
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
  // The same rule as a tile: a drawn belief says when it was said. One
  // line for one source; with several, the overlay names each.
  var beliefCaption = beliefs.length === 1
    ? html`<div class="caption">${issuedCaption(beliefs[0].forecast)}
      ${logic.forecastFreshness(beliefs[0].forecast, Date.now()).stale
        ? html`<span class="stale-mark">stale</span>` : ''}</div>`
    : '';
  return html`<div class="card tile clickable span-all" data-action="history-detail" data-room="${entity.room}" data-entity="${entity.name}" data-aspect="${aspect}">
    <div class="card-label">${(entity.label + ' · ' + ((field && field.label) || aspect)).toUpperCase()} <span class="faint">${hours >= 48 ? Math.round(hours / 24) + 'd' : hours + 'h'}</span></div>
    <div class="big">${logic.formatAspect(aspect, field, value)}</div>${chart}${beliefCaption}</div>`;
}
