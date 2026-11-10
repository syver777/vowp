"use strict";

const zlib = require("node:zlib");

const BLOCK_SIZE = 512;

function writeOctal(buffer, offset, length, value) {
  const text = value.toString(8).padStart(length - 1, "0");
  if (text.length > length - 1) {
    throw new Error(`value ${value} does not fit in ${length} octal bytes`);
  }
  buffer.write(text, offset, length - 1, "ascii");
  buffer[offset + length - 1] = 0;
}

function makeEntry(entry) {
  const name = Buffer.from(entry.name, "utf8");
  if (name.length > 100) {
    throw new Error(`test tar entry name is too long: ${entry.name}`);
  }
  const data = Buffer.isBuffer(entry.data)
    ? entry.data
    : Buffer.from(entry.data ?? "", "utf8");
  const header = Buffer.alloc(BLOCK_SIZE, 0);
  name.copy(header, 0);
  writeOctal(header, 100, 8, entry.mode ?? 0o644);
  writeOctal(header, 108, 8, 0);
  writeOctal(header, 116, 8, 0);
  writeOctal(header, 124, 12, data.length);
  writeOctal(header, 136, 12, 0);
  header.fill(0x20, 148, 156);
  header.write(entry.type ?? "0", 156, 1, "ascii");
  header.write("ustar\0", 257, 6, "ascii");
  header.write("00", 263, 2, "ascii");
  let sum = 0;
  for (const byte of header) sum += byte;
  header.write(`${sum.toString(8).padStart(6, "0")}\0 `, 148, 8, "ascii");
  return { header, data };
}

/** Build a gzipped ustar archive for tests from {name, data, mode} entries. */
function makeTarGz(entries) {
  const chunks = [];
  for (const entry of entries) {
    const { header, data } = makeEntry(entry);
    chunks.push(header, data);
    const padding = (BLOCK_SIZE - (data.length % BLOCK_SIZE)) % BLOCK_SIZE;
    if (padding > 0) chunks.push(Buffer.alloc(padding));
  }
  chunks.push(Buffer.alloc(BLOCK_SIZE * 2));
  return zlib.gzipSync(Buffer.concat(chunks));
}

module.exports = { makeTarGz };
