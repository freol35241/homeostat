/* The history overlay: one aspect's record over a chosen window, with its
 * current belief, the sources it is derived from or every forecast the
 * recorder kept, and what was commanded of it. */
import { fetchHistory } from '../api.js';
import { buildChart, buildTimeline, fmtDuration, formatChartClock, formatChartTime, formatChartValue, horizonCaption, issuedCaption } from '../charts.js';
import { html } from '../html.js';
import logic, { titleCase } from '../logic.js';
import { overlayWide, renderOverlayContent, showOverlay } from './panel.js';
import { aspectsFor, descriptorField, findEntity, forecastsOf, liveForecastsOf, overlay, stateValue, store } from '../store.js';

var RANGE_PRESETS = [{ label: '1h', hours: 1 }, { label: '6h', hours: 6 }, { label: '24h', hours: 24 }, { label: '7d', hours: 168 }];

// The newest issues a braid draws. Past this the lines stop being
// separable and the fetch stops being small; the page says how many it
// drew rather than letting the window quietly mean different things.
var BRAID_ISSUES = 40;

// detail overlay, range-selectable
export function fetchHistoryRange(entityName, aspect, hours, shape) {
  var rd = overlay.rangeData[hours];
  if (rd && (rd.loaded || rd.loading)) return;
  overlay.rangeData[hours] = { points: [], loaded: false, loading: true };
  fetchHistory(entityName, aspect, hours, shape).then(function (loaded) {
    if (!overlay.shapeSettled && loaded.points.length) {
      // guessed without a live value: the recorded values have the say
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

/* What a computed value is derived FROM, over the same window. Fetched
 * only when the reader asks: contributors are diagnosis, and every one is
 * an ordinary entity whose series the recorder already keeps, so this is
 * the same history call the chart above already makes. */
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

/* When each source last went in or out of the computation. Declared
 * sources say what MAY contribute; these say what did, so a contributor
 * the fusion has dropped stops reading as if it were still voting
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

// The contributors the chart should draw right now: declared, asked for,
// and arrived. One still loading simply is not drawn yet.
function drawnSources(entity, aspect, hours) {
  if (overlay.layer !== 'sources') return null;
  var out = [];
  contributorsOf(entity, aspect).forEach(function (c) {
    var d = overlay.sourceData[sourceCacheKey(hours, c)];
    if (!d || !d.loaded || !d.points.length) return;
    // Carries the contributor's identity and caveat, not just its line:
    // the legend names it, says whether it is still reporting, and shows
    // what is true of THIS source and not of the aspect.
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

/* What the house SAID about this window, as against what it now
 * believes: every issue the recorder kept. Fetched only when the reader
 * asks for it — a braid is analysis, and a house with no recorder has
 * none of this while still having a forecast to draw. */
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

/* What was asked of this aspect, beside what it did. The recorder types
 * every cmd envelope into its own series
 * (docs/design.md#history-and-the-recorder), and this is where that
 * audit is read. Always fetched rather than gated on whether the page
 * thinks the aspect is commandable: recorded commands are the honest
 * test, and the rules for who may command what live on the server. The strip renders only when the window actually holds
 * commands. */
function fetchCommandRange(entityName, aspect, hours) {
  var cd = overlay.cmdData[hours];
  if (cd && (cd.loaded || cd.loading)) return;
  overlay.cmdData[hours] = { points: [], loaded: false, loading: true };
  // Commands are edges, never a curve: their runs are the shape, whatever
  // the aspect's own readings are drawn as.
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
  // The descriptor's kind decides the shape where it has one, and its
  // word is final — an enum coded as integers is runs, not a curve.
  // Undescribed, the live value's type decides; without a value yet (a
  // fresh page, a sensor that has not reported) that is a guess the
  // first points fetched settle (fetchHistoryRange).
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
  // Sources are this entity's, so the layer and the fetched contributor
  // history do not survive into the next overlay.
  overlay.sourceData = {};
  overlay.sourceEvents = {};
  fetchHistoryRange(entityName, aspect, 24, overlay.shape);
  showOverlay();
}

/* Which line is which, on demand: pointing at a source in the legend
 * lights its line and stands the others down, and a tap holds that until
 * it is tapped again. Emphasis, not colour — a contributor owning a hue
 * would compete with the accent the computed value keeps
 * (docs/design.md#charts-forecasts-and-sources). */
export function wireSourceLegend(root) {
  var scope = root || document;
  var entries = scope.querySelectorAll('.source-legend span[data-source]');
  if (!entries.length) return;
  // The pin belongs to the overlay, not to this wiring. Live state
  // re-renders the panel — a forecast re-issue is enough — which throws
  // away these nodes and every class on them, and a fresh closure would
  // take its highlight from whatever the pointer happens to be over: a
  // pin that quietly moves to another source, or vanishes, while the
  // reader is still reading. Same reason the braid's pin lives in
  // `overlay.pinned`.
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
  // Re-apply on every wiring, so a re-render restores the pin rather than
  // dropping it.
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
  // One row for what is drawn beside the record: the several opinions
  // behind the value's past, or the several claims about its future.
  // Both are owner work, which is why they live here and not on a tile —
  // a family wants the temperature, not which sensor read low — and only
  // one can be drawn at a time, so they are one control and not two
  // (docs/design.md#charts-forecasts-and-sources). A chip appears only
  // where the house has the thing it names, so an ordinary sensor's
  // overlay has no such row.
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
  // The viewBox height is baked when the chart is built, so a taller
  // chart is a rebuild, not a restyle — which is why toggling the width
  // re-renders rather than just swapping a class.
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
      // Under `forecasts` the current belief steps aside: drawing it over
      // the braid would make one issue look special for no reason.
      forecast: overlay.layer === 'forecasts' ? null : liveForecastsOf(entity, aspect),
      issues: overlay.layer === 'forecasts' ? overlay.issues : null,
      pinned: overlay.pinned,
      // Contributors stand down behind the braid: two greys on one chart,
      // one per issue and one per source, would read as one set of lines.
      contributors: overlay.layer === 'forecasts' ? null : drawnSources(entity, aspect, overlay.range)
    });
    var stats = rangeStats(points);
    statsHtml = html`<div class="stat-row">
      ${statItem('Latest', stats.latest, aspect)}${statItem('Min', stats.min, aspect)}
      ${statItem('Max', stats.max, aspect)}${statItem('Avg', stats.avg, aspect)}</div>`;
  }

  // The four stats describe the RECORDED window the chips select, so a
  // drawn forecast can peak above the MAX beside it — which reads as a
  // contradiction unless the horizon says its own extreme out loud.
  // One caption per source: "where the horizon goes" is a claim, and
  // averaging two providers into one sentence would state a horizon
  // neither of them predicted.
  var horizonHtml = '';
  if (overlay.layer !== 'forecasts') {
    var beliefs = forecastsOf(entity, aspect);
    horizonHtml = beliefs.map(function (b) {
      var fresh = logic.forecastFreshness(b.forecast, Date.now());
      var who = beliefs.length > 1 ? b.source + ' · ' : '';
      var issued = issuedCaption(b.forecast);
      // A claim whose horizon has run out is not drawn here, so this note
      // is the only thing that distinguishes "this producer stopped" from
      // "this aspect has no forecast" — the braid beside it is where the
      // spent claim itself can still be read.
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
                // Issues from several providers in one braid would make
                // "they disagree" and "it drifted" look the same, so the
                // count says how many sources are mixed in.
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
  // The legend doubles as the scrub readout: with no cursor it names the
  // sources, with one it says what each read at that instant. Colour
  // cannot tell them apart — they are all the same grey, deliberately —
  // so naming them is the identification.
  var legendHtml = '';
  var drawn = overlay.layer === 'forecasts' ? null : drawnSources(entity, aspect, overlay.range);
  if (drawn) {
    var usage = sourceUsageNow(entity, aspect, overlay.range,
                               contributorsOf(entity, aspect));
    legendHtml = html`<div class="chart-note source-legend" id="sources-legend">${
      drawn.map(function (c) {
        // A contributor that has dropped out still draws — the line
        // stopping IS the diagnosis — but it must not read as live.
        // Two different ways to be out: its own device is gone, or the
        // computation is choosing not to use it.
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

/* What was asked, under what happened. An aspect nothing ever commanded
 * has no strip at all — this is the recorder's cmd series, so the strip
 * appears exactly where there is an audit to show. */
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
