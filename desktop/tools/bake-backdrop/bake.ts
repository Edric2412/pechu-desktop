// Draws the page backdrop (the 3 blurred silk ribbons of public/js/backdrop.js) once per theme and saves each layer as an image,
// so the desktop window never has to compute the blur. Run: `bun install && bun run bake` in this folder (see README.md).
//   BAKE_WIDTH=1600  width of each image in pixels (height is 5/8 of it)
//   BAKE_QUALITY=80  WebP quality
//   BAKE_OUT=...     output folder (default ../../assets/backdrop)
import { parseHTML } from 'linkedom';
import { Resvg } from '@resvg/resvg-js';
import sharp from 'sharp';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

const root = resolve(import.meta.dir, '../../..'); // the repository root
const outDir = resolve(process.env.BAKE_OUT ?? join(root, 'desktop/assets/backdrop'));
const OUT_W = Number(process.env.BAKE_WIDTH ?? 1600);
const OUT_H = Math.round((OUT_W * 1000) / 1600);
const QUALITY = Number(process.env.BAKE_QUALITY ?? 80);
const W = 1600, H = 1000, MARGIN = 300; // the page's drawing area; the blur filters reach 300 units beyond it

// --- 1. The real drawing code, run against a fake document (public/js/backdrop.js is not copied, and not edited) ---
const { document } = parseHTML('<!doctype html><html><body></body></html>');
const source = readFileSync(join(root, 'public/js/backdrop.js'), 'utf8').replace('export function', 'function');
const { RIBBONS, ribbonSvg } = new Function('document', `${source}\nreturn { RIBBONS, ribbonSvg };`)(document) as {
  RIBBONS: { id: string }[];
  ribbonSvg: (r: { id: string }) => { outerHTML: string; querySelector(s: string): { setAttribute(k: string, v: string): void } };
};

// --- 2. The colours of each theme, read from the page's own stylesheets ---
function block(css: string, selector: string): Record<string, string> {
  const start = css.indexOf(`${selector} {`);
  if (start < 0) throw new Error(`no "${selector}" block`);
  const body = css.slice(css.indexOf('{', start) + 1, css.indexOf('}', start));
  const vars: Record<string, string> = {};
  for (const m of body.replace(/\/\*[\s\S]*?\*\//g, '').matchAll(/(--[\w-]+)\s*:\s*([^;]+);/g)) vars[m[1]] = m[2].trim();
  return vars;
}
const backdropCss = readFileSync(join(root, 'public/css/backdrop.css'), 'utf8');
const stylesCss = readFileSync(join(root, 'public/styles.css'), 'utf8');
const light = { ...block(stylesCss, ':root'), ...block(backdropCss, ':root') };
const dark = { ...light, ...block(stylesCss, ':root[data-theme="dark"]'), ...block(backdropCss, ':root[data-theme="dark"]') };
const themes = { light, dark };

const resolveVar = (vars: Record<string, string>, name: string, depth = 0): string => {
  if (depth > 5 || !(name in vars)) throw new Error(`unknown custom property ${name}`);
  return vars[name].replace(/var\((--[\w-]+)\)/g, (_, n) => resolveVar(vars, n, depth + 1));
};

// --- 3. Draw, replace var(--x), rasterise, crop to the 1600x1000 area, save ---
mkdirSync(outDir, { recursive: true });
const report: string[] = [];
for (const [theme, vars] of Object.entries(themes)) {
  for (const ribbon of RIBBONS) {
    const svg = ribbonSvg(ribbon);
    svg.querySelector('.bd-rimg').setAttribute('opacity', resolveVar(vars, '--bd-rim-k')); // css/backdrop.css: .bd-rimg { opacity: var(--bd-rim-k) }
    let markup = svg.outerHTML.replace(/var\((--[\w-]+)\)/g, (_, n) => resolveVar(vars, n));
    if (markup.includes('var(')) throw new Error('an unresolved var() is left');
    // draw a margin around the area as well, so blur from outside it reaches in, then cut the margin off
    markup = markup
      .replace('<svg ', '<svg xmlns="http://www.w3.org/2000/svg" ')
      .replace(/viewBox="[^"]*"/, `viewBox="${-MARGIN} ${-MARGIN} ${W + 2 * MARGIN} ${H + 2 * MARGIN}"`);
    const scale = OUT_W / W;
    const png = new Resvg(markup, { fitTo: { mode: 'width', value: Math.round((W + 2 * MARGIN) * scale) } }).render().asPng();
    const file = join(outDir, `${theme}-${ribbon.id}.webp`);
    const image = sharp(png).extract({ left: Math.round(MARGIN * scale), top: Math.round(MARGIN * scale), width: OUT_W, height: OUT_H });
    const bytes = await image.webp({ quality: QUALITY, alphaQuality: 100, effort: 6, smartSubsample: false }).toBuffer();
    writeFileSync(file, bytes);
    report.push(`${theme}-${ribbon.id}.webp  ${OUT_W}x${OUT_H}  ${(bytes.length / 1024).toFixed(0)} KB`);
  }
}
console.log(report.join('\n'));
