/* The frame around the views: the rail, the phone's tabs, the top bar's
 * status button and its sheet, the reconnecting banner, the toast, and the
 * taps and keys that move between views. */
import { byId, html } from './html.js';
import logic, { titleCase, unitNameFromHealthKey } from './logic.js';
import { closeOverlay } from './overlay/panel.js';
import { overlay, store } from './store.js';
import { renderView } from './views/index.js';

// Tab icons by generated view; a file's own view gets the generic one.
var TAB_ICONS = {
  view: html`<svg viewBox="0 0 24 24" width="22" height="22" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><rect x="3.5" y="4" width="17" height="16" rx="2"/><line x1="3.5" y1="9.5" x2="20.5" y2="9.5"/><line x1="9" y1="9.5" x2="9" y2="20"/></svg>`,
  now: html`<svg viewBox="0 0 24 24" width="22" height="22" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 12 8 12 10 6 14 18 16 12 21 12"/></svg>`,
  setpoints: html`<svg viewBox="0 0 24 24" width="22" height="22" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><line x1="6" y1="4" x2="6" y2="20"/><circle cx="6" cy="9" r="2.2" fill="currentColor" stroke="none"/><line x1="12" y1="4" x2="12" y2="20"/><circle cx="12" cy="16" r="2.2" fill="currentColor" stroke="none"/><line x1="18" y1="4" x2="18" y2="20"/><circle cx="18" cy="11" r="2.2" fill="currentColor" stroke="none"/></svg>`,
  rooms: html`<svg viewBox="0 0 24 24" width="22" height="22" fill="none" stroke="currentColor" stroke-width="2"><rect x="3.5" y="3.5" width="7.5" height="7.5" rx="1.4"/><rect x="13" y="3.5" width="7.5" height="7.5" rx="1.4"/><rect x="3.5" y="13" width="7.5" height="7.5" rx="1.4"/><rect x="13" y="13" width="7.5" height="7.5" rx="1.4"/></svg>`
};

export function updateBanner() {
  var b = byId('banner');
  if (!b) return;
  b.classList.toggle('show', !store.connected);
}

var toastTimer = null;

export function toast(msg) {
  var el = byId('toast');
  el.textContent = msg;
  el.classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(function () { el.classList.remove('show'); }, 3200);
}

function healthWorstAndCount() {
  var order = { open: 0, backoff: 1, starting: 2, stopped: 2, running: 3 };
  var worst = null;
  var okCount = 0;
  var total = 0;
  Object.keys(store.health).forEach(function (k) {
    var h = store.health[k] || {};
    total++;
    var status = h.status || 'running';
    if (status === 'running') okCount++;
    var rank = order[status];
    if (rank === undefined) rank = 3;
    if (worst === null || rank < worst.rank) {
      worst = { rank: rank, status: status, key: k };
    }
  });
  return { worst: worst, ok: okCount, total: total };
}

export function statusGlyph(status) {
  if (status === 'running') return '●'; // ●
  if (status === 'backoff') return '▲'; // ▲
  if (status === 'open') return '✕'; // ✕
  return '○'; // ○ starting / stopped / unknown
}

export function statusChipHtml(status) {
  return html`<span class="status-badge"><span class="status-glyph status-${status}">${statusGlyph(status)}</span>${status}</span>`;
}

export function healthTooltip(h) {
  var restarts = (h.restarts !== undefined && h.restarts !== null) ? h.restarts : 0;
  var backoff = (h.backoff_ms !== undefined && h.backoff_ms !== null) ? h.backoff_ms : 0;
  var exitCode = (h.last_exit_code !== undefined && h.last_exit_code !== null) ? h.last_exit_code : 0;
  return 'restarts ' + restarts + ' · backoff ' + backoff + 'ms · exit ' + exitCode;
}

export function fmtOrDash(v) {
  return (v === undefined || v === null) ? '—' : String(v);
}

// The nav is the file's views (dashboard-logic.js, viewsOf); Health and
// Not shown are fixed chrome below it, never views the file can name.
export function currentView() {
  var views = logic.viewsOf(store.model);
  if (store.view === 'health' || store.view === 'notshown') return store.view;
  var named = views.filter(function (v) { return v.name === store.view; })[0];
  return named ? named.name : (views.length ? views[0].name : 'notshown');
}

