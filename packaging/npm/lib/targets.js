"use strict";

// The four targets that .github/workflows/release.yml builds. The keys are
// Node's process.platform and process.arch values; the values are the Rust
// target triples used in the release archive names.
const TARGETS = Object.freeze({
  "linux-x64": "x86_64-unknown-linux-gnu",
  "linux-arm64": "aarch64-unknown-linux-gnu",
  "darwin-x64": "x86_64-apple-darwin",
  "darwin-arm64": "aarch64-apple-darwin",
});

const SUPPORTED = Object.freeze(
  Object.entries(TARGETS).map(([id, target]) => {
    const [platform, arch] = id.split("-");
    return Object.freeze({ platform, arch, target });
  }),
);

/**
 * Return the release target triple for a Node platform/arch pair, or null when
 * the pair has no prebuilt archive.
 */
function targetFor(platform, arch) {
  return TARGETS[`${platform}-${arch}`] || null;
}

/**
 * True when Node is running against musl libc. The Linux release archives are
 * linked against glibc, so musl systems have no compatible archive.
 */
function isMusl(platform = process.platform) {
  if (platform !== "linux") return false;
  if (!process.report || typeof process.report.getReport !== "function") return false;
  try {
    const report = process.report.getReport();
    return Boolean(report && report.header && !report.header.glibcVersionRuntime);
  } catch {
    // Without a report there is no reliable libc answer; assume the common
    // glibc case and let the launcher report an exec failure if it is wrong.
    return false;
  }
}

/** Build the error text used for platforms without a prebuilt archive. */
function unsupportedMessage(platform, arch, options = {}) {
  const id = `${platform}/${arch}`;
  if (options.musl) {
    return (
      `unsupported platform: ${id} uses musl libc, but the Linux release ` +
      `archives are built against glibc (Ubuntu 22.04, glibc 2.35). ` +
      `Build from source with Cargo instead: https://github.com/syver777/vowp`
    );
  }
  const supported = SUPPORTED.map(
    ({ platform: p, arch: a, target }) => `${p}/${a} (${target})`,
  ).join(", ");
  return (
    `unsupported platform: ${id}; supported platforms: ${supported}. ` +
    `Build from source with Cargo instead: https://github.com/syver777/vowp`
  );
}

module.exports = { TARGETS, SUPPORTED, targetFor, isMusl, unsupportedMessage };
