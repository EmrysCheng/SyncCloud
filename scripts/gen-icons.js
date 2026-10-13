// 生成 SyncCloud 应用图标：icons/icon.ico + 32x32.png + 128x128.png
// 纯 Node（zlib 内置），像素级绘制：深蓝圆角底 + 白云 + 橙色同步双箭头
const zlib = require('zlib');
const fs = require('fs');
const path = require('path');

function crc32(buf) {
  let table = [];
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c >>> 0;
  }
  let crc = 0xffffffff;
  for (const b of buf) crc = table[(crc ^ b) & 0xff] ^ (crc >>> 8);
  return (crc ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const td = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(td));
  return Buffer.concat([len, td, crc]);
}

function makePng(size) {
  const px = new Uint8Array(size * size * 4);
  const set = (x, y, r, g, b, a = 255) => {
    if (x < 0 || y < 0 || x >= size || y >= size) return;
    const i = (y * size + x) * 4;
    px[i] = r; px[i + 1] = g; px[i + 2] = b; px[i + 3] = a;
  };
  const S = size / 32; // 相对 32 网格的缩放
  // 圆角矩形背景
  const R = 7 * S;
  for (let y = 0; y < size; y++)
    for (let x = 0; x < size; x++) {
      // 圆角判定：到四角的距离
      let dx = Math.max(R - x, x - (size - 1 - R), 0);
      let dy = Math.max(R - y, y - (size - 1 - R), 0);
      const inside = dx * dx + dy * dy <= R * R;
      if (inside) set(x, y, 30, 58, 138); // #1e3a8a 深蓝
    }
  // 云朵（三个圆 + 底部矩形），白色，位于上半部
  const cloud = (cx, cy, r) => {
    for (let y = -r; y <= r; y++)
      for (let x = -r; x <= r; x++)
        if (x * x + y * y <= r * r) set(cx + x, cy + y, 255, 255, 255);
  };
  const c1x = Math.round(11 * S), c1y = Math.round(13 * S), r1 = Math.round(4.5 * S);
  const c2x = Math.round(17 * S), c2y = Math.round(11 * S), r2 = Math.round(5.5 * S);
  const c3x = Math.round(23 * S), c3y = Math.round(14 * S), r3 = Math.round(4 * S);
  cloud(c1x, c1y, r1); cloud(c2x, c2y, r2); cloud(c3x, c3y, r3);
  for (let y = 0; y < Math.round(4 * S); y++)
    for (let x = c1x; x <= c3x; x++) set(x, c1y + y, 255, 255, 255);
  // 同步双箭头（橙色圆弧箭头简化为两段粗箭头），位于下半部
  const O = [249, 115, 22]; // #f97316
  const barY = Math.round(22 * S), barY2 = Math.round(26 * S), thick = Math.max(2, Math.round(1.8 * S));
  for (let x = Math.round(9 * S); x <= Math.round(21 * S); x++)
    for (let t = 0; t < thick; t++) { set(x, barY + t, ...O); set(x, barY2 + t, ...O); }
  // 右箭头尖（上行，指向右）
  for (let i = 0; i < Math.round(3 * S); i++)
    for (let t = -i; t <= i; t++) set(Math.round(21 * S) + i, barY + Math.floor(thick / 2) + t, ...O);
  // 左箭头尖（下行，指向左）
  for (let i = 0; i < Math.round(3 * S); i++)
    for (let t = -i; t <= i; t++) set(Math.round(9 * S) - i, barY2 + Math.floor(thick / 2) + t, ...O);

  // 编码 PNG
  const raw = Buffer.alloc((size * 4 + 1) * size);
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0; // filter none
    Buffer.from(px.buffer, y * size * 4, size * 4).copy(raw, y * (size * 4 + 1) + 1);
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; ihdr[9] = 6; // 8bit RGBA
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', zlib.deflateSync(raw, { level: 9 })),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

function makeIco(png32, png128) {
  const entries = [
    { size: 32, png: png32 },
    { size: 128, png: png128 },
  ];
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0);
  header.writeUInt16LE(1, 2); // type icon
  header.writeUInt16LE(entries.length, 4);
  const dir = Buffer.alloc(16 * entries.length);
  let offset = 6 + dir.length;
  entries.forEach((e, i) => {
    const base = i * 16;
    dir[base] = e.size === 256 ? 0 : e.size;
    dir[base + 1] = e.size === 256 ? 0 : e.size;
    dir.writeUInt16LE(1, base + 4); // planes
    dir.writeUInt16LE(32, base + 6); // bpp
    dir.writeUInt32LE(e.png.length, base + 8);
    dir.writeUInt32LE(offset, base + 12);
    offset += e.png.length;
  });
  return Buffer.concat([header, dir, ...entries.map(e => e.png)]);
}

const outDir = path.join(__dirname, '..', 'src-tauri', 'icons');
fs.mkdirSync(outDir, { recursive: true });
const png32 = makePng(32);
const png128 = makePng(128);
fs.writeFileSync(path.join(outDir, '32x32.png'), png32);
fs.writeFileSync(path.join(outDir, '128x128.png'), png128);
fs.writeFileSync(path.join(outDir, 'icon.ico'), makeIco(png32, png128));
console.log('icons generated:', fs.readdirSync(outDir));
