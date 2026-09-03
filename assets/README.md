# subbier icon assets

How to rebuild the rasters, and the rules for drawing the mark. What the mark
is and why it is a template image is in
[`docs/ARCHITECTURE.md` §9](../docs/ARCHITECTURE.md).

## Drawing rules

`sr.svg` is laid out on an **18-unit grid that maps 1:1 onto 18 device pixels
at 1x**: a 2-unit stroke, 2-unit counters on whole-pixel rows, a baseline at
y=15 and 1-unit sidebearings. Only the `R`'s leg is a diagonal. 18, 36 and 54
are exact multiples, so every edge stays on a whole pixel at 1x, 2x and 3x.
Never render it at a size that is not a multiple of 18.

- **Semibold, not bold.** Heavier closes the `s` apertures and the `R` counter,
  and at this size the counters are what make it readable.
- **The `s` is x-height (10 units) against the `R`'s cap height (12).** Set
  equal, it reads "SR" and the `.rs`-backwards joke is gone.
- **The raster stays square.** The tray backend fixes the height at 18pt and
  derives the width from the bitmap's aspect ratio.

If you edit the SVG, move things by whole units, then re-render at 18px and
look at it before committing.

## Regenerating

```sh
cargo install resvg     # once; pure Rust, no system dependencies
./build.sh
```

`build.sh` rewrites every generated file below from the two SVGs. `sips` and
`iconutil` are macOS built-ins; the comment at the top of `build.sh` says why
`resvg` rather than `rsvg-convert`, `sips` or `qlmanage` does the rasterising.

Then look at `sr-18.png` at actual size before committing.

## The files

| Path | What |
|---|---|
| `sr.svg` | menu bar mark, 18x18 black + alpha template. **Source of truth**, hand-edited; geometry notes are in a comment at its top. |
| `sr-color.svg` | bundle icon, 1024x1024, orange gradient on a white monogram. Hand-edited. |
| `sr-18.png` | 18x18 RGBA, generated |
| `sr-36.png` | 36x36 RGBA, generated — **the one the binary `include_bytes!`es** |
| `sr-54.png` | 54x54 RGBA, generated |
| `Subbier.icns` | 16–512 at 1x and 2x, generated |
