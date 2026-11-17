# bake-backdrop

Draws the page backdrop of Pechu (the 3 blurred silk ribbons) once, as images, for the **desktop app only**.
The desktop window (WebKitGTK) paints the live blurred SVG on the CPU, which delayed the first paint by about 5 s and the
app content by about 10 s. With these images the window never computes the blur.

## Re-bake

```
cd desktop/tools/bake-backdrop
bun install
bun run bake
```

Writes `desktop/assets/backdrop/{light,dark}-{a,b,c}.webp` (6 files, about 60 KB each, 1600x1000 with transparency).
Options (environment variables): `BAKE_WIDTH` (default 1600), `BAKE_QUALITY` (WebP quality, default 80), `BAKE_OUT`.

**Re-run it whenever `public/js/backdrop.js`, the `--bd-*` colours in `public/css/backdrop.css`, or `--coral` in
`public/styles.css` change**, then rebuild the desktop app (the images are embedded in it).

## How it works

1. `public/js/backdrop.js` is loaded as text and run against a fake `document` (linkedom), so the real `ribbonSvg` builds each
   layer. Nothing is copied from it and `public/` is not touched.
2. Every `var(--x)` is replaced with the theme's real value, read from `public/css/backdrop.css` and `public/styles.css`
   (light = `:root`, dark = `:root[data-theme="dark"]`). The rim opacity `--bd-rim-k` (0.55 light, 1 dark) is put on the
   `.bd-rimg` group, so it is baked in.
3. `@resvg/resvg-js` draws it with a 300 unit margin (so blur from outside the 1600x1000 area reaches in), `sharp` cuts the
   margin off and saves a WebP with alpha.
4. `desktop/assets/static-backdrop.css` hides each layer's `<svg>` and shows the matching image
   (`background: right top / cover`, the same as the SVG's `xMaxYMin slice`).

## How close is it

Measured in the real desktop window (same screen position, live SVG with animation stopped vs these images): mean absolute
pixel difference 0.2 to 0.3 percent on bare backdrop areas, never more than 7 of 255 levels in either theme.
The `.bd-fine` rule for phone widths (hides the rim) is not baked: the desktop window is at least 900 px wide, so it never applies.
