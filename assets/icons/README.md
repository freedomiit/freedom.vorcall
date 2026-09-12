The Vorcall UI icon set: hand-drawn, stroke-based SVGs on a 24-unit grid, uncoloured (`stroke="currentColor"`) so the client tints them through iced's `svg::Style { color }`. Five glyphs — `dots`, `drag`, `info`, `warning` and `palette` — carry filled discs as well (`fill="currentColor"`), which take the same tint.

Regenerate with `python3 assets/brand/gen.py icons assets/icons` — this mode needs no shapely, unlike the brand outputs.

Edit the geometry in `assets/brand/gen.py`, never these files: every icon must come out byte-identical on a regeneration.
