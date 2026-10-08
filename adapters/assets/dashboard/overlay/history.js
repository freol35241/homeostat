/* The history overlay: one aspect's record over a chosen window. It also
 * shows the current forecast, either the sources the value is derived
 * from or every forecast the recorder kept, and the commands sent to the
 * aspect. */
import { fetchHistory } from '../api.js';
import { buildChart, buildTimeline, fmtDuration, formatChartClock, formatChartTime, formatChartValue, horizonCaption, issuedCaption } from '../charts.js';
import { html } from '../html.js';
import logic, { titleCase } from '../logic.js';
import { overlayWide, renderOverlayContent, showOverlay } from './panel.js';
import { aspectsFor, descriptorField, findEntity, forecastsOf, liveForecastsOf, overlay, stateValue, store } from '../store.js';

var RANGE_PRESETS = [{ label: '1h', hours: 1 }, { label: '6h', hours: 6 }, { label: '24h', hours: 24 }, { label: '7d', hours: 168 }];

// The number of newest issues a braid draws. Beyond this the lines can no
// longer be told apart and the fetch gets large. The page says how many it
// drew, so the reader knows when the window is not fully shown.
var BRAID_ISSUES = 40;

// detail overlay, range-selectable
export function fetchHistoryRange(entityName, aspect, hours, shape) {
  var rd = overlay.rangeData[hours];
  if (rd && (rd.loaded || rd.loading)) return;
  overlay.rangeData[hours] = { points: [], loaded: false, loading: true };
  fetchHistory(entityName, aspect, hours, shape).then(function (loaded) {
    if (!overlay.shapeSettled && loaded.points.length) {
      // the shape was guessed without a live value; the recorded values decide
      overlay.shapeSettled = true;
      var actual = logic.historyShape(loaded.points[0].value);
      if (actual !== overlay.shape && overlay.open && overlay.aspect === aspect) {
        overlay.shape = actual;
        overlay.rangeData = {};
        fetchHistoryRange(entityName, aspect, hours, actual);
        return;
      }
    }
    overlay.rangeData[hours] = loaded;
    overlay.lastPoints = loaded.points;
    if (overlay.open && overlay.type === 'history') renderOverlayContent();
  });
  fetchCommandRange(entityName, aspect, hours);
}

/* What a computed value is derived from, over the same window. It is
 * fetched only when the reader asks, because contributors are for
 * diagnosis. Each contributor is an ordinary entity whose series the
 * recorder already keeps, so this is the same history call the chart
 * above makes. */
function contributorsOf(entity, aspect) {
  return logic.contributorsFor((store.model && store.model.entities) || [], entity.name, aspect);
}

// A contributor's room, for reading its own `available` aspect.
function contributorRoom(c) {
  var entities = (store.model && store.model.entities) || [];
  for (var i = 0; i < entities.length; i++) {
    if (entities[i].name === c.entity) return entities[i].room;
  }
  return null;
}

function sourceCacheKey(hours, c) {
  return hours + '|' + c.entity + '|' + c.aspect;
}

/* When each source last joined or left the computation. Declared sources
 * say what may contribute, and these events say what did. A contributor
 * the fusion has dropped is then no longer shown as if it still counted
 * (docs/design.md#which-sources-a-computation-actually-used). */
export function fetchSourceEvents(entity, aspect, hours) {
  var ck = hours + '|' + entity.name + '|' + aspect;
  var cached = overlay.sourceEvents[ck];
  if (cached && (cached.loaded || cached.loading)) return;
  overlay.sourceEvents[ck] = { events: [], loaded: false, loading: true };
  fetch('/api/source-events?entity=' + encodeURIComponent(entity.name) +
        '&aspect=' + encodeURIComponent(aspect) + '&hours=' + hours)
    .then(function (r) { return r.json(); })
    .then(function (data) {
      overlay.sourceEvents[ck] = {
        events: (data && data.events) || [], loaded: true, loading: false
      };
      if (overlay.open && overlay.type === 'history') renderOverlayContent();
    })
    .catch(function () {
      overlay.sourceEvents[ck] = { events: [], loaded: true, loading: false };
    });
}

