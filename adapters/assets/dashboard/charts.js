/* Charts: line and timeline markup over dashboard-logic.js's geometry,
 * and the scrub, with its crosshair, tooltip and the readouts under a
 * chart. */
import { byId, html } from './html.js';
import logic from './logic.js';
import { renderOverlayContent } from './overlay/panel.js';
import { descriptorField, overlay } from './store.js';

export var CHART_VBW = 400; // internal viewBox width used for coordinate math; visual width is CSS 100%

// chartId -> {coords, width, height, aspect} — populated while building chart
// markup, consumed by the pointermove/pointerleave hover wiring.
var chartRegistry = {};

function chartPathData(coords) {
  return coords.map(function (c, i) {
    return (i === 0 ? 'M ' : 'L ') + c.x.toFixed(1) + ' ' + c.y.toFixed(1);
  }).join(' ');
}

export function formatChartValue(aspect, v) {
  if (typeof v !== 'number') return String(v);
  return logic.formatAspect(aspect || '', null, v);
}

// A stored issue's stroke: paler the older it is, within the accent's
// own hue so the braid still reads as one series' future.
function issueInk(age) {
  // Neutral, and deliberately NOT ramping into the accent: the outcome
  // wears the accent, and a braid that reached it would make its newest
  // line indistinguishable from what actually happened.
  var pale = [214, 218, 216], full = [113, 126, 120];
  var c = pale.map(function (v, i) { return Math.round(v + (full[i] - v) * age); });
  return 'rgb(' + c.join(',') + ')';
}

// When a drawn belief was issued, said out loud. Without it a claim made
// yesterday reads exactly like one made ten minutes ago.
export function issuedCaption(forecast) {
  var f = logic.forecastFreshness(forecast, Date.now());
  if (!f || f.issued === null) return '';
  // A bare clock is only unambiguous within the day it was said: "issued
  // 13:00" on a claim made yesterday reads as this afternoon, which is
  // the misreading this caption exists to prevent. Older than today, the
  // date comes with it.
  var sameDay = new Date(f.issued).toDateString() === new Date(Date.now()).toDateString();
  return 'issued ' + (sameDay ? formatChartClock(f.issued) : formatChartTime(f.issued));
}

// "min 0.38 at 03:00" — where the horizon goes, named at the hour it
// gets there. A flat horizon summarises to nothing and the caption stays
// empty rather than reporting an extreme that is just the current value.
export function horizonCaption(aspect, field, forecast, fromTs) {
  var summary = logic.horizonSummary(forecast, fromTs);
  if (!summary) return '';
  var falling = summary.min.t > summary.max.t;
  var point = falling ? summary.min : summary.max;
  return (falling ? 'min ' : 'max ') +
    logic.formatAspect(aspect, field, point.v) + ' at ' + formatChartClock(point.t);
}

