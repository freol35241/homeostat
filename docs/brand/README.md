# The Homeostat mark

The mark is a house held at a set point. It has a pine-green silhouette,
a step response cut into it, and an amber line the response settles onto.
It draws the idea that home automation is regulation
([docs/design.md](../design.md#what-homeostat-is) and the concept
diagram).

| File | Use |
|---|---|
| `homeostat-mark.svg` | the mark on light backgrounds: README, docs, listings |
| `homeostat-mark-dark.svg` | the same with the house one step lighter, for dark backgrounds |
| `homeostat-mark-mono.svg` | one colour, with the response cut through the house. It uses `currentColor`, for themed icons, status bars and inline glyphs |

`generate.py` generates all three from one geometry. Edit the numbers
there, not the SVGs:

    python3 docs/brand/generate.py docs/brand adapters/assets

Any further directory gets only the colour mark. `adapters/assets` holds
the dashboard's copy, which it serves as its favicon and wordmark. The
companion app's launcher and notification drawables use the same numbers.

## Palette

Two colours and an ink.

| Token | Hex | Where |
|---|---|---|
| house | `#1F5E4A` | the mark; the brand colour wherever one is needed |
| house, dark backgrounds | `#2A7A61` | the same, lighter so the silhouette stays visible on dark |
| trace | `#F2EFE6` | the response; light surfaces |
| set point | `#E8A33D` | the line; the only accent, used for an `alert` on the phone and a deviation on *Now* |

Green stands for calm and equilibrium, and is dark enough to look
restrained. Amber is a warm colour for the set point. The palette avoids
the sky blue and the green of the category's largest project. The green
here is a dark pine, not a bright eco-label green.

## Geometry

The canvas is 108 units, the same as Android's adaptive icon. The house
corners have a 4-unit radius, which takes off the sharp corner without
making the shape look soft. The response is a cubic curve with a minimum
curvature radius of 4.3, so its 7-unit stroke outlines cleanly into the
cutout.

In the one-colour mark, the set point passes behind the response. It
stops a unit short of the curve where the two cross, and it is absent
where the response has settled onto it. Even-odd filling cannot union two
holes, and the colour mark shows the same layering.
