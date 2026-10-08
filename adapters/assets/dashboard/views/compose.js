/* A view made of widgets. The list comes from dashboard.toml, or is
 * implied by a generated view. Each widget is a card built from things
 * the page already renders. A widget only says where something goes; the
 * dashboard's own code draws it. */
import { byId, html } from '../html.js';
import { entitiesInRoom, findEntityByName, personEntities, unitByName } from '../store.js';
import { widgetBurner } from '../widgets/burner.js';
import { widgetDeviations } from '../widgets/deviations.js';
import { widgetDial } from '../widgets/dial.js';
import { renderRoomCard, widgetEntity } from '../widgets/entity.js';
import { mountMap, renderMapCard } from '../widgets/map.js';
import { widgetParams } from '../widgets/params.js';
import { widgetPeople } from '../widgets/people.js';
import { widgetChart, widgetTiles } from '../widgets/readings.js';
import { widgetUnit } from '../widgets/unit.js';

// Lays out a view's widgets, in order, in one grid. The map is mounted
// after the markup is inserted, because Leaflet owns its container.
export function renderWidgets(title, widgets) {
  var persons = personEntities();
  var wantMap = false;
  // One widget's card. A group calls this for each of its members, so a
  // member renders the same as it would directly on the view.
  function card(w) {
    if (w.kind === 'tile') {
      var te = findEntityByName(w.entity);
      return te ? widgetTiles(te, w.aspect) : '';
    }
    if (w.kind === 'chart') {
      var ce = findEntityByName(w.entity);
      return ce ? widgetChart(ce, w.aspect, w.hours || 24) : '';
    }
    if (w.kind === 'entity') {
      var ee = findEntityByName(w.entity);
      return ee ? widgetEntity(ee) : '';
    }
    if (w.kind === 'dial') {
      var de = findEntityByName(w.entity);
      return de ? widgetDial(de, w.aspect) : '';
    }
    if (w.kind === 'burner') {
      var bu = findEntityByName(w.entity);
      return bu ? widgetBurner(bu) : '';
    }
    if (w.kind === 'room') return renderRoomCard(w.room, entitiesInRoom(w.room));
    if (w.kind === 'unit') return widgetUnit(w.unit);
    if (w.kind === 'params') {
      var pu = unitByName(w.unit);
      return pu ? widgetParams(pu) : '';
    }
    if (w.kind === 'people') return widgetPeople(persons);
    if (w.kind === 'deviations') return widgetDeviations();
    if (w.kind === 'map' && persons.length > 0) {
      wantMap = true;
      return renderMapCard(persons);
    }
    if (w.kind === 'group') return widgetGroup(w, card);
    return '';
  }
  var cards = html`${widgets.map(card)}`;
  byId('view').innerHTML = html`<h1 class="view-title">${title}</h1>
    ${String(cards) ? html`<div class="grid">${cards}</div>` : html`<div class="card"><div class="empty-hint">Nothing to show yet.</div></div>`}`;
  if (wantMap) mountMap(persons);
}

// A group is one card around its members: its label, then each member's
// own card. Inside a group the CSS removes each member's card frame, so a
// dial and the traces that explain it read as one thing. Groups are one
// level deep, and the core refuses a group inside a group.
function widgetGroup(w, card) {
  var body = html`${(w.widgets || []).map(card)}`;
  if (!String(body)) return '';
  return html`<div class="card group-card">
    ${w.label ? html`<div class="card-label">${w.label.toUpperCase()}</div>` : ''}
    ${body}</div>`;
}
