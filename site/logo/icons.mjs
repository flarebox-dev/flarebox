// SPDX-License-Identifier: Apache-2.0
import { readFile, writeFile } from 'node:fs/promises';
import sharp from 'sharp';

const site = new URL('../', import.meta.url);
const appIcon = await readFile(new URL('koidrive-bold-appicon.svg', import.meta.url));
const mark = await readFile(new URL('koidrive-bold-icon.svg', import.meta.url), 'utf8');
const wordmark = await readFile(new URL('wordmark.svg', import.meta.url), 'utf8');
const png = (size) => sharp(appIcon).resize(size, size).png().toBuffer();

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

await writeFile(new URL('favicon.svg', site), appIcon);
await writeFile(new URL('favicon.ico', site), ico(await Promise.all([16, 32].map(async (size) => ({ size, data: await png(size) })))));
for (const [name, size] of [['apple-touch-icon.png', 180], ['icon-192.png', 192], ['icon-512.png', 512]]) {
  await writeFile(new URL(name, site), await png(size));
}
const [vx, vy, vw, vh] = wordmark.match(/viewBox="([^"]+)"/)[1].split(' ').map(Number);
const wordHeight = 100;
const scale = wordHeight / vh;
const markSize = 200;
const gap = 40;
const width = markSize + gap + vw * scale;
const x = (1200 - width) / 2;
const wordBody = wordmark.replace(/^[\s\S]*?<svg[^>]*>/, '').replace(/<\/svg>\s*$/, '');
const markBody = mark.replace(/^[\s\S]*?<svg[^>]*>/, '').replace(/<\/svg>\s*$/, '');
const og = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 630" width="1200" height="630"><rect width="1200" height="630" fill="#030201"/>' +
  '<svg x="' + x + '" y="215" width="' + markSize + '" height="' + markSize + '" viewBox="0 0 512 512">' + markBody + '</svg>' +
  '<g transform="translate(' + (x + markSize + gap - vx * scale) + ' ' + (265 - vy * scale) + ') scale(' + scale + ')">' + wordBody + '</g></svg>\n';
await writeFile(new URL('og.png', site), await sharp(Buffer.from(og)).png().toBuffer());
console.log('Koidrive icons and og.png written to site/');