function sourceUsageNow(entity, aspect, hours, contributors) {
  var ck = hours + '|' + entity.name + '|' + aspect;
  var d = overlay.sourceEvents[ck];
  return logic.sourceUsage(d && d.loaded ? d.events : [], contributors);
}

export function fetchSources(entity, aspect, hours) {
  contributorsOf(entity, aspect).forEach(function (c) {
    var ck = sourceCacheKey(hours, c);
    var cached = overlay.sourceData[ck];
    if (cached && (cached.loaded || cached.loading)) return;
    overlay.sourceData[ck] = { points: [], loaded: false, loading: true };
    fetchHistory(c.entity, c.aspect, hours, 'chart').then(function (loaded) {
      overlay.sourceData[ck] = loaded;
      if (overlay.open && overlay.type === 'history') renderOverlayContent();
    });
  });
}

// The contributors the chart should draw now: declared, requested, and
// loaded. One that is still loading is not drawn yet.
function drawnSources(entity, aspect, hours) {
  if (overlay.layer !== 'sources') return null;
  var out = [];
  contributorsOf(entity, aspect).forEach(function (c) {
    var d = overlay.sourceData[sourceCacheKey(hours, c)];
    if (!d || !d.loaded || !d.points.length) return;
    // Includes the contributor's identity and caveat, not just its line.
    // The legend names it, says whether it is still reporting, and shows
    // the caveat that applies to this source rather than to the aspect.
    out.push({
      name: c.name,
      label: c.label,
      entity: c.entity,
      note: c.note,
      points: d.points
    });
  });
  return out.length ? out : null;
}

/* What the house forecast for this window at the time, as opposed to its
 * current forecast: every issue the recorder kept. It is fetched only
 * when the reader asks, because a braid is for analysis. A house with no
 * recorder has no stored issues but can still draw its current
 * forecast. */
export function fetchIssues(entityName, aspect, hours) {
  overlay.issuesLoading = true;
  var url = '/api/forecasts?entity=' + encodeURIComponent(entityName) +
    '&aspect=' + encodeURIComponent(aspect) + '&hours=' + hours + '&limit=' + BRAID_ISSUES;
  fetch(url)
    .then(function (r) { return r.json(); })
    .then(function (data) {
      overlay.issuesLoading = false;
      if (!overlay.open || overlay.aspect !== aspect) return;
      overlay.issues = logic.decodeIssues(data.issues);
      renderOverlayContent();
    })
    .catch(function () {
      overlay.issuesLoading = false;
      overlay.issues = [];
      if (overlay.open) renderOverlayContent();
    });
}

/* The commands sent to this aspect, beside what it did. The recorder
 * stores every cmd envelope in its own series
 * (docs/design.md#history-and-the-recorder), and this reads that record.
 * It is always fetched, whether or not the page thinks the aspect is
 * commandable. Recorded commands are the reliable test, and the rules for
 * who may command what are on the server. The strip renders only when
 * the window holds commands. */
function fetchCommandRange(entityName, aspect, hours) {
  var cd = overlay.cmdData[hours];
  if (cd && (cd.loaded || cd.loading)) return;
  overlay.cmdData[hours] = { points: [], loaded: false, loading: true };
  // Commands are drawn as runs, not a curve, however the aspect's own
  // readings are drawn.
  fetchHistory(entityName, aspect, hours, 'timeline', 'cmd').then(function (loaded) {
    overlay.cmdData[hours] = loaded;
    if (overlay.open && overlay.type === 'history' && overlay.aspect === aspect) renderOverlayContent();
  });
}

