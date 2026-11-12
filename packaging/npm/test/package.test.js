"use strict";

const assert = require("node:assert/strict");
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");

const packageRoot = path.resolve(__dirname, "..");
const pkg = require("../package.json");

test("package manifest matches the release contract", () => {
  assert.equal(pkg.name, "vowp");
  assert.equal(pkg.version, "0.3.0");
  assert.deepEqual(pkg.bin, { vowp: "bin/vowp.js" });
  assert.equal(pkg.license, "MIT");
  assert.equal(pkg.engines.node, ">=18");
  assert.deepEqual(pkg.os, ["linux", "darwin"]);
  assert.deepEqual(pkg.cpu, ["x64", "arm64"]);
  assert.equal(pkg.repository.url, "git+https://github.com/syver777/vowp.git");
  assert.equal(pkg.dependencies, undefined);
  assert.equal(pkg.optionalDependencies, undefined);
  assert.equal(pkg.scripts.postinstall, "node scripts/postinstall.js");
});

test("npm pack contains only the runtime files", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "vowp-npm-pack-"));
  try {
    const userconfig = path.join(root, "npmrc");
    const cache = path.join(root, "cache");
    fs.writeFileSync(userconfig, "");
    fs.mkdirSync(cache);

    const npm = process.platform === "win32" ? "npm.cmd" : "npm";
    const result = spawnSync(npm, ["pack", "--dry-run", "--json"], {
      cwd: packageRoot,
      encoding: "utf8",
      timeout: 120_000,
      env: {
        ...process.env,
        npm_config_userconfig: userconfig,
        npm_config_cache: cache,
        npm_config_audit: "false",
        npm_config_fund: "false",
        npm_config_update_notifier: "false",
      },
    });
    assert.equal(result.status, 0, result.stderr);

    let report;
    try {
      report = JSON.parse(result.stdout);
    } catch {
      assert.fail(`npm pack did not print JSON:\n${result.stdout}\n${result.stderr}`);
    }
    const files = report[0].files.map((file) => file.path).sort();
    assert.deepEqual(files, [
      "LICENSE",
      "README.md",
      "bin/vowp.js",
      "lib/download.js",
      "lib/install.js",
      "lib/tar.js",
      "lib/targets.js",
      "package.json",
      "scripts/postinstall.js",
    ]);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