export function renderShell() {
  var views = logic.viewsOf(store.model);
  var current = currentView();
  var unplaced = logic.placement(store.model);
  var notShownCount = unplaced.entities.length + unplaced.params.length;

  var nav = byId('rail-nav');
  nav.innerHTML = html`${views.map(function (v) {
    return html`<button data-view="${v.name}" class="${current === v.name ? 'active' : ''}">${v.label}</button>`;
  })}`;

  // The phone's bottom bar is the house's views and nothing else: every
  // slot there is one the file named. Health and Not shown are behind the
  // top bar's status button instead (the status sheet, below).
  var tabs = byId('tabs-row');
  var tabHtml = views.map(function (v) {
    return html`<button data-view="${v.name}" class="${current === v.name ? 'active' : ''}">${TAB_ICONS[v.kind] || TAB_ICONS.view}<span class="tab-label">${v.label}</span></button>`;
  });
  // Rebuilding the scroller would reset its scroll position on every
  // render; only the active marks change between renders of the same nav.
  var viewsRow = tabs.querySelector('.tabs-views');
  var same = viewsRow && viewsRow.getAttribute('data-views') === views.map(function (v) { return v.name; }).join('\n');
  if (!same) {
    tabs.innerHTML = html`<div class="tabs-views" data-views="${views.map(function (v) { return v.name; }).join('\n')}">${tabHtml}</div>`;
  } else {
    tabs.querySelectorAll('button[data-view]').forEach(function (b) {
      b.classList.toggle('active', b.getAttribute('data-view') === current);
    });
  }
  var activeTab = tabs.querySelector('.tabs-views button.active');
  if (activeTab && activeTab.scrollIntoView) activeTab.scrollIntoView({ block: 'nearest', inline: 'nearest' });

  var hw = healthWorstAndCount();
  var pin = byId('health-pin');
  var healthRows;
  if (hw.total === 0) {
    healthRows = html`<div class="row faint">no units yet</div>`;
  } else {
    var worstLine = hw.worst.status === 'running'
      ? ''
      : html`<div class="row"><span class="status-glyph status-${hw.worst.status}">${statusGlyph(hw.worst.status)}</span>&nbsp;
        ${titleCase(unitNameFromHealthKey(hw.worst.key))} · ${hw.worst.status}</div>`;
    healthRows = html`${worstLine}
      <div class="row"><span class="status-glyph status-running">●</span> ${hw.ok}/${hw.total} ok</div>`;
  }
  var chrome = html`<button class="pin-btn${current === 'health' ? ' active' : ''}" data-view="health"><div class="label">Health</div>${healthRows}</button>
    <button class="pin-btn${current === 'notshown' ? ' active' : ''}" data-view="notshown"><div class="label">Not shown</div>
    <div class="row">${notShownCount
      ? unplaced.entities.length + (unplaced.entities.length === 1 ? ' entity' : ' entities') +
        (unplaced.params.length ? ' · ' + unplaced.params.length + (unplaced.params.length === 1 ? ' setpoint' : ' setpoints') : '')
      : 'everything is on a view'}</div></button>`;
  pin.innerHTML = chrome;
  var about = aboutHtml();
  byId('rail-about').innerHTML = about;

  // The phone: one button in the top bar, the house's health at a glance,
  // opening the same chrome and the same about lines as the rail.
  var statusBtn = byId('topbar-status');
  var glyphStatus = hw.worst ? hw.worst.status : 'running';
  statusBtn.innerHTML = html`<span class="status-glyph status-${glyphStatus}">${statusGlyph(glyphStatus)}</span>
    ${hw.total ? hw.ok + '/' + hw.total : 'health'}${notShownCount ? html`<span class="faint">· ${notShownCount} not shown</span>` : ''}`;
  statusBtn.setAttribute('aria-label', 'Health, Not shown and about');
  statusBtn.classList.toggle('active', current === 'health' || current === 'notshown' || statusSheetOpen);
  statusBtn.setAttribute('aria-expanded', String(statusSheetOpen));
  var sheet = byId('status-sheet');
  sheet.innerHTML = html`${chrome}<div class="about">${about}</div>`;
  sheet.classList.toggle('show', statusSheetOpen);

  updateBanner();
}

var statusSheetOpen = false;

// Where the page is served from (dashboard.py, /api/model `about`) and
// where the project lives.
function aboutHtml() {
  var lines = logic.aboutLines(store.model && store.model.about);
  var link = function (href, text) {
    return html`<a href="${href}" target="_blank" rel="noopener">${text}</a>`;
  };
  return html`${lines.map(function (l) {
    return html`<div>${l.label} ${l.href ? link(l.href, l.text) : l.text}
      ${l.commit ? html` · ${link(l.commitHref, l.commit)}` : ''}
      ${l.note ? html` <span class="about-note">${l.note}</span>` : ''}</div>`;
  })}
    <div class="about-links">${logic.ABOUT_LINKS.map(function (l) { return link(l.href, l.label); })}</div>`;
}

// Registered before main.js's delegated taps, because this module runs
// before main.js, which imports it: a tap that only puts the sheet away
// stops here and reaches nothing else.
document.addEventListener('click', function (e) {
  var toggle = e.target.closest('[data-action="status-sheet"]');
  var inSheet = e.target.closest('#status-sheet');
  if (toggle) {
    statusSheetOpen = !statusSheetOpen;
    renderShell();
    return;
  }
  if (statusSheetOpen && !inSheet) {
    // A tap outside puts the sheet away and does nothing else: the
    // finger was aiming at the sheet's edge, not at what lies under it.
    statusSheetOpen = false;
    renderShell();
    e.preventDefault();
    e.stopImmediatePropagation();
    return;
  }
  if (statusSheetOpen && e.target.closest('[data-view]')) {
    statusSheetOpen = false; // a pick in the sheet puts it away
    renderShell();
  }
  var btn = e.target.closest('[data-view]');
  if (btn) {
    store.view = btn.getAttribute('data-view');
    renderShell();
    renderView();
  }
});

document.addEventListener('keydown', function (e) {
  if (e.key === 'Escape' && overlay.open) closeOverlay();
  else if (e.key === 'Escape' && statusSheetOpen) {
    statusSheetOpen = false;
    renderShell();
    byId('topbar-status').focus();
  }
});
