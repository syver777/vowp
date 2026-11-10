"use strict";

const assert = require("node:assert/strict");
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");

const packageRoot = path.resolve(__dirname, "..");

function makeLauncher() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "vowp-npm-launcher-"));
  fs.mkdirSync(path.join(root, "bin"));
  fs.mkdirSync(path.join(root, "vendor"));
  fs.copyFileSync(
    path.join(packageRoot, "bin", "vowp.js"),
    path.join(root, "bin", "vowp.js"),
  );
  fs.cpSync(path.join(packageRoot, "lib"), path.join(root, "lib"), {
    recursive: true,
  });
  return root;
}

function writeFakeBinary(root, body) {
  const binary = path.join(root, "vendor", "vowp");
  fs.writeFileSync(binary, body);
  fs.chmodSync(binary, 0o755);
  return binary;
}

test("launcher passes arguments, stdio and the exit code through", () => {
  const root = makeLauncher();
  try {
    writeFakeBinary(
      root,
      [
        "#!/bin/sh",
        'i=1; for arg in "$@"; do printf \'arg%d:%s\\n\' "$i" "$arg"; i=$((i+1)); done',
        "printf 'to-stderr\\n' >&2",
        "exit 7",
        "",
      ].join("\n"),
    );
    const result = spawnSync(
      process.execPath,
      [path.join(root, "bin", "vowp.js"), "one two", "three"],
      { encoding: "utf8", timeout: 30_000 },
    );
    assert.equal(result.status, 7);
    assert.equal(result.stdout, "arg1:one two\narg2:three\n");
    assert.equal(result.stderr, "to-stderr\n");
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("launcher reports a missing binary with a rebuild hint", () => {
  const root = makeLauncher();
  try {
    const result = spawnSync(
      process.execPath,
      [path.join(root, "bin", "vowp.js")],
      { encoding: "utf8", timeout: 30_000 },
    );
    assert.equal(result.status, 1);
    assert.match(result.stderr, /prebuilt binary is not installed/);
    assert.match(result.stderr, /npm rebuild -g vowp/);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
