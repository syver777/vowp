"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");

const { targetFor } = require("../lib/targets");
const { cleanEnv, copyPackage } = require("./helpers/package");
const { buildRelease } = require("./helpers/release");
const { run } = require("./helpers/run");
const { startServer } = require("./helpers/server");

const packageRoot = path.resolve(__dirname, "..");
const { version } = require("../package.json");
const target = targetFor(process.platform, process.arch);
const noTarget = target === null ? "no prebuilt archive for this test platform" : false;

function makePackage() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "vowp-npm-test-"));
  const dir = path.join(root, "package");
  fs.mkdirSync(dir);
  copyPackage(packageRoot, dir);
  return { root, dir };
}

function runPostinstall(packageDir, env, nodeArgs = []) {
  return run(
    process.execPath,
    [...nodeArgs, path.join(packageDir, "scripts", "postinstall.js")],
    {
      cwd: packageDir,
      env,
      timeout: 60_000,
    },
  );
}

test("postinstall downloads, verifies and installs the archive", { skip: noTarget }, async () => {
  const { root, dir } = makePackage();
  const release = await startServer(buildRelease({ version, targets: [target] }));
  try {
    const result = await runPostinstall(
      dir,
      cleanEnv({ VOWP_RELEASE_BASE_URL: release.url }),
    );
    assert.equal(result.status, 0, result.stderr);

    const binary = path.join(dir, "vendor", "vowp");
    assert.equal(
      fs.readFileSync(binary, "utf8"),
      `#!/bin/sh\nprintf 'vowp ${version}\\n'\n`,
    );
    const mode = fs.statSync(binary).mode & 0o777;
    assert.notEqual(mode & 0o111, 0, `installed mode is not executable: ${mode.toString(8)}`);

    assert.match(result.stdout, /verified SHA256 [0-9a-f]{64}/);
    assert.match(result.stdout, /installed .*vendor\/vowp/);
    assert.deepEqual(
      release.requests.map((request) => request.pathname).sort(),
      [
        `/v${version}/vowp-${version}-${target}.tar.gz`,
        `/v${version}/vowp-${version}-${target}.tar.gz.sha256`,
      ],
    );
  } finally {
    await release.close();
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("postinstall rejects a wrong checksum and installs nothing", { skip: noTarget }, async () => {
  const { root, dir } = makePackage();
  const release = await startServer(
    buildRelease({ version, targets: [target], corruptTarget: target }),
  );
  try {
    const result = await runPostinstall(
      dir,
      cleanEnv({ VOWP_RELEASE_BASE_URL: release.url }),
    );
    assert.equal(result.status, 1);
    assert.match(result.stderr, /vowp: error: SHA256 mismatch/);
    assert.equal(fs.existsSync(path.join(dir, "vendor", "vowp")), false);
  } finally {
    await release.close();
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("postinstall fails with a clear message on an unsupported platform", async () => {
  const { root, dir } = makePackage();
  try {
    const preload = path.join(root, "force-platform.js");
    fs.writeFileSync(
      preload,
      'Object.defineProperty(process, "platform", { value: "win32" });\n' +
        'Object.defineProperty(process, "arch", { value: "x64" });\n',
    );
    const result = await runPostinstall(
      dir,
      cleanEnv({ VOWP_RELEASE_BASE_URL: "http://127.0.0.1:1" }),
      ["--require", preload],
    );
    assert.equal(result.status, 1);
    assert.match(result.stderr, /vowp: error: unsupported platform: win32\/x64/);
    assert.match(result.stderr, /supported platforms: linux\/x64/);
    assert.equal(fs.existsSync(path.join(dir, "vendor", "vowp")), false);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("VOWP_SKIP_DOWNLOAD=1 makes no network request", async () => {
  const { root, dir } = makePackage();
  const release = await startServer(buildRelease({ version, targets: [target || "x86_64-unknown-linux-gnu"] }));
  try {
    const result = await runPostinstall(
      dir,
      cleanEnv({
        VOWP_RELEASE_BASE_URL: release.url,
        VOWP_SKIP_DOWNLOAD: "1",
      }),
    );
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /skipping the prebuilt binary download/);
    assert.equal(release.requests.length, 0);
    assert.equal(fs.existsSync(path.join(dir, "vendor", "vowp")), false);
  } finally {
    await release.close();
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("postinstall rejects an insecure release URL", { skip: noTarget }, async () => {
  const { root, dir } = makePackage();
  try {
    const result = await runPostinstall(
      dir,
      cleanEnv({ VOWP_RELEASE_BASE_URL: "http://mirror.example/vowp" }),
    );
    assert.equal(result.status, 1);
    assert.match(result.stderr, /refusing an insecure release URL/);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
