/* The detail overlay: a slide-over on a desktop, a sheet on a phone. Its
 * opening, closing and width, the live camera mount, and rendering the
 * body of whichever kind it holds. */
import { wireChartInteractions } from '../charts.js';
import { markPendingControls } from '../commands.js';
import { byId } from '../html.js';
import { renderEntityDetailBody } from './entity.js';
import { renderHistoryDetailBody, wireSourceLegend } from './history.js';
import { renderViewTextBody } from './text.js';
import { renderUnitDetailBody } from './unit.js';
import { overlay } from '../store.js';

/* Whether the detail overlay covers the window. A per-viewer
 * convenience, so it lives in localStorage rather than on the bus: it is
 * not house state, nobody else's browser should learn it, and a reader
 * who widened it once means it next time too. Every access is guarded —
 * a private window or blocked site data throws on the property itself,
 * and a chart is not worth a blank page. */
var WIDE_KEY = 'homeostat.overlay.wide';

export function overlayWide() {
  try {
    return window.localStorage.getItem(WIDE_KEY) === '1';
  } catch (e) {
    return false;
  }
}

export function setOverlayWide(wide) {
  try {
    window.localStorage.setItem(WIDE_KEY, wide ? '1' : '0');
  } catch (e) {
    // no storage: the choice still applies to this overlay, just not the next
  }
  applyOverlayWide(wide);
}

function applyOverlayWide(wide) {
  byId('overlay-panel').classList.toggle('wide', wide);
  byId('overlay-wide').setAttribute('aria-pressed', wide ? 'true' : 'false');
  // The charts read their own width from the DOM, so they have to be
  // rebuilt rather than merely restyled.
  if (overlay.open) renderOverlayContent();
}

export function showOverlay() {
  var wasOpen = overlay.open;
  overlay.open = true;
  applyOverlayWide(overlayWide());
  renderOverlayContent();
  byId('overlay-backdrop').classList.add('show');
  var panel = byId('overlay-panel');
  panel.classList.add('show');
  if (!wasOpen) {
    history.pushState({ homeostatOverlay: true }, '', location.pathname + location.search + location.hash);
  }
  panel.focus();
}

function hideOverlayUI() {
  overlay.open = false;
  overlay.type = null;
  mountCameraLive(null);  // hiding the panel only drops a class; the stream would outlive it
  byId('overlay-backdrop').classList.remove('show');
  byId('overlay-panel').classList.remove('show');
}

export function closeOverlay() {
  if (history.state && history.state.homeostatOverlay) {
    history.back(); // triggers popstate below, which actually hides the panel
  } else {
    hideOverlayUI();
  }
}

window.addEventListener('popstate', function (e) {
  if (!e.state || !e.state.homeostatOverlay) {
    hideOverlayUI();
  }
});

export function renderOverlayContent() {
  if (!overlay.open) return;
  var r;
  if (overlay.type === 'history') r = renderHistoryDetailBody();
  else if (overlay.type === 'entity') r = renderEntityDetailBody();
  else if (overlay.type === 'unit') r = renderUnitDetailBody();
  else if (overlay.type === 'text') r = renderViewTextBody();
  else r = { title: '', body: '' };
  byId('overlay-title').textContent = r.title;
  // Every state delta re-runs this, so the swap happens either side of
  // the mount rather than over it. Carrying the player across an
  // innerHTML swap is not enough: detaching a <video-rtc> and putting it
  // back re-runs its connectedCallback, which seeks to the live edge and
  // calls play() — a stutter and a flash of native controls per delta.
  byId('overlay-head').innerHTML = r.body;
  byId('overlay-rest').innerHTML = r.rest || '';
  mountCameraLive(r.camera);
  wireChartInteractions(byId('overlay-body'));
  wireSourceLegend(byId('overlay-body'));
  // The overlay re-renders on its own (opening, a fetch landing, a group
  // toggled), and its controls are as much the command's as the view's.
  markPendingControls();
}

// The live player for `name`, or none. Mounting is idempotent: the same
// camera leaves the running element untouched, which is the whole point.
function mountCameraLive(name) {
  var mount = byId('overlay-mount');
  var live = mount.firstElementChild;
  if (live && live.getAttribute('data-entity') === name) return;
  mount.innerHTML = '';  // a different camera, or none: drop the stream
  if (!name) return;
  live = document.createElement('video-rtc');
  live.setAttribute('data-entity', name);
  live.mode = 'mse';
  live.src = '/api/camera/' + encodeURIComponent(name) + '/live';
  mount.appendChild(live);
}
