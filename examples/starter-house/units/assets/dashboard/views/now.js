/* The Now view. */
import { renderWidgets } from './compose.js';

// What needs attention, not an inventory: people, what is out of the
// ordinary, and the map when anyone has a location. A reading gets a tile
// only when dashboard.toml places it.
export function renderNow() {
  renderWidgets('Now', [{ kind: 'people' }, { kind: 'deviations' }, { kind: 'map' }]);
}
