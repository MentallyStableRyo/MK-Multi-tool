/**
 * Generates `app-icon.png` — the 1024×1024 source that `tauri icon` expands
 * into every platform-specific format (.ico, .icns, and the PNG set).
 *
 * Written as a hand-rolled PNG encoder so the project has no image-processing
 * dependency: the icon is a few hundred lines of arithmetic, not a build tool.
 *
 *   node scripts/gen-icon.mjs
 */

import { deflateSync } from "node:zlib";
import { writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const SIZE = 1024;
const OUT = join(dirname(fileURLToPath(import.meta.url)), "..", "app-icon.png");

// ---------------------------------------------------------------- drawing

/** Signed distance to a rounded rectangle, negative inside. */
function roundedRectSdf(x, y, cx, cy, halfW, halfH, radius) {
  const dx = Math.abs(x - cx) - (halfW - radius);
  const dy = Math.abs(y - cy) - (halfH - radius);
  const outside = Math.hypot(Math.max(dx, 0), Math.max(dy, 0));
  return outside + Math.min(Math.max(dx, dy), 0) - radius;
}

/** Signed distance to a triangle, negative inside. */
function triangleSdf(px, py, a, b, c) {
  const edge = (p, q) => {
    const ex = q[0] - p[0];
    const ey = q[1] - p[1];
    const wx = px - p[0];
    const wy = py - p[1];
    const t = Math.min(1, Math.max(0, (wx * ex + wy * ey) / (ex * ex + ey * ey)));
    return Math.hypot(wx - ex * t, wy - ey * t);
  };

  // A point is inside when it falls on the same side of all three edges. The
  // comparison must accept either winding, since screen coordinates put y
  // downward and flip the sign of every cross product.
  const sign = (p, q) => (q[0] - p[0]) * (py - p[1]) - (q[1] - p[1]) * (px - p[0]);
  const s1 = sign(a, b);
  const s2 = sign(b, c);
  const s3 = sign(c, a);
  const inside = (s1 >= 0 && s2 >= 0 && s3 >= 0) || (s1 <= 0 && s2 <= 0 && s3 <= 0);

  const distance = Math.min(edge(a, b), edge(b, c), edge(c, a));
  return inside ? -distance : distance;
}

/** Antialiased coverage from a signed distance, over roughly one pixel. */
const coverage = (sdf) => Math.min(1, Math.max(0, 0.5 - sdf));

function mix(a, b, t) {
  return [
    a[0] + (b[0] - a[0]) * t,
    a[1] + (b[1] - a[1]) * t,
    a[2] + (b[2] - a[2]) * t,
  ];
}

/** Composite `src` over `dst` with alpha `alpha`, both premultiplied-free. */
function over(dst, src, alpha) {
  return mix(dst, src, alpha);
}

function render() {
  const pixels = Buffer.alloc(SIZE * SIZE * 4);

  const centre = SIZE / 2;
  const plateHalf = SIZE * 0.43;
  const plateRadius = SIZE * 0.21;

  // Accent gradient, matching --accent / --accent-dim in styles.css.
  const gradTop = [0x8a, 0x95, 0xff];
  const gradBottom = [0x3a, 0x42, 0x90];

  // Play triangle, cut by a vertical gap — the "slice".
  const triA = [SIZE * 0.40, SIZE * 0.30];
  const triB = [SIZE * 0.40, SIZE * 0.70];
  const triC = [SIZE * 0.745, SIZE * 0.50];
  const gapCentre = SIZE * 0.555;
  const gapHalf = SIZE * 0.028;

  for (let y = 0; y < SIZE; y += 1) {
    for (let x = 0; x < SIZE; x += 1) {
      const px = x + 0.5;
      const py = y + 0.5;

      const plate = coverage(roundedRectSdf(px, py, centre, centre, plateHalf, plateHalf, plateRadius));
      if (plate <= 0) {
        // Fully transparent outside the plate.
        continue;
      }

      let colour = mix(gradTop, gradBottom, py / SIZE);

      // Subtle top-left sheen so the plate reads as a physical surface.
      const sheen = Math.max(0, 1 - Math.hypot(px - SIZE * 0.3, py - SIZE * 0.22) / (SIZE * 0.55));
      colour = over(colour, [0xff, 0xff, 0xff], sheen * 0.14);

      const inTriangle = coverage(triangleSdf(px, py, triA, triB, triC));
      const inGap = coverage(Math.abs(px - gapCentre) - gapHalf);
      const mark = inTriangle * (1 - inGap);
      colour = over(colour, [0xff, 0xff, 0xff], mark);

      const offset = (y * SIZE + x) * 4;
      pixels[offset] = Math.round(colour[0]);
      pixels[offset + 1] = Math.round(colour[1]);
      pixels[offset + 2] = Math.round(colour[2]);
      pixels[offset + 3] = Math.round(plate * 255);
    }
  }

  return pixels;
}

// ------------------------------------------------------------ PNG encoding

const CRC_TABLE = (() => {
  const table = new Int32Array(256);
  for (let n = 0; n < 256; n += 1) {
    let c = n;
    for (let k = 0; k < 8; k += 1) {
      c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    }
    table[n] = c;
  }
  return table;
})();

function crc32(buffer) {
  let crc = -1;
  for (const byte of buffer) {
    crc = CRC_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  }
  return (crc ^ -1) >>> 0;
}

function chunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length);

  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));

  return Buffer.concat([length, body, crc]);
}

function encodePng(rgba, width, height) {
  // Filter type 0 (None) on every scanline: the image is smooth enough that
  // deflate handles it well, and it keeps this encoder trivially correct.
  const raw = Buffer.alloc(height * (width * 4 + 1));
  for (let y = 0; y < height; y += 1) {
    const src = y * width * 4;
    const dst = y * (width * 4 + 1);
    raw[dst] = 0;
    rgba.copy(raw, dst + 1, src, src + width * 4);
  }

  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // colour type: RGBA
  ihdr[10] = 0; // deflate
  ihdr[11] = 0; // adaptive filtering
  ihdr[12] = 0; // no interlace

  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

// -------------------------------------------------------------------- main

const png = encodePng(render(), SIZE, SIZE);
writeFileSync(OUT, png);
console.log(`✓ wrote ${OUT} (${SIZE}×${SIZE}, ${(png.length / 1024).toFixed(1)} KB)`);
console.log("  next: npx tauri icon app-icon.png");
