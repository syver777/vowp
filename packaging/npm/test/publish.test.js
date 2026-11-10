"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");

const { cleanEnv } = require("./helpers/package");
const { buildRelease } = require("./helpers/release");
const { run } = require("./helpers/run");
const { startServer } = require("./helpers/server");

const packageRoot = path.resolve(__dirname, "..");
const { version } = require("../package.json");

function makeFakeEnvironment() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "vowp-npm-publish-"));
  const bin = path.join(root, "bin");
  fs.mkdirSync(bin);
  const npm = path.join(bin, "npm");
  fs.writeFileSync(
    npm,
    [
      "#!/bin/sh",
      'printf \'%s\\n\' "$*" >> "$VOWP_TEST_NPM_LOG"',
      'if [ -n "${npm_config_userconfig:-}" ] && [ -f "$npm_config_userconfig" ]; then',
      '  printf \'%s\\n\' "$npm_config_userconfig" >> "$VOWP_TEST_NPM_CONFIG_PATH"',
      '  cp "$npm_config_userconfig" "$VOWP_TEST_NPM_CONFIG_COPY"',
      "fi",
      'exit "${VOWP_TEST_NPM_EXIT:-0}"',
      "",
    ].join("\n"),
  );
  fs.chmodSync(npm, 0o755);
  return root;
}

function runPublish(root, server, args = [], token) {
  const env = cleanEnv({
    PATH: `${path.join(root, "bin")}:${process.env.PATH}`,
    VOWP_RELEASE_BASE_URL: server.url,
    VOWP_TEST_NPM_LOG: path.join(root, "npm.log"),
    VOWP_TEST_NPM_CONFIG_PATH: path.join(root, "npmrc.path"),
    VOWP_TEST_NPM_CONFIG_COPY: path.join(root, "npmrc.copy"),
  });
  if (token !== undefined) env.NODE_AUTH_TOKEN = token;
  return run(
    "sh",
    [path.join(packageRoot, "scripts", "publish.sh"), ...args],
    { cwd: packageRoot, env, timeout: 120_000 },
  );
}

function readLog(root) {
  const file = path.join(root, "npm.log");
  return fs.existsSync(file) ? fs.readFileSync(file, "utf8").trim() : "";
}

async function withRelease(files, fn) {
  const root = makeFakeEnvironment();
  const release = await startServer(files);
  try {
    return await fn(root, release);
  } finally {
    await release.close();
    fs.rmSync(root, { recursive: true, force: true });
  }
}

test("publish.sh verifies the release and runs npm publish --dry-run by default", async () => {
  await withRelease(buildRelease({ version }), async (root, release) => {
    const result = await runPublish(root, release);
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /verified vowp-0\.1\.0-x86_64-unknown-linux-gnu\.tar\.gz/);
    assert.match(result.stdout, /dry run only; nothing was published/);
    assert.equal(readLog(root), "publish --dry-run");
    assert.doesNotMatch(
      fs.readFileSync(path.join(root, "npmrc.copy"), "utf8"),
      /_authToken/,
    );
  });
});

test("publish.sh refuses when a release asset is missing", async () => {
  const archive = `vowp-${version}-x86_64-unknown-linux-gnu.tar.gz`;
  await withRelease(buildRelease({ version, omit: [archive] }), async (root, release) => {
    const result = await runPublish(root, release);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /release asset is not available/);
    assert.equal(readLog(root), "");
  });
});

test("publish.sh refuses when an archive does not match its checksum", async () => {
  await withRelease(
    buildRelease({ version, corruptTarget: "aarch64-apple-darwin" }),
    async (root, release) => {
      const result = await runPublish(root, release);
      assert.equal(result.status, 1);
      assert.match(result.stderr, /SHA256 mismatch for vowp-0\.1\.0-aarch64-apple-darwin/);
      assert.equal(readLog(root), "");
    },
  );
});

test("publish.sh --execute refuses without NODE_AUTH_TOKEN", async () => {
  await withRelease(buildRelease({ version }), async (root, release) => {
    const result = await runPublish(root, release, ["--execute"]);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /NODE_AUTH_TOKEN is not set/);
    assert.equal(readLog(root), "");
  });
});

test("publish.sh --execute uses a temporary npmrc and never prints the token", async () => {
  const token = "npm_test_token_do_not_print_123";
  await withRelease(buildRelease({ version }), async (root, release) => {
    const before = fs.readdirSync(packageRoot).sort();
    const result = await runPublish(root, release, ["--execute"], token);
    assert.equal(result.status, 0, result.stderr);
    assert.equal(readLog(root), "publish --access public");

    const configPath = fs
      .readFileSync(path.join(root, "npmrc.path"), "utf8")
      .trim();
    assert.equal(fs.existsSync(configPath), false, "temporary npmrc was not removed");
    assert.match(
      fs.readFileSync(path.join(root, "npmrc.copy"), "utf8"),
      /\/\/registry\.npmjs\.org\/:_authToken=npm_test_token_do_not_print_123/,
    );
    assert.equal(result.stdout.includes(token), false);
    assert.equal(result.stderr.includes(token), false);
    assert.deepEqual(fs.readdirSync(packageRoot).sort(), before);
  });
});
