# Homebrew formula

`vowp.rb` is the formula source for a **`homebrew-vowp` tap**. It is a template:
`update.sh <version>` fills the version and the four SHA256 hashes from the
release's `SHA256SUMS` file. The formula is not in Homebrew core and this
repository does not create or push a tap.

## Create the tap (owner action)

Create a public GitHub repository named `homebrew-vowp` under the same account
as the main repository (`syver777/homebrew-vowp`). These are owner actions;
Homebrew is not installed in the build container, so the `brew` commands below
were not run here.

Fill the formula for the release first, from a VOWP checkout:

```sh
packaging/homebrew/update.sh 0.2.0
```

Then, in a local clone of the empty tap repository, copy the **filled**
formula:

```sh
mkdir -p Formula
cp /path/to/vowp/packaging/homebrew/vowp.rb Formula/vowp.rb
git add Formula/vowp.rb
git commit -m "vowp 0.2.0"
git push origin main
```

Do not copy `vowp.rb` before `update.sh` has run: it is either the raw
template with `__VOWP_VERSION__`/`__SHA256_*__` placeholders or the previous
release's formula, and will not install the current release.

Users then install with:

```sh
brew tap syver777/vowp
brew install vowp
```

The tap repository name must be `homebrew-vowp`; `brew tap syver777/vowp`
resolves it to `github.com/syver777/homebrew-vowp`.

## Update for a release

Publish the GitHub release first (the formula URLs point at release assets, and
assets of a draft release are not public). From this repository:

```sh
packaging/homebrew/update.sh 0.2.0
```

With no second argument the script downloads
`https://github.com/syver777/vowp/releases/download/v0.2.0/SHA256SUMS`. To use a
local file, or to write a copy of the formula instead of editing `vowp.rb`:

```sh
packaging/homebrew/update.sh 0.2.0 /path/to/SHA256SUMS /tmp/vowp.rb
```

The script rewrites the version line and the four platform `sha256` lines in
place, so it works both on the initial template and on a formula that a
previous release already filled. Copy the filled formula to the tap and commit
there; do not commit the unfilled template.

## What the formula does

- Selects the archive for macOS arm64/Intel and Linux arm64/Intel.
- Installs the `vowp` binary and, when present in the archive, `README.md` and
  `LICENSE` under the formula's `doc` directory.
- Runs `vowp --version` as the formula test.
- Requires no build tools; the archive is a prebuilt binary.
