/* Which view is drawn: the generated ones by kind, a file's own as its
 * widgets, and Health and Not shown, which are chrome. */
import { wireChartInteractions } from '../charts.js';
import { currentView } from '../chrome.js';
import { byId, html } from '../html.js';
import logic from '../logic.js';
import { store } from '../store.js';
import { renderWidgets } from './compose.js';
import { renderHealth } from './health.js';
import { renderNotShown } from './notshown.js';
import { renderNow } from './now.js';
import { renderRooms } from './rooms.js';
import { renderSetpoints } from './setpoints.js';

var GENERATED = { now: renderNow, setpoints: renderSetpoints, rooms: renderRooms };

export function renderView() {
  var current = currentView();
  var view = logic.viewsOf(store.model).filter(function (v) { return v.name === current; })[0];
  if (current === 'health') renderHealth();
  else if (current === 'notshown') renderNotShown();
  else if (view && view.kind) GENERATED[view.kind]();
  else if (view) renderWidgets(view.label, view.widgets);
  // A view the nav names is text in the house repo, and says so: the
  // button opens that text. Health and Not shown are chrome, never text.
  var title = byId('view').querySelector('h1.view-title');
  if (view && title && logic.viewText(store.model, view.name)) {
    title.insertAdjacentHTML('beforeend', html`<button class="view-text-btn" data-action="view-text" data-view-name="${view.name}" title="The text in dashboard.toml that makes this view">Text</button>`);
  }
  wireChartInteractions(byId('view'));
}
