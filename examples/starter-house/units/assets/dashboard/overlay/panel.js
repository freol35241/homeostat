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

/* Whether the detail overlay covers the window. This is a per-viewer
 * preference, so it is kept in localStorage rather than on the bus. It is
 * not house state, other browsers should not get it, and a reader who
 * widened the overlay once probably wants it wide next time. Every access
 * is guarded, because in a private window or with blocked site data even
 * reading the property throws, and that must not blank the page. */
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
    // no storage: the choice applies to this overlay but is not remembered
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
  mountCameraLive(null);  // hiding the panel only removes a class, so stop the stream here
  byId('overlay-backdrop').classList.remove('show');
  byId('overlay-panel').classList.remove('show');
}

export function closeOverlay() {
  if (history.state && history.state.homeostatOverlay) {
    history.back(); // triggers popstate below, which hides the panel
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
  // Every state delta re-runs this, so the content is replaced on either
  // side of the mount, not over it. Moving the player across an innerHTML
  // swap is not enough. Detaching a <video-rtc> and putting it back
  // re-runs its connectedCallback, which seeks to the live edge and calls
  // play(). That causes a stutter and a flash of native controls on every
  // delta.
  byId('overlay-head').innerHTML = r.body;
  byId('overlay-rest').innerHTML = r.rest || '';
  mountCameraLive(r.camera);
  wireChartInteractions(byId('overlay-body'));
  wireSourceLegend(byId('overlay-body'));
  // The overlay also re-renders on its own (on opening, when a fetch
  // returns, or when a group is toggled), and its controls need the
  // pending-command marks as much as the view's do.
  markPendingControls();
}

// The live player for `name`, or none. Mounting is idempotent: mounting
// the same camera again leaves the running element untouched.
function mountCameraLive(name) {
  var mount = byId('overlay-mount');
  var live = mount.firstElementChild;
  if (live && live.getAttribute('data-entity') === name) return;
  mount.innerHTML = '';  // a different camera, or none: stop the stream
  if (!name) return;
  live = document.createElement('video-rtc');
  live.setAttribute('data-entity', name);
  live.mode = 'mse';
  live.src = '/api/camera/' + encodeURIComponent(name) + '/live';
  mount.appendChild(live);
}
