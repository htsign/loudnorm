// Regenerates assets/icon.ico, assets/icon.icns and assets/icon-256.png from assets/icon.svg and assets/icon-small.svg.
//
// Requires Node.js 18+ and the `playwright` npm package with its Chromium build. Install them outside the repository
// so that no package.json / node_modules lands in it, then point NODE_PATH at them:
//   npm install --prefix <dir> playwright && npx --prefix <dir> playwright install chromium
//   NODE_PATH=<dir>/node_modules node assets/make_icons.js
// Chromium renders the SVGs, so the anti-aliasing of the committed files depends on this renderer.
//
// Every .ico entry is a PNG. The 256 px entry is byte-identical to icon-256.png, which is what the window uses.
// Small sizes come from icon-small.svg (3 bars, legible at 16 px); larger ones come from icon.svg (4 bars).
// The .icns (macOS app bundle icon) holds PNG entries ic07-ic14 (128-1024 px, plus the 32 and 64 px entries of the
// 16 and 32 pt Retina sizes); ic11-ic14, the Retina entries, need macOS 10.8 or later. It follows the same rule for
// choosing the SVG. An image of the same size and SVG is rendered once and shared, so the 256 px entries of the .ico and
// the .icns are byte-identical to icon-256.png.
const { chromium } = require('playwright');
const fs = require('fs');
const path = require('path');

const assets = __dirname;
const jobs = /** @type {const} */ ([
  [16, 'icon-small.svg'],
  [20, 'icon-small.svg'],
  [24, 'icon-small.svg'],
  [32, 'icon-small.svg'],
  [40, 'icon.svg'],
  [48, 'icon.svg'],
  [64, 'icon.svg'],
  [256, 'icon.svg'],
]);
// Entries of the .icns: OSType, edge length in px and SVG. 256 and 512 px appear twice (1x and 2x of the next size down).
const icnsJobs = /** @type {const} */ ([
  ['ic11', 32, 'icon-small.svg'],
  ['ic12', 64, 'icon.svg'],
  ['ic07', 128, 'icon.svg'],
  ['ic13', 256, 'icon.svg'],
  ['ic08', 256, 'icon.svg'],
  ['ic14', 512, 'icon.svg'],
  ['ic09', 512, 'icon.svg'],
  ['ic10', 1024, 'icon.svg'],
]);

/**
 * Renders an SVG from the assets folder to a square, transparent PNG with Chromium.
 * @param {import('playwright').Browser} browser
 * @param {number} size Edge length in px.
 * @param {string} file SVG file name in the assets folder.
 * @returns {Promise<Buffer>}
 */
const render = async (browser, size, file) => {
  const svg = fs.readFileSync(path.join(assets, file), 'utf8');
  const uri = 'data:image/svg+xml;base64,' + Buffer.from(svg).toString('base64');
  const page = await browser.newPage({ viewport: { width: size, height: size }, deviceScaleFactor: 1 });
  await page.setContent(
    `<html><body style="margin:0;background:transparent"><img src="${uri}" width="${size}" height="${size}" style="display:block"></body></html>`,
  );
  await page.evaluate(() => document.querySelector('img').decode());
  const png = await page.screenshot({ omitBackground: true, clip: { x: 0, y: 0, width: size, height: size } });
  await page.close();
  return png;
};

/** Rendered PNGs by `${size}:${file}`, so that every size and SVG is rendered only once. */
const rendered = new Map();

/**
 * Same as render, but returns the Buffer of an earlier render for the same size and SVG.
 * @param {import('playwright').Browser} browser
 * @param {number} size Edge length in px.
 * @param {string} file SVG file name in the assets folder.
 * @returns {Promise<Buffer>}
 */
const renderOnce = async (browser, size, file) => {
  const key = `${size}:${file}`;
  const cached = rendered.get(key);
  if (cached != null) return cached;
  const png = await render(browser, size, file);
  rendered.set(key, png);
  return png;
};

/**
 * Packs PNG images into an .ico: ICONDIR + ICONDIRENTRY[] + image data. A width/height byte of 0 means 256.
 * @param {ReadonlyArray<readonly [number, Buffer]>} entries Pairs of edge length in px and PNG data.
 * @returns {Buffer}
 */
const packIco = entries => {
  const head = Buffer.alloc(6 + 16 * entries.length);
  head.writeUInt16LE(0, 0);
  head.writeUInt16LE(1, 2);
  head.writeUInt16LE(entries.length, 4);
  let offset = head.length;
  for (const [i, [size, png]] of entries.entries()) {
    const at = 6 + 16 * i;
    const dim = size >= 256 ? 0 : size;
    head.writeUInt8(dim, at);
    head.writeUInt8(dim, at + 1);
    head.writeUInt16LE(1, at + 4);
    head.writeUInt16LE(32, at + 6);
    head.writeUInt32LE(png.length, at + 8);
    head.writeUInt32LE(offset, at + 12);
    offset += png.length;
  }
  return Buffer.concat([head, ...entries.map(([, png]) => png)]);
};

/**
 * Packs PNG images into an .icns: the header "icns" + total length, then per image its OSType + length + PNG data.
 * All numbers are big-endian and every length includes its own 8-byte header.
 * @param {ReadonlyArray<readonly [string, Buffer]>} entries Pairs of 4-character OSType and PNG data.
 * @returns {Buffer}
 */
const packIcns = entries => {
  const blocks = entries.map(([type, png]) => {
    const head = Buffer.alloc(8);
    head.write(type, 0, 4, 'ascii');
    head.writeUInt32BE(8 + png.length, 4);
    return Buffer.concat([head, png]);
  });
  const total = 8 + blocks.reduce((sum, block) => sum + block.length, 0);
  const head = Buffer.alloc(8);
  head.write('icns', 0, 4, 'ascii');
  head.writeUInt32BE(total, 4);
  return Buffer.concat([head, ...blocks]);
};

(async () => {
  const browser = await chromium.launch();
  try {
    const entries = [];
    for (const [size, file] of jobs) entries.push([size, await renderOnce(browser, size, file)]);
    fs.writeFileSync(path.join(assets, 'icon.ico'), packIco(entries));
    fs.writeFileSync(path.join(assets, 'icon-256.png'), entries.find(([size]) => size === 256)[1]);

    const icnsEntries = [];
    for (const [type, size, file] of icnsJobs) icnsEntries.push([type, await renderOnce(browser, size, file)]);
    fs.writeFileSync(path.join(assets, 'icon.icns'), packIcns(icnsEntries));
  } finally {
    await browser.close();
  }
})().catch(e => {
  console.error(e);
  process.exit(1);
});