// A reading's time, at the precision the window justifies: on an hour of
// history the recorder's buckets are seconds wide, so minutes alone hide
// the thing the reader opened the chart to see. `spanMs` is the charted
// window; without one the time is the day and the minute.
export function formatChartTime(ts, spanMs) {
  var d = new Date(ts);
  if (isNaN(d.getTime())) return '';
  var fmt = { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' };
  if (spanMs && spanMs <= 3 * 3600e3) fmt.second = '2-digit';
  return d.toLocaleString([], fmt);
}

// The same instant with the date dropped — the far end of a run, which
// the near end has already dated.
export function formatChartClock(ts, spanMs) {
  var d = new Date(ts);
  if (isNaN(d.getTime())) return '';
  var fmt = { hour: '2-digit', minute: '2-digit' };
  if (spanMs && spanMs <= 3 * 3600e3) fmt.second = '2-digit';
  return d.toLocaleTimeString([], fmt);
}

// Builds a chart's markup (line + optional area/gridlines/y-labels) and
// registers its geometry under chartId for hover/tooltip wiring. Guards
// against sparse/empty history with a "no history yet" placeholder.
export function buildChart(chartId, points, opts) {
  opts = opts || {};
  var height = opts.height || 36;
  var pad = opts.pad !== undefined ? opts.pad : 3;
  var sizeClass = opts.sizeClass || 'chart-tile';
  var geo = logic.chartGeometry(points || [], CHART_VBW, height, pad, opts.window, opts.forecast, opts.issues, opts.contributors);
  var inner;
  if (!geo) {
    chartRegistry[chartId] = null;
    inner = html`<div class="chart-empty">no history yet</div>`;
  } else {
    // The forecast's points join the hover registry, so a scrub reads
    // straight across the now-rule and "what will it be at 03:00" is the
    // same gesture as "what was it at 03:00".
    chartRegistry[chartId] = {
      coords: geo.forecast
        ? geo.coords.concat.apply(geo.coords, geo.forecast.map(function (f) { return f.coords; }))
        : geo.coords,
      issues: geo.issues || null,
      contributors: geo.contributors || null,
      actual: geo.coords,
      width: CHART_VBW, height: height, aspect: opts.aspect, window: opts.window
    };
    var d = chartPathData(geo.coords);
    // A forecast alone is enough to draw, so the recorded past can be
    // empty here — a detail overlay renders before its history arrives,
    // and a house that has a horizon but no recorder never has one.
    var last = geo.coords.length ? geo.coords[geo.coords.length - 1] : null;
    var marks = [];
    if (opts.gridlines) {
      [pad, height / 2, height - pad].forEach(function (y) {
        marks.push(html`<line class="chart-grid" x1="0" y1="${y.toFixed(1)}" x2="${CHART_VBW}" y2="${y.toFixed(1)}"/>`);
      });
    }
    // An area fill is for ONE series. Its baseline here is the smallest
    // value drawn, not zero and not any reference the reader chose, so
    // the filled area measures nothing — harmless while it is the only
    // mark and a slab the moment it is not, with a hard vertical cliff
    // where the record stops and the forecast takes over. A second
    // series (a horizon, a braid, a contributor) turns this into a line
    // chart, and a line chart carries its own shape.
    var alone = !(geo.forecast || geo.issues || geo.contributors);
    if (opts.area && last && alone) {
      var areaD = d + ' L ' + last.x.toFixed(1) + ' ' + (height - pad) + ' L ' + geo.coords[0].x.toFixed(1) + ' ' + (height - pad) + ' Z';
      marks.push(html`<path class="chart-area" d="${areaD}"/>`);
    }
    if (geo.issues) {
      geo.issues.forEach(function (f, n) {
        var d2 = chartPathData(f.coords);
        var pinned = opts.pinned !== undefined && opts.pinned !== null && f.issued === opts.pinned;
        marks.push(html`<path class="chart-issue-hit" data-issue="${n}" d="${d2}"/>`);
        marks.push(html`<path class="chart-issue${pinned ? ' pinned' : ''}" data-issue="${n}" d="${d2}" stroke="${pinned ? '' : issueInk(f.age)}"/>`);
      });
    }
    if (geo.contributors) {
      geo.contributors.forEach(function (c) {
        marks.push(html`<path class="chart-contributor" data-source="${c.name}" d="${chartPathData(c.coords)}"/>`);
      });
    }
    marks.push(html`<path class="chart-line" d="${d}"/>`);
    if (geo.forecast && geo.forecast.length) {
      // Dashed, and starting from the last recorded point so the eye
      // follows one line: the change in stroke is what says "from here
      // on this is a belief, not a record". With several sources each
      // gets its own line from that same point — they are claims about
      // one future, and which is which is read from the legend and the
      // scrub, not from a colour none of them can own.
      geo.forecast.forEach(function (f) {
        // Joined to the last record ONLY when it continues from there.
        // A claim that starts earlier already spans the now-rule under
        // its own steam, and anchoring it to the record would make the
        // path run backwards from the present to reach its own first
        // point — a straight dashed line into the past that no source
        // ever drew.
        var head = f.coords[0];
        var joined = last && head && last.x <= head.x ? [last].concat(f.coords) : f.coords;
        marks.push(html`<path class="chart-forecast${f.stale ? ' stale' : ''}" data-source="${f.source}" d="${chartPathData(joined)}"/>`);
      });
    }
    if (geo.nowX !== undefined) {
      marks.push(html`<line class="chart-now" x1="${geo.nowX.toFixed(1)}" y1="0" x2="${geo.nowX.toFixed(1)}" y2="${height}"/>`);
    }
    // The dot marks the latest RECORDED value, so there is none to mark
    // when nothing has been recorded.
    var dot = last
      ? html`<div class="chart-dot" style="left:${(last.x / CHART_VBW * 100).toFixed(2)}%; top:${(last.y / height * 100).toFixed(2)}%;"></div>`
      : '';
    inner = html`<svg viewBox="0 0 ${CHART_VBW} ${height}" preserveAspectRatio="none">${marks}
      <line class="chart-crosshair" x1="0" y1="0" x2="0" y2="${height}" style="display:none;"/></svg>
      ${dot}<div class="chart-crosshair-dot" style="display:none;"></div>
      ${opts.yLabels ? html`<div class="chart-ylabel chart-ylabel-top">${formatChartValue(opts.aspect, geo.max)}</div>
        <div class="chart-ylabel chart-ylabel-bottom">${formatChartValue(opts.aspect, geo.min)}</div>` : ''}`;
  }
  // A chart with no time axis leaves the reader to guess how far back it
  // goes; the range chip says "24h" but never where the window starts or
  // ends, and with a horizon drawn the right edge is not even now. The
  // middle slot names the now-rule when one is drawn, which is the only
  // instant on the axis a reader has to find rather than read.
  var axis = '';
  if (opts.yLabels && geo && geo.from && geo.to > geo.from) {
    var span = geo.to - geo.from;
    // The near end dates the axis; the far end only needs the clock,
    // unless it falls on another day.
    var sameDay = new Date(geo.from).toDateString() === new Date(geo.to).toDateString();
    var nowPct = geo.nowX === undefined ? null : (geo.nowX / CHART_VBW) * 100;
    // Crowded against either end it would collide with the label there,
    // and the rule itself already says where now is.
    var nowLabel = nowPct !== null && nowPct > 14 && nowPct < 86
      ? html`<span class="chart-xnow" style="left:${nowPct.toFixed(1)}%">now</span>`
      : '';
    axis = html`<div class="chart-xaxis">
      <span>${formatChartTime(geo.from, span)}</span>${nowLabel}
      <span>${sameDay ? formatChartClock(geo.to, span) : formatChartTime(geo.to, span)}</span>
      </div>`;
  }
  return html`<div class="chart-wrap ${sizeClass}" data-chart-id="${chartId}">${inner}</div>${axis}`;
}

// A timeline: one band per run of a bool or string state over the window
// (dashboard-logic.js, timelineRuns). A boolean is a filled band while
// true and a faint one while false; a string's runs alternate two tints
// so each change reads as an edge, and the tooltip names the value.
export function buildTimeline(chartId, points, opts) {
  var height = opts.height || 36;
  var sizeClass = opts.sizeClass || 'chart-tile';
  var runs = logic.timelineRuns(points || [], opts.window.from, opts.window.to);
  var inner;
  if (runs.length === 0) {
    chartRegistry[chartId] = null;
    inner = html`<div class="chart-empty">no history yet</div>`;
  } else {
    var span = opts.window.to - opts.window.from;
    chartRegistry[chartId] = { runs: runs, window: opts.window, width: CHART_VBW, height: height, aspect: opts.aspect, field: opts.field };
    var bands = runs.map(function (r, i) {
      var x = (r.start - opts.window.from) / span * CHART_VBW;
      var w = Math.max(1, (r.end - r.start) / span * CHART_VBW);
      var cls = r.value === true ? 'tl-on' : r.value === false ? 'tl-off' : (i % 2 ? 'tl-b' : 'tl-a');
      return html`<rect class="${cls}" x="${x.toFixed(1)}" y="2" width="${w.toFixed(1)}" height="${height - 4}"/>`;
    });
    inner = html`<svg viewBox="0 0 ${CHART_VBW} ${height}" preserveAspectRatio="none">${bands}
      <line class="chart-crosshair" x1="0" y1="0" x2="0" y2="${height}" style="display:none;"/></svg>`;
  }
  return html`<div class="chart-wrap ${sizeClass}" data-chart-id="${chartId}">${inner}</div>`;
}

export function minMaxCaption(points) {
  var vals = (points || []).map(function (p) { return p.value; }).filter(function (v) { return typeof v === 'number'; });
  if (vals.length === 0) return '';
  var min = Math.min.apply(null, vals);
  var max = Math.max.apply(null, vals);
  return min.toFixed(1) + '–' + max.toFixed(1) + ' today';
}

export function wireChartInteractions(root) {
  (root || document).querySelectorAll('.chart-wrap[data-chart-id]').forEach(function (wrap) {
    var id = wrap.getAttribute('data-chart-id');
    var reg = chartRegistry[id];
    if (!reg) return;
    var onMove;
    if (reg.runs) onMove = onTimelinePointerMove;
    else if (reg.coords && reg.coords.length >= 2) onMove = onChartPointerMove;
    else return;

    // A chart inside a card that taps through to something else keeps
    // hover-only reading: claiming the press there would fight the tap
    // the card exists for. The charts a reader actually scrubs — the
    // detail overlay's — sit in no such card.
    if (!wrap.closest('[data-action]')) {
      wrap.classList.add('scrubbable');
      var down = null;
      wrap.addEventListener('pointerdown', function (ev) {
        // A touch has no position until it moves, so the press itself has
        // to read, or a tap would show nothing and only a drag would work.
        // Capture keeps a finger that wanders off a 64px strip scrubbing
        // instead of dropping the gesture mid-read.
        if (wrap.setPointerCapture) wrap.setPointerCapture(ev.pointerId);
        down = { x: ev.clientX, y: ev.clientY };
        onMove(wrap, reg, ev);
      });
      // A tap pins the line under it; a drag is a scrub and pins nothing.
      // The two arrive as the same pointer sequence, so the only thing
      // separating them is how far the pointer travelled.
      wrap.addEventListener('pointerup', function (ev) {
        var moved = down && Math.hypot(ev.clientX - down.x, ev.clientY - down.y) > TAP_SLOP;
        down = null;
        if (moved || !reg.issues) return;
        var rect = wrap.getBoundingClientRect();
        if (!rect.width) return;
        var near = nearestIssue(
          reg, rect,
          ((ev.clientX - rect.left) / rect.width) * reg.width,
          ((ev.clientY - rect.top) / rect.height) * reg.height
        );
        overlay.pinned = near && overlay.pinned !== near.issue.issued ? near.issue.issued : null;
        renderOverlayContent();
      });
    }
    wrap.addEventListener('pointermove', function (ev) { onMove(wrap, reg, ev); });
    // pointerleave never fires for a lifted finger, and a gesture the
    // scroller claims arrives as pointercancel: either one would leave
    // the crosshair and the tooltip stranded on screen.
    wrap.addEventListener('pointerup', function () { onChartPointerLeave(wrap); });
    wrap.addEventListener('pointercancel', function () { onChartPointerLeave(wrap); });
    wrap.addEventListener('pointerleave', function () { onChartPointerLeave(wrap); });
  });
}

/* Which drawn issue the pointer is nearest, in BOTH dimensions. The
 * nearest point by x alone is right for one series and meaningless for a
 * braid: x alone picks whichever line happens to have a sample closest
 * horizontally, ignoring that the pointer may be nowhere near it. */
function nearestIssue(reg, rect, tx, ty) {
  if (!reg.issues || !reg.issues.length) return null;
  // Distance is measured in SCREEN pixels, not viewBox units. The
  // viewBox is stretched to the wrapper, so its x and y units are
  // nothing like the same size — at the full-window chart one x unit is
  // six times a y unit — and a circle of "nearness" in viewBox space is
  // a thin ellipse to the reader, who then cannot hit the line they are
  // pointing at.
  var sx = rect.width / reg.width, sy = rect.height / reg.height;
  var best = null, bestD = Infinity;
  reg.issues.forEach(function (f, n) {
    f.coords.forEach(function (p) {
      var dx = (p.x - tx) * sx, dy = (p.y - ty) * sy;
      var d = dx * dx + dy * dy;
      if (d < bestD) { bestD = d; best = { index: n, issue: f, point: p }; }
    });
  });
  // Far from every line is not a selection: a scrub across empty chart
  // should not drag a highlight along with it.
  return bestD <= NEAR_ISSUE * NEAR_ISSUE ? best : null;
}

// CSS pixels — what the reader sees, and roughly a fingertip.
var NEAR_ISSUE = 26;

// Pointer travel, in CSS pixels, past which a press is a scrub and not a
// tap. A thumb never holds perfectly still, so zero would mean nothing
// was ever tappable.
var TAP_SLOP = 8;

function onChartPointerMove(wrap, reg, ev) {
  var rect = wrap.getBoundingClientRect();
  if (rect.width === 0) return;
  var fx = (ev.clientX - rect.left) / rect.width;
  fx = Math.max(0, Math.min(1, fx));
  var tx = fx * reg.width;
  var svg = wrap.querySelector('svg');
  if (!svg) return;
  if (reg.issues) {
    onBraidPointerMove(wrap, reg, ev, rect, tx);
    return;
  }
  var c = reg.coords[0];
  reg.coords.forEach(function (p) { if (Math.abs(p.x - tx) < Math.abs(c.x - tx)) c = p; });
  var line = svg.querySelector('.chart-crosshair');
  var dot = wrap.querySelector('.chart-crosshair-dot');
  line.setAttribute('x1', c.x); line.setAttribute('x2', c.x);
  line.style.display = '';
  if (dot) {
    dot.style.left = (c.x / reg.width * 100).toFixed(2) + '%';
    dot.style.top = (c.y / reg.height * 100).toFixed(2) + '%';
    dot.style.display = '';
  }
  var px = rect.left + (c.x / reg.width) * rect.width;
  var py = rect.top + (c.y / reg.height) * rect.height;
  var span = reg.window ? reg.window.to - reg.window.from : 0;
  showChartTooltip(px, py, formatChartValue(reg.aspect, c.value), formatChartTime(c.ts, span));
  sourcesColumn(reg, tx);
}

/* Every source's reading at the instant under the pointer, written under
 * the chart. The tooltip is about the value the house believes; this is
 * about where that belief came from, which is the question the sources
 * were drawn to answer. */
function sourcesColumn(reg, tx) {
  var el = byId('sources-column');
  if (!el) return;
  if (!reg.contributors || tx === null) { el.textContent = ''; return; }
  var field = descriptorField(overlay.entity, overlay.aspect);
  var parts = [];
  reg.contributors.forEach(function (c) {
    var near = null;
    c.coords.forEach(function (p) {
      if (!near || Math.abs(p.x - tx) < Math.abs(near.x - tx)) near = p;
    });
    if (near) parts.push(c.label + ' ' + logic.formatAspect(overlay.aspect, field, near.value));
  });
  el.textContent = parts.join(' \u00b7 ');
}

/* Scrubbing a braid reads two things at once: the issue under the
 * pointer (highlighted, and named in the tooltip) and the whole column
 * at that instant (how many forecasts spoke about it, and how far apart
 * they were) — which is the one slice that cannot share this chart's
 * axis, delivered by the gesture rather than by a second control. */
function onBraidPointerMove(wrap, reg, ev, rect, tx) {
  var ty = ((ev.clientY - rect.top) / rect.height) * reg.height;
  var near = nearestIssue(reg, rect, tx, ty);
  wrap.querySelectorAll('.chart-issue').forEach(function (el) {
    el.classList.toggle('hovered', !!near && el.getAttribute('data-issue') === String(near.index));
  });
  var line = wrap.querySelector('.chart-crosshair');
  if (line) { line.setAttribute('x1', tx); line.setAttribute('x2', tx); line.style.display = ''; }
  var when = reg.window ? reg.window.from + (tx / reg.width) * (reg.window.to - reg.window.from) : null;
  var span = reg.window ? reg.window.to - reg.window.from : 0;
  if (!near) { hideChartTooltip(); braidColumn(reg, when); return; }
  var dot = wrap.querySelector('.chart-crosshair-dot');
  if (dot) {
    dot.style.left = (near.point.x / reg.width * 100).toFixed(2) + '%';
    dot.style.top = (near.point.y / reg.height * 100).toFixed(2) + '%';
    dot.style.display = '';
  }
  showChartTooltip(
    rect.left + (near.point.x / reg.width) * rect.width,
    rect.top + (near.point.y / reg.height) * rect.height,
    formatChartValue(reg.aspect, near.point.value),
    (near.issue.source ? logic.sourceLabel(near.issue.source) + ' \u00b7 ' : '') +
      'issued ' + formatChartTime(near.issue.issued, span) + ' \u2014 for ' +
      formatChartClock(near.point.ts, span)
  );
  braidColumn(reg, when);
}

// The column reading, written under the chart rather than into the
// tooltip: the tooltip is about one forecast, this is about all of them.
function braidColumn(reg, when) {
  var el = byId('braid-column');
  if (!el) return;
  var col = when === null ? null : logic.columnAt(overlay.issues || [], when);
  if (!col) { el.textContent = ''; return; }
  var field = descriptorField(overlay.entity, overlay.aspect);
  el.textContent = formatChartClock(when) + ' \u00b7 ' + col.count + ' forecast' +
    (col.count === 1 ? '' : 's') + ' \u00b7 ' +
    logic.formatAspect(overlay.aspect, field, col.min) + ' to ' +
    logic.formatAspect(overlay.aspect, field, col.max);
}

function onTimelinePointerMove(wrap, reg, ev) {
  var rect = wrap.getBoundingClientRect();
  if (rect.width === 0) return;
  var fx = Math.max(0, Math.min(1, (ev.clientX - rect.left) / rect.width));
  var t = reg.window.from + fx * (reg.window.to - reg.window.from);
  var run = null;
  reg.runs.forEach(function (r) { if (t >= r.start && t < r.end) run = r; });
  var line = wrap.querySelector('.chart-crosshair');
  if (!line) return;
  var x = fx * reg.width;
  line.setAttribute('x1', x); line.setAttribute('x2', x);
  line.style.display = '';
  if (!run) { hideChartTooltip(); return; }
  // A run's two edges and how long it held: "since 14:32" alone could not
  // say whether a state lasted a minute or all afternoon, which is most
  // of what a timeline is read for. The last run has no end yet.
  var span = reg.window.to - reg.window.from;
  var ongoing = run.end >= reg.window.to;
  var when = ongoing
    ? 'since ' + formatChartTime(run.start, span)
    : formatChartTime(run.start, span) + ' → ' + formatChartClock(run.end, span);
  showChartTooltip(ev.clientX, rect.top, logic.formatAspect(reg.aspect || '', reg.field, run.value),
    when + ' · ' + fmtDuration(run.end - run.start));
}

function onChartPointerLeave(wrap) {
  var line = wrap.querySelector('.chart-crosshair');
  var dot = wrap.querySelector('.chart-crosshair-dot');
  if (line) line.style.display = 'none';
  if (dot) dot.style.display = 'none';
  hideChartTooltip();
  var col = byId('sources-column');
  if (col) col.textContent = '';
}

function showChartTooltip(x, y, valueText, timeText) {
  var tt = byId('chart-tooltip');
  byId('chart-tooltip-value').textContent = valueText;
  byId('chart-tooltip-time').textContent = timeText;
  tt.classList.add('show');
  var ttRect = tt.getBoundingClientRect();
  var left = x + 12;
  var top = y - ttRect.height - 10;
  if (left + ttRect.width > window.innerWidth - 8) left = x - ttRect.width - 12;
  if (left < 8) left = 8;
  if (top < 8) top = y + 12;
  tt.style.left = left + 'px';
  tt.style.top = top + 'px';
}

function hideChartTooltip() {
  byId('chart-tooltip').classList.remove('show');
}

export function fmtDuration(ms) {
  var mins = Math.round(ms / 60000);
  if (mins < 60) return mins + ' min';
  var h = Math.floor(mins / 60), m = mins % 60;
  if (h < 48) return m ? h + ' h ' + m + ' min' : h + ' h';
  return Math.round(h / 24) + ' d';
}
