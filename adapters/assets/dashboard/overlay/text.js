/* The view-text overlay: the dashboard.toml block behind a view. */
import { html } from '../html.js';
import logic from '../logic.js';
import { showOverlay } from './panel.js';
import { overlay, store } from '../store.js';

export function openViewText(name) {
  overlay.type = 'text';
  overlay.viewName = name;
  showOverlay();
}

/* The text behind a view: the dashboard.toml block that makes it, with
 * the widget kinds that name what is on screen. Read-only, deliberately —
 * the dashboard never writes the house (docs/design.md#views-are-text)
 * — but it gives what someone points at a name they can use, with a
 * person or with an agent working in the house repo. */
export function renderViewTextBody() {
  var view = logic.viewsOf(store.model).filter(function (v) { return v.name === overlay.viewName; })[0];
  var text = logic.viewText(store.model, overlay.viewName) || '';
  var docs = 'https://github.com/freol35241/homeostat/blob/main/docs/';
  return {
    title: (view ? view.label : overlay.viewName) + ' · as text',
    body: html`<div class="view-text-note">This view is this block of <code>dashboard.toml</code> at the house root, rendered from what the house parsed (its comments are not kept).</div>
      <pre class="view-text">${text}</pre>
      <div class="view-text-note">Change it in the file, yourself or with an agent in the house repo; <code>homeostat plan</code> checks it before anything changes. <a href="${docs}widgets.md" target="_blank" rel="noopener">What each widget looks like</a> · <a href="${docs}manifest.md#dashboard-views-dashboardtoml" target="_blank" rel="noopener">every field</a></div>`
  };
}
