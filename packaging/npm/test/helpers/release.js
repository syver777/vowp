"use strict";

const crypto = require("node:crypto");

const { makeTarGz } = require("./tar");

const ALL_TARGETS = [
  "x86_64-unknown-linux-gnu",
  "aarch64-unknown-linux-gnu",
  "x86_64-apple-darwin",
  "aarch64-apple-darwin",
];

function binaryFor(version) {
  return Buffer.from(`#!/bin/sh\nprintf 'vowp ${version}\\n'\n`, "utf8");
}

function archiveFor(version, target) {
  const root = `vowp-${version}-${target}`;
  return makeTarGz([
    { name: `${root}/vowp`, data: binaryFor(version), mode: 0o755 },
    { name: `${root}/LICENSE`, data: "MIT\n" },
    { name: `${root}/README.md`, data: "VOWP test release\n" },
  ]);
}

function sha256(buffer) {
  return crypto.createHash("sha256").update(buffer).digest("hex");
}

/**
 * Build the URL-path -> bytes map for a fake release tree.
 *
 * `corruptTarget` replaces that target's archive with different bytes after
 * the checksums are written, so verification must fail. `omit` removes named
 * files from the tree.
 */
function buildRelease(options) {
  const {
    version,
    targets = ALL_TARGETS,
    corruptTarget = null,
    omit = [],
  } = options;
  const files = new Map();
  const sumLines = [];

  for (const target of targets) {
    const name = `vowp-${version}-${target}.tar.gz`;
    const bytes = archiveFor(version, target);
    const hash = sha256(bytes);
    if (!omit.includes(name)) {
      files.set(`/v${version}/${name}`, bytes);
    }
    if (!omit.includes(`${name}.sha256`)) {
      files.set(
        `/v${version}/${name}.sha256`,
        Buffer.from(`${hash}  ${name}\n`, "utf8"),
      );
    }
    sumLines.push(`${hash}  ${name}`);
  }

  files.set(
    `/v${version}/SHA256SUMS`,
    Buffer.from(`${sumLines.join("\n")}\n`, "utf8"),
  );

  if (corruptTarget !== null) {
    const name = `vowp-${version}-${corruptTarget}.tar.gz`;
    const original = files.get(`/v${version}/${name}`);
    if (original) {
      files.set(
        `/v${version}/${name}`,
        Buffer.concat([original, Buffer.from("corrupted", "utf8")]),
      );
    }
  }

  return files;
}

module.exports = { ALL_TARGETS, archiveFor, binaryFor, buildRelease, sha256 };
