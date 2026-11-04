# Vendored assets

dashboard.py serves these files at `/assets/`, so the dashboard makes no
external requests at runtime. A tile CDN would learn the family's
positions (see [Map and people](../../docs/design.md#map-and-people)). To
update a file, download it again and update its row in this table.

| file | source | sha256 |
|---|---|---|
| map/basemaps.mjs | https://unpkg.com/@protomaps/basemaps@5.7.2/dist/esm/index.js | 9d6ee63bee82fd4af79bedaaab538f904341b35fbc1f6254b86166aa4ee24cf1 |
| map/fonts/Noto Sans Italic/0-255.pbf | https://protomaps.github.io/basemaps-assets/fonts/Noto%20Sans%20Italic/0-255.pbf | 43edfca91c285ba1226f09d5e74d68e1473a088c517f02a5212ff6ccb10037dc |
| map/fonts/Noto Sans Italic/256-511.pbf | https://protomaps.github.io/basemaps-assets/fonts/Noto%20Sans%20Italic/256-511.pbf | a6f9f6574c86a4a28aba630ca1017857ceadd2370ed32a1293c35f819ab9bd60 |
| map/fonts/Noto Sans Italic/8192-8447.pbf | https://protomaps.github.io/basemaps-assets/fonts/Noto%20Sans%20Italic/8192-8447.pbf | 5dc4e6680116fef01be024e272859d130262613c7e634409a590e5d50ad124ae |
| map/fonts/Noto Sans Medium/0-255.pbf | https://protomaps.github.io/basemaps-assets/fonts/Noto%20Sans%20Medium/0-255.pbf | ba2f0118dd024e3041b158e5f9eb49bc0a658019f53f458e9f5c0b8efcd79b91 |
| map/fonts/Noto Sans Medium/256-511.pbf | https://protomaps.github.io/basemaps-assets/fonts/Noto%20Sans%20Medium/256-511.pbf | d5e801a1a5b1d409d3298c3a1e1ca76328e2314a751078833a618620e8e66e4d |
| map/fonts/Noto Sans Medium/8192-8447.pbf | https://protomaps.github.io/basemaps-assets/fonts/Noto%20Sans%20Medium/8192-8447.pbf | cc38e4956207f0edba1aaf749b61e8f6f1678ef1ad443d32ee285b21d0eb67aa |
| map/fonts/Noto Sans Regular/0-255.pbf | https://protomaps.github.io/basemaps-assets/fonts/Noto%20Sans%20Regular/0-255.pbf | 62c6d49b15fa836eb6aa45e259c7ca6762f44b011b09e47776efbe4a6db1b397 |
| map/fonts/Noto Sans Regular/256-511.pbf | https://protomaps.github.io/basemaps-assets/fonts/Noto%20Sans%20Regular/256-511.pbf | 2eca7561f9f566bcacfda5dd04fb5880baec1328ec0f5484678289a13994de8a |
| map/fonts/Noto Sans Regular/8192-8447.pbf | https://protomaps.github.io/basemaps-assets/fonts/Noto%20Sans%20Regular/8192-8447.pbf | 8ea977a587352fe31b4159ffdbc9a40be79056f2472017c742ea1e4a931864b9 |
| map/maplibre-gl-worker.mjs | https://unpkg.com/maplibre-gl@6.16.0/dist/maplibre-gl-worker.mjs | 0bf06492bdb7ba3d4132b78e435621bfba60d5250967991c02b1a73affea8cd5 |
| map/maplibre-gl.css | https://unpkg.com/maplibre-gl@6.16.0/dist/maplibre-gl.css | a90798a1953017790e590f7e1bd35ff103a8a4263972cb47290b78adf86f8c20 |
| map/maplibre-gl.mjs | https://unpkg.com/maplibre-gl@6.16.0/dist/maplibre-gl.mjs | fbbdf8ddd9a48e49059a4be6795a4f214a711314bb5809f72d3730b48e7897e3 |
| map/pmtiles.js | https://unpkg.com/pmtiles@4.5.0/dist/pmtiles.js | caf981bc46f6327ee7e65d5dc964d89d38a69f60edca2bd4c5c890c21b554c6c |
| map/sprites/dark.json | https://protomaps.github.io/basemaps-assets/sprites/v4/dark.json | bfac76cf7ed5c2aa2992695904056a1c6b07785b7fd20e6c640cb44fd6244a2e |
| map/sprites/dark.png | https://protomaps.github.io/basemaps-assets/sprites/v4/dark.png | 8920f68e8f9f7a4188da43e62a81bbc426532579fff7b5f27aad09e130f4246e |
| map/sprites/dark@2x.json | https://protomaps.github.io/basemaps-assets/sprites/v4/dark@2x.json | 1fb5b123fbe35d2e1f6ac171513ce01df89e671273a417ee89c44886a4c132f0 |
| map/sprites/dark@2x.png | https://protomaps.github.io/basemaps-assets/sprites/v4/dark@2x.png | 6e01b4e366d304f27bbd55390511a3d4e39aa1d03eb3e82f873f7ea32e74cbe4 |
| map/sprites/light.json | https://protomaps.github.io/basemaps-assets/sprites/v4/light.json | bfac76cf7ed5c2aa2992695904056a1c6b07785b7fd20e6c640cb44fd6244a2e |
| map/sprites/light.png | https://protomaps.github.io/basemaps-assets/sprites/v4/light.png | b6a34640917bdc57d0bd080836db33376371a3312ebe7b849045268015de3481 |
| map/sprites/light@2x.json | https://protomaps.github.io/basemaps-assets/sprites/v4/light@2x.json | 1fb5b123fbe35d2e1f6ac171513ce01df89e671273a417ee89c44886a4c132f0 |
| map/sprites/light@2x.png | https://protomaps.github.io/basemaps-assets/sprites/v4/light@2x.png | 23f6e9df27c2e9a14385763980a24e3608966bfc8192a106058bc2ca959ab563 |
| video-rtc.js | https://raw.githubusercontent.com/AlexxIT/go2rtc/master/www/video-rtc.js (VideoRTC v1.6.0, MIT) | d48ce627baf7c341a92c0f5844a3c546431f9db873ff21489671aba2ecfe64fb |

The map's files are all under `map/`, which dashboard.py serves by file
type and `scripts/sync_starter.sh` copies whole:

- MapLibre GL draws the map. It loads `maplibre-gl-worker.mjs` from
  beside `maplibre-gl.mjs`, so the two stay in one directory. Their source
  maps are left out; a browser asks for one only while its developer
  tools are open.
- `pmtiles.js` reads the house's tile extract. It is the standalone build:
  the module build imports its decompressor by package name, which a page
  cannot resolve.
- `basemaps.mjs` is `@protomaps/basemaps`, which builds the style's
  layers for a light or dark map.
- `fonts/` and `sprites/` are what that style asks for, from
  [basemaps-assets](https://protomaps.github.io/basemaps-assets/). The
  fonts are cut into ranges of 256 code points. Only the Latin ones are
  here: Basic Latin and Latin-1 (0-255), Latin Extended (256-511) and
  punctuation (8192-8447), for each of the three fonts the style uses. A
  label with other characters, such as a place with only a Russian name,
  asks for a range that is not here. That request fails, which the
  browser logs, and MapLibre draws those characters in the browser's own
  font instead. To add a script, download its range for each font into
  the same directories, for example Cyrillic as `1024-1279.pbf`, and add
  the rows here.

`homeostat-mark.svg` is not vendored. It is the project's own mark,
generated by `docs/brand/generate.py` (run it with this directory as a
second argument). It is served from here so the page's favicon and
wordmark load nothing external. Edit the numbers in the generator, not
this copy.

The rest of this directory is the repo's own code, not vendored, and is
edited like any source file:

- `dashboard-logic.js` is the page's pure decision logic. It is kept apart
  from the page so `node --test tests/js` can test it.
- `dashboard.css` is the page's stylesheet.
- `dashboard/` holds the page's ES modules. `dashboard/main.js` is the
  entry point.
