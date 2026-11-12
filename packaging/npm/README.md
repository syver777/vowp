# vowp (npm)

The npm package for **VOWP** — a local-first supervisor for AI coding agents.
It installs the prebuilt `vowp` binary for your platform; Rust is not
required.

> **Status: early release (0.3.0).** The v0.3 command surface is complete, but
> VOWP is young and interfaces may still change. The source, documentation and
> issue tracker live at [github.com/syver777/vowp](https://github.com/syver777/vowp).

## Install

```sh
npm install -g vowp
vowp --version
```

Or run it without a global install:

```sh
npx vowp --version
```

Requires Node.js 18 or newer. `npm install` runs a postinstall step that
downloads the archive for the current platform and verifies it before use.

The package installs `vowp` only. The loopback web window is served by the
separate `vowp-daemon`; install it separately when you want the window (see the
[install guide](https://github.com/syver777/vowp/blob/main/docs/guides/install.md)).

## Supported platforms

| Platform | Architecture | Release target |
|---|---|---|
| Linux (glibc) | x86_64 | `x86_64-unknown-linux-gnu` |
| Linux (glibc) | arm64 | `aarch64-unknown-linux-gnu` |
| macOS | Intel | `x86_64-apple-darwin` |
| macOS | Apple silicon | `aarch64-apple-darwin` |

The Linux archives are built on Ubuntu 22.04 against glibc 2.35; older glibc
and musl systems such as Alpine are not supported. The macOS archives are not
code-signed or notarized. Windows is not supported. On any other platform the
postinstall step stops with an error that names the platform and suggests
building from source.

## How the binary is installed and verified

The package version is the release version. For version `X.Y.Z` the postinstall
step downloads exactly two files:

```text
https://github.com/syver777/vowp/releases/download/vX.Y.Z/vowp-X.Y.Z-<target>.tar.gz
https://github.com/syver777/vowp/releases/download/vX.Y.Z/vowp-X.Y.Z-<target>.tar.gz.sha256
```

Both are built by the repository's release workflow. The step:

1. computes the SHA-256 of the downloaded archive;
2. compares it with the hash in the `.sha256` file;
3. fails without installing anything when the hashes differ;
4. extracts only `vowp-X.Y.Z-<target>/vowp` from the archive, ignoring other
   entries and rejecting absolute or `..` paths;
5. installs it as `vendor/vowp` inside the package and marks it executable.

`bin/vowp.js` then runs that binary with the arguments and standard
input/output/error inherited from the caller, and exits with the binary's exit
code. The install step makes no other network calls, and the package has no
runtime dependencies beyond Node.js built-ins.

## Skip the download

Set `VOWP_SKIP_DOWNLOAD=1` to install the package without downloading the
binary:

```sh
VOWP_SKIP_DOWNLOAD=1 npm install -g vowp
```

The launcher will then report that the binary is missing until you reinstall
or run `npm rebuild -g vowp` (global install) / `npm rebuild vowp` (project
install) without the variable.

## Mirrors

Point the download at a mirror with `VOWP_RELEASE_BASE_URL`. The mirror must
serve the same release layout, with the version directory and the archive plus
its `.sha256` file:

```sh
VOWP_RELEASE_BASE_URL=https://mirror.example/vowp/releases/download \
  npm install -g vowp
```

That example resolves to
`https://mirror.example/vowp/releases/download/vX.Y.Z/vowp-X.Y.Z-<target>.tar.gz`.
HTTPS is required; plain HTTP is accepted only for loopback hosts, which is
useful for a local test server. The checksum is always verified, so a mirror
cannot substitute a different binary without changing the `.sha256` file too.

## Troubleshooting

Re-run the download after an interrupted install. For a global install:

```sh
npm rebuild -g vowp
```

For a package installed in a project:

```sh
npm rebuild vowp
```

If your npm configuration sets `ignore-scripts=true`, lifecycle scripts are
disabled for that install; enable them or run `scripts/postinstall.js` from the
installed package directory.

## License

MIT. See [LICENSE](https://github.com/syver777/vowp/blob/main/LICENSE).
