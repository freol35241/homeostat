# Vendored map assets

Served by dashboard.py at `/assets/` so the dashboard makes zero external
fetches at runtime (tile CDNs would leak family positions — see
docs/design.md, Map and person entities). Update by re-downloading and
bumping this table.

| file | source | sha256 |
|---|---|---|
| leaflet.js | https://unpkg.com/leaflet@1.9.4/dist/leaflet.js | db49d009c841f5ca34a888c96511ae936fd9f5533e90d8b2c4d57596f4e5641a |
| leaflet.css | https://unpkg.com/leaflet@1.9.4/dist/leaflet.css | a7837102824184820dfa198d1ebcd109ff6d0ff9a2672a074b9a1b4d147d04c6 |
| protomaps-leaflet.js | https://unpkg.com/protomaps-leaflet@4.0.1/dist/protomaps-leaflet.js | 8e3d2aa0f5a2fd46871ff9c6ed47fdcdb969bc6ed10bf6719dee507b46a2ec9e |

Leaflet's `images/` directory is intentionally omitted: the map uses
`divIcon` markers and no layers control, so nothing references it.