export function openHistoryDetail(room, entityName, aspect) {
  var entity = findEntity(room, entityName);
  if (!entity) return;
  overlay.type = 'history';
  overlay.entity = entity;
  overlay.aspect = aspect;
  // The descriptor's kind decides the shape when there is one. An enum
  // coded as integers is drawn as runs, not a curve. Without a
  // descriptor, the live value's type decides. Without a value yet (a
  // fresh page, or a sensor that has not reported), the shape is a guess
  // that the first fetched points correct (fetchHistoryRange).
  var live = stateValue(room, entityName, aspect);
  var field = descriptorField(entity, aspect);
  var described = !!(field && (field.kind === 'boolean' || field.kind === 'enum'));
  overlay.shape = logic.historyShape(live, field);
  overlay.shapeSettled = described || live !== undefined;
  overlay.range = 24;
  overlay.rangeData = {};
  overlay.cmdData = {};
  overlay.layer = '';
  overlay.issues = [];
  overlay.pinned = null;
  overlay.sourcePinned = null;
  overlay.lastPoints = null;
  // Sources belong to this entity, so the layer and the fetched
  // contributor history are reset for the next overlay.
  overlay.sourceData = {};
  overlay.sourceEvents = {};
  fetchHistoryRange(entityName, aspect, 24, overlay.shape);
  showOverlay();
}

/* Which line is which, on demand. Pointing at a source in the legend
 * highlights its line and dims the others, and a tap keeps that until it
 * is tapped again. Lines are told apart by emphasis rather than colour,
 * because a colour per contributor would compete with the accent colour
 * of the computed value (docs/design.md#charts-forecasts-and-sources). */
export function wireSourceLegend(root) {
  var scope = root || document;
  var entries = scope.querySelectorAll('.source-legend span[data-source]');
  if (!entries.length) return;
  // The pin is stored on the overlay, not in this handler. Live state
  // re-renders the panel (a new forecast issue is enough), which discards
  // these nodes and their classes. A new closure would take its highlight
  // from whatever the pointer is over, so the pin could move to another
  // source or disappear while the reader is still reading. The braid's pin
  // is kept in `overlay.pinned` for the same reason.
  var sticky = overlay.sourcePinned || null;
  var marks = scope.querySelectorAll('.chart-contributor, .source-legend span');
  var highlight = function (name) {
    marks.forEach(function (el) {
      var own = el.getAttribute('data-source');
      el.classList.toggle('hi', !!name && own === name);
      el.classList.toggle('dim', !!name && own !== name);
    });
  };
  entries.forEach(function (el) {
    var name = el.getAttribute('data-source');
    el.addEventListener('pointerenter', function () { if (!sticky) highlight(name); });
    el.addEventListener('pointerleave', function () { if (!sticky) highlight(null); });
    el.addEventListener('click', function () {
      sticky = sticky === name ? null : name;
      overlay.sourcePinned = sticky;
      highlight(sticky);
    });
  });
  // Re-apply on every wiring, so a re-render restores the pin.
  highlight(sticky);
}

function statItem(label, value, aspect) {
  return statText(label, value === null ? '—' : formatChartValue(aspect, value));
}

function statText(label, text) {
  return html`<div class="stat-item"><div class="stat-value">${text}</div><div class="stat-label">${label}</div></div>`;
}

function rangeStats(points) {
  var vals = (points || []).map(function (p) { return p.value; }).filter(function (v) { return typeof v === 'number'; });
  if (vals.length === 0) return { latest: null, min: null, max: null, avg: null };
  var sum = 0;
  vals.forEach(function (v) { sum += v; });
  return {
    latest: points[points.length - 1].value,
    min: Math.min.apply(null, vals),
    max: Math.max.apply(null, vals),
    avg: sum / vals.length
  };
}

