// SPDX-License-Identifier: Apache-2.0
// Draws the cube mark (three faces in three strengths of the orange, split by a seam) and renders the icons in site/:
// favicon.svg (the 32 px drawing), favicon.ico (16 and 32 px), apple-touch-icon.png (180 px) and the manifest's 192
// and 512 px icons, on the paper. Each size is fitted to whole pixels, so 16 and 32 px need no hinting. It also prints
// the 28 px drawing the nav inlines in index.html. Run with sharp installed: node site/logo/icons.mjs
import { writeFile } from 'node:fs/promises';
import sharp from 'sharp';

const site = new URL('../', import.meta.url);
const PAPER = '#030201';
const TOP = '#fea47c';
const LEFT = '#f97c3d';
const RIGHT = '#dd6222';
const GAP_RATIO = 28 / 416;
const FILL = 416 / 512;
const SMALL_DIAGONAL = 1.125;

const fmt = (v) => String(Math.round(v * 100000) / 100000);

// The cube's width, rounded so the seam splits it into two whole halves.
function fit(W0) {
  const g = Math.max(1, Math.floor(GAP_RATIO * W0 + 0.5));
  return { W: (W0 - g) % 2 === 0 ? W0 : W0 + 1, g };
}

// The three faces of a cube W wide at (x0, y0), with a vertical seam g and a diagonal seam gdv.
function faces(W, g, gdv, x0, y0) {
  const a = W / 2, cx = x0 + a, cy = y0 + a;
  const hv = g / 2, hd = gdv / 2;
  return [
    [TOP, [[cx, cy - a], [cx + a - hd, cy - a / 2 - hd / 2], [cx, cy - hd], [cx - a + hd, cy - a / 2 - hd / 2]]],
    [LEFT, [[cx - a, cy - a / 2 + hd], [cx - hv, cy + hd - hv / 2], [cx - hv, cy + a - hv / 2], [cx - a, cy + a / 2]]],
    [RIGHT, [[cx + hv, cy + hd - hv / 2], [cx + a, cy - a / 2 + hd], [cx + a, cy + a / 2], [cx + hv, cy + a - hv / 2]]],
  ].map(([fill, pts]) => '<path d="M' + pts.map(([x, y]) => fmt(x) + ' ' + fmt(y)).join('L') + 'Z" fill="' + fill + '"/>').join('');
}

function icon(S, bg) {
  const { W, g } = fit(S === 512 ? 416 : Math.round(S * FILL));
  const x0 = Math.floor((S - W) / 2);
  const gdv = S === 512 ? Math.ceil((g * Math.sqrt(5)) / 2 / 4) * 4 : S <= 32 ? SMALL_DIAGONAL * g : Math.ceil((g * Math.sqrt(5)) / 2 / 0.5 - 1e-9) * 0.5;
  return '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ' + S + ' ' + S + '" width="' + S + '" height="' + S + '">' + (bg ? '<rect width="' + S + '" height="' + S + '" fill="' + bg + '"/>' : '') + faces(W, g, gdv, x0, x0) + '</svg>\n';
}

const png = (svg) => sharp(Buffer.from(svg)).png().toBuffer();

// An ICO file whose entries are PNGs.
function ico(images) {
  const header = Buffer.alloc(6 + 16 * images.length);
  header.writeUInt16LE(0, 0);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(images.length, 4);
  let offset = header.length;
  images.forEach(({ size, data }, i) => {
    const e = 6 + 16 * i;
    header.writeUInt8(size, e);
    header.writeUInt8(size, e + 1);
    header.writeUInt16LE(1, e + 4);
    header.writeUInt16LE(32, e + 6);
    header.writeUInt32LE(data.length, e + 8);
    header.writeUInt32LE(offset, e + 12);
    offset += data.length;
  });
  return Buffer.concat([header, ...images.map((i) => i.data)]);
}

await writeFile(new URL('favicon.svg', site), icon(32));
await writeFile(new URL('favicon.ico', site), ico(await Promise.all([16, 32].map(async (size) => ({ size, data: await png(icon(size)) })))));
await writeFile(new URL('apple-touch-icon.png', site), await png(icon(180, PAPER)));
await writeFile(new URL('icon-192.png', site), await png(icon(192, PAPER)));
await writeFile(new URL('icon-512.png', site), await png(icon(512, PAPER)));
console.log('icons written to site/');
console.log('nav cube, viewBox 0 0 28 28: ' + faces(28, 2, 2 * SMALL_DIAGONAL, 0, 0));
