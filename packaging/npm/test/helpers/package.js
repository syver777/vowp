"use strict";

const fs = require("node:fs");
const path = require("node:path");

/**
 * Copy the files that npm would publish into a temporary package directory,
 * so lifecycle scripts run against a layout close to an installed package.
 */
function copyPackage(packageRoot, destination) {
  for (const name of ["package.json", "bin", "lib", "scripts"]) {
    fs.cpSync(path.join(packageRoot, name), path.join(destination, name), {
      recursive: true,
    });
  }
}

/** Environment without the variables this package reads, plus overrides. */
function cleanEnv(overrides = {}) {
  const env = { ...process.env };
  delete env.VOWP_SKIP_DOWNLOAD;
  delete env.VOWP_RELEASE_BASE_URL;
  delete env.NODE_AUTH_TOKEN;
  return { ...env, ...overrides };
}

module.exports = { cleanEnv, copyPackage };
