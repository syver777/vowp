"use strict";

const zlib = require("node:zlib");

const BLOCK_SIZE = 512;
const MAX_ENTRY_BYTES = 512 * 1024 * 1024;
const MAX_TAR_BYTES = 512 * 1024 * 1024;

function readString(buffer, offset, length) {
  const slice = buffer.subarray(offset, offset + length);
  const end = slice.indexOf(0);
  return (end === -1 ? slice : slice.subarray(0, end)).toString("utf8");
}

function readOctal(buffer, offset, length, what) {
  const text = readString(buffer, offset, length).trim();
  if (text === "") return 0;
  if (!/^[0-7]+$/.test(text)) {
    throw new Error(`invalid tar archive: ${what} is not octal`);
  }
  return Number.parseInt(text, 8);
}

function verifyChecksum(header) {
  const stored = readOctal(header, 148, 8, "header checksum");
  let sum = 0;
  for (let i = 0; i < BLOCK_SIZE; i += 1) {
    sum += i >= 148 && i < 156 ? 0x20 : header[i];
  }
  if (stored !== sum) {
    throw new Error("invalid tar archive: header checksum mismatch");
  }
}

function isZeroBlock(block) {
  for (const byte of block) {
    if (byte !== 0) return false;
  }
  return true;
}

// Parse a POSIX pax extended header ("<length> <key>=<value>\n" records). The
// release archives only use the path and size records, but values are kept so
// the caller can apply the ones it understands.
function parsePax(data) {
  const entries = {};
  let offset = 0;
  while (offset < data.length) {
    const space = data.indexOf(0x20, offset);
    if (space === -1) break;
    const lengthText = data.subarray(offset, space).toString("ascii");
    if (!/^[0-9]+$/.test(lengthText)) {
      throw new Error("invalid tar archive: bad pax header length");
    }
    const length = Number.parseInt(lengthText, 10);
    if (length <= 0 || offset + length > data.length) {
      throw new Error("invalid tar archive: bad pax header record");
    }
    const record = data.subarray(space + 1, offset + length - 1).toString("utf8");
    const equals = record.indexOf("=");
    if (equals !== -1) {
      entries[record.slice(0, equals)] = record.slice(equals + 1);
    }
    offset += length;
  }
  return entries;
}

function safePath(name) {
  if (name.startsWith("/")) {
    throw new Error(`invalid tar archive: absolute path ${JSON.stringify(name)}`);
  }
  const parts = name.split("/").filter((part) => part !== "" && part !== ".");
  if (parts.some((part) => part === "..")) {
    throw new Error(`invalid tar archive: unsafe path ${JSON.stringify(name)}`);
  }
  if (parts.length === 0) {
    throw new Error("invalid tar archive: empty entry name");
  }
  return parts.join("/");
}

function joinName(prefix, name) {
  return prefix ? `${prefix}/${name}` : name;
}

/**
 * Extract one regular file from a gzipped tar archive.
 *
 * Only the requested entry is returned; nothing is written to disk. The
 * archive is validated (header checksums and safe entry names) even for entries
 * that are not the requested one.
 *
 * @param {Buffer} archive gzipped tar bytes
 * @param {string} wantedPath normalized path of the file to return
 * @returns {{data: Buffer, mode: number}}
 */
function extractFile(archive, wantedPath) {
  let tar;
  try {
    tar = zlib.gunzipSync(archive, { maxOutputLength: MAX_TAR_BYTES });
  } catch (error) {
    throw new Error(`invalid tar archive: ${error.message}`);
  }
  let offset = 0;
  let pendingName = null;
  let pendingPax = null;

  while (offset + BLOCK_SIZE <= tar.length) {
    const header = tar.subarray(offset, offset + BLOCK_SIZE);
    if (isZeroBlock(header)) break;
    verifyChecksum(header);

    const size = readOctal(header, 124, 12, "entry size");
    if (size > MAX_ENTRY_BYTES) {
      throw new Error(`invalid tar archive: entry is larger than ${MAX_ENTRY_BYTES} bytes`);
    }
    const type = header[156] === 0 ? "0" : String.fromCharCode(header[156]);
    const dataStart = offset + BLOCK_SIZE;
    const dataEnd = dataStart + size;
    if (dataEnd > tar.length) {
      throw new Error("invalid tar archive: truncated entry");
    }
    const data = tar.subarray(dataStart, dataEnd);

    if (type === "L") {
      pendingName = readString(data, 0, data.length);
    } else if (type === "x") {
      pendingPax = parsePax(data);
    } else if (type === "g") {
      // Global pax attributes do not affect the entries we need.
    } else {
      const paxPath = pendingPax && pendingPax.path ? pendingPax.path : null;
      const rawName = paxPath || pendingName || joinName(
        readString(header, 345, 155),
        readString(header, 0, 100),
      );
      const name = safePath(rawName);
      if (name === wantedPath) {
        if (type !== "0") {
          throw new Error(
            `invalid tar archive: ${wantedPath} is not a regular file`,
          );
        }
        return { data: Buffer.from(data), mode: readOctal(header, 100, 8, "mode") };
      }
      pendingName = null;
      pendingPax = null;
    }

    offset = dataStart + Math.ceil(size / BLOCK_SIZE) * BLOCK_SIZE;
  }

  throw new Error(`invalid tar archive: it does not contain ${wantedPath}`);
}

module.exports = { extractFile };