export function renderHistoryDetailBody() {
  var entity = overlay.entity, aspect = overlay.aspect;
  var multi = aspectsFor(entity).length > 1;
  var label = entity.label + (multi ? ' · ' + titleCase(aspect) : '');

  var chips = RANGE_PRESETS.map(function (rp) {
    return html`<button data-action="range-chip" data-hours="${rp.hours}" class="${overlay.range === rp.hours ? 'active' : ''}">${rp.label}</button>`;
  });
  // One row of chips for what is drawn beside the record: the sources
  // behind the value's past, or the stored forecasts of its future. Both
  // are for the owner, so they are here and not on a tile. A family wants
  // the temperature, not which sensor read low. Only one can be drawn at a
  // time, so they share one control
  // (docs/design.md#charts-forecasts-and-sources). A chip appears only
  // when the house has what it names, so an ordinary sensor's overlay has
  // no such row.
  var layers = [{ id: '', label: 'value' }];
  if (overlay.shape !== 'timeline' && contributorsOf(entity, aspect).length) {
    layers.push({ id: 'sources', label: 'sources' });
  }
  if (forecastsOf(entity, aspect).length) {
    layers.push({ id: 'forecasts', label: 'forecasts' });
  }
  var layerChips = layers.length > 1
    ? html`<div class="seg" style="margin-top:8px;">${layers.map(function (l) {
      return html`<button data-action="chart-layer" data-layer="${l.id}" class="${overlay.layer === l.id ? 'active' : ''}">${l.label}</button>`;
    })}</div>`
    : '';

  var rd = overlay.rangeData[overlay.range];
  var loaded = !!(rd && rd.loaded);
  var points = loaded ? rd.points : (overlay.lastPoints || []);
  var win = rd && rd.window ? rd.window : { from: Date.now() - overlay.range * 3600e3, to: Date.now() };
  var field = descriptorField(entity, aspect);
  // The viewBox height is fixed when the chart is built, so a taller chart
  // needs a rebuild. That is why toggling the width re-renders instead of
  // only swapping a class.
  var wide = overlayWide();
  var chartHtml, statsHtml;
  if (overlay.shape === 'timeline') {
    chartHtml = buildTimeline('detail-chart', points, { height: wide ? 96 : 64, sizeClass: wide ? 'chart-timeline-wide' : 'chart-timeline', aspect: aspect, field: field, window: win });
    var tl = logic.timelineStats(logic.timelineRuns(points, win.from, win.to));
    statsHtml = html`<div class="stat-row">
      ${statText('Now', tl.latest === null ? '—' : logic.formatAspect(aspect, field, tl.latest))}
      ${tl.onMs === null ? '' : statText('On', fmtDuration(tl.onMs) + ' of ' + fmtDuration(win.to - win.from))}
      ${statText('Changes', String(tl.changes))}</div>`;
  } else {
    chartHtml = buildChart('detail-chart', points, {
      height: wide ? 440 : 220, sizeClass: wide ? 'chart-detail-wide' : 'chart-detail',
      aspect: aspect, gridlines: true, area: true, yLabels: true, window: win,
      // Under `forecasts` the current forecast is not drawn. Drawing it
      // over the braid would make one issue stand out for no reason.
      forecast: overlay.layer === 'forecasts' ? null : liveForecastsOf(entity, aspect),
      issues: overlay.layer === 'forecasts' ? overlay.issues : null,
      pinned: overlay.pinned,
      // Contributors are not drawn with the braid. Two sets of grey lines
      // on one chart, one per issue and one per source, would look like
      // one set.
      contributors: overlay.layer === 'forecasts' ? null : drawnSources(entity, aspect, overlay.range)
    });
    var stats = rangeStats(points);
    statsHtml = html`<div class="stat-row">
      ${statItem('Latest', stats.latest, aspect)}${statItem('Min', stats.min, aspect)}
      ${statItem('Max', stats.max, aspect)}${statItem('Avg', stats.avg, aspect)}</div>`;
  }

  // The four stats describe the recorded window the chips select, so a
  // drawn forecast can peak above the MAX shown beside it. That looks like
  // a contradiction unless the horizon states its own extreme.
  // There is one caption per source. Combining two providers into one
  // sentence would describe a horizon neither of them predicted.
  var horizonHtml = '';
  if (overlay.layer !== 'forecasts') {
    var beliefs = forecastsOf(entity, aspect);
    horizonHtml = beliefs.map(function (b) {
      var fresh = logic.forecastFreshness(b.forecast, Date.now());
      var who = beliefs.length > 1 ? b.source + ' · ' : '';
      var issued = issuedCaption(b.forecast);
      // A forecast whose horizon has run out is not drawn here. This note
      // is the only thing that tells "this producer stopped" apart from
      // "this aspect has no forecast". The expired forecast itself can
      // still be read in the braid.
      if (fresh.expired) {
        return html`<div class="chart-note">spent · ${who}ran out ${formatChartTime(b.forecast.to)}${issued ? ' · ' + issued : ''}</div>`;
      }
      var ahead = horizonCaption(aspect, field, b.forecast, Date.now());
      if (!ahead && !issued) return '';
      return html`<div class="chart-note">ahead · ${who}${ahead ? ahead + (issued ? ' · ' : '') : ''}${issued}
        ${fresh.stale ? html`<span class="stale-mark">stale</span>` : ''}</div>`;
    });
  }
  var braidHtml = '';
  if (overlay.layer === 'forecasts') {
    braidHtml = html`<div class="chart-note" id="braid-column"></div>
      <div class="chart-note">${
        overlay.issuesLoading ? 'reading the forecasts\u2026'
          : overlay.issues.length
            ? overlay.issues.length + ' forecast' + (overlay.issues.length === 1 ? '' : 's') +
              (function () {
                // With issues from several providers in one braid, "they
                // disagree" and "it drifted" look the same, so the count
                // says how many sources are mixed in.
                var seen = {};
                overlay.issues.forEach(function (f) { if (f.source) seen[f.source] = 1; });
                var n = Object.keys(seen).length;
                return n > 1 ? ' from ' + n + ' sources' : '';
              })() +
              ' kept for this window' + (overlay.issues.length >= BRAID_ISSUES ? ' (newest ' + BRAID_ISSUES + ')' : '') +
              (overlay.pinned ? ' \u00b7 one pinned \u2014 tap it again to release' : ' \u00b7 tap a line to pin it')
            : 'nothing kept for this window \u2014 the recorder may not subscribe home/forecast/**'
      }</div>`;
  }
  // The legend is also the scrub readout. With no cursor it names the
  // sources, and with one it says what each read at that instant. The
  // lines are all the same grey, so the names are what identify them.
  var legendHtml = '';
  var drawn = overlay.layer === 'forecasts' ? null : drawnSources(entity, aspect, overlay.range);
  if (drawn) {
    var usage = sourceUsageNow(entity, aspect, overlay.range,
                               contributorsOf(entity, aspect));
    legendHtml = html`<div class="chart-note source-legend" id="sources-legend">${
      drawn.map(function (c) {
        // A contributor that has dropped out is still drawn, because
        // where its line stops is useful, but it must not look live. It
        // can be out in two ways: its own device is gone, or the
        // computation chooses not to use it.
        var down = stateValue(contributorRoom(c), c.entity, 'available') === false;
        var use = usage[c.name];
        var excluded = use && use.used === false;
        var since = excluded && use.since
          ? ' since ' + formatChartClock(use.since / 1000)
          : '';
        return html`<span data-source="${c.name}"${c.note ? html` title="${c.note}"` : ''}>
          ${c.label}
          ${down ? ' \u00b7 unavailable' : ''}
          ${excluded ? ' \u00b7 not used' + since : ''}
          ${c.note ? ' \u00b7 ' + c.note : ''}</span>`;
      })}</div>
      <div class="chart-note" id="sources-column"></div>`;
  } else if (overlay.layer === 'sources' && contributorsOf(entity, aspect).length) {
    legendHtml = html`<div class="chart-note">reading the sources\u2026</div>`;
  }
  var body = html`<div class="seg">${chips}</div>${layerChips}
    <div class="${loaded ? '' : 'chart-loading'}">${chartHtml}${statsHtml}${horizonHtml}</div>
    ${legendHtml}${braidHtml}${commandStrip(aspect, field, win)}`;
  return { title: label, body: body };
}

/* The commands sent, under what happened. This is the recorder's cmd
 * series, so an aspect that was never commanded has no strip. */
function commandStrip(aspect, field, win) {
  var cd = overlay.cmdData[overlay.range];
  if (!cd || !cd.loaded || !cd.points.length) return '';
  var runs = logic.timelineRuns(cd.points, win.from, win.to);
  var latest = cd.points[cd.points.length - 1];
  return html`<div class="card-label" style="margin-top:20px;">Commanded</div>
    ${buildTimeline('detail-cmd', cd.points, { height: 28, sizeClass: 'chart-commands', aspect: aspect, field: field, window: win })}
    <div class="chart-note">${runs.length + (runs.length === 1 ? ' command' : ' commands') +
      ' · last ' + logic.formatAspect(aspect, field, latest.value) + ' at ' + formatChartTime(latest.ts)}</div>`;
}
