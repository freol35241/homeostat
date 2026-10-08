/* The Now view. */
import { renderWidgets } from './compose.js';

// The error signal, not an inventory: people, what is out of the
// ordinary, and the map when anyone has a location. A reading earns a
// tile only by being placed in dashboard.toml.
export function renderNow() {
  renderWidgets('Now', [{ kind: 'people' }, { kind: 'deviations' }, { kind: 'map' }]);
}
