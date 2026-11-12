#!/usr/bin/env python3
"""Generate the third-party licence notice bundle for release archives.

The dependency index is built from
``cargo metadata --format-version 1 --locked``. The full licence texts are the
reviewed files in ``scripts/release/license-texts/``. Copyright notices are
read from the ``LICENSE*``, ``LICENCE*``, ``COPYING*``, ``COPYRIGHT*``,
``UNLICENSE*`` and ``NOTICE*`` files shipped inside each crate's source
package, including nested vendored directories but excluding ``src``, tests,
examples and build output.

Usage:
    scripts/release/generate-third-party-licenses.py
    scripts/release/generate-third-party-licenses.py --check

The default writes the repository root ``THIRD-PARTY-LICENSES.md``. ``--check``
exits 0 when that file already matches a fresh generation, and 1 when it is
stale or when a source package, licence file or unrecognised copyright notice
cannot be handled. It exits 2 when cargo metadata cannot be read.

The only tracked file it writes is ``THIRD-PARTY-LICENSES.md``. The script
removes its own ``__pycache__`` directory because the repository's
script-safety test reads every file under ``scripts/`` as UTF-8 text.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path


def _remove_bytecode_cache() -> None:
    """Keep binary .pyc files out of scripts/, which the tmux safety test reads."""
    cache = Path(__file__).resolve().parent / "__pycache__"
    if cache.is_dir():
        shutil.rmtree(cache, ignore_errors=True)


_remove_bytecode_cache()

ROOT = Path(__file__).resolve().parents[2]
OUTPUT = ROOT / "THIRD-PARTY-LICENSES.md"
TEXT_DIR = Path(__file__).resolve().parent / "license-texts"

# Keep this order: it is the order of the full-text sections in the bundle.
LICENCE_TEXTS = (
    ("MIT", "MIT.txt", "MIT License"),
    ("MIT-0", "MIT-0.txt", "MIT No Attribution"),
    ("Apache-2.0", "Apache-2.0.txt", "Apache License 2.0"),
    (
        "Apache-2.0 WITH LLVM-exception",
        "Apache-2.0-WITH-LLVM-exception.txt",
        "Apache License 2.0 with LLVM exception",
    ),
    ("BSD-2-Clause", "BSD-2-Clause.txt", "BSD 2-Clause License"),
    ("BSD-3-Clause", "BSD-3-Clause.txt", "BSD 3-Clause License"),
    ("ISC", "ISC.txt", "ISC License"),
    ("Unicode-3.0", "Unicode-3.0.txt", "Unicode License V3"),
    ("Zlib", "Zlib.txt", "Zlib License"),
)
TEXT_BY_ID = {
    licence: (filename, heading) for licence, filename, heading in LICENCE_TEXTS
}

LICENCE_FILE_PREFIXES = (
    "LICENSE",
    "LICENCE",
    "COPYING",
    "COPYRIGHT",
    "UNLICENSE",
    "NOTICE",
)
# Do not treat licence files inside source, test or build trees as package
# notices. Vendored licence directories such as libsqlite3-sys/sqlcipher are
# deliberately kept.
SKIPPED_NOTICE_DIRS = {
    ".cargo",
    ".git",
    "benches",
    "examples",
    "fuzz",
    "src",
    "target",
    "tests",
}

# Lines that are part of the licence boilerplate rather than a real notice.
NOTICE_PLACEHOLDERS = (
    "[yyyy]",
    "[year]",
    "{yyyy}",
    "[name",
    "{name",
    "<year>",
    "name of copyright owner",
    "fullname",
)
NOTICE_BOILERPLATE_PREFIXES = (
    "and ",
    "convention",
    "holders",
    "in ",
    "interest",
    "is ",
    "law",
    "license",
    "notice",
    "or ",
    "owner",
    "protection",
    "rights",
    "shall",
    "statement",
    "subsisting",
    "to ",
    "treaty",
)
EMAIL_RE = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")
ANGLE_EMAIL_RE = re.compile(r"\s*<[^<>]*@[^<>]*>")
# A copyright notice can follow other text on the line, wrap across lines and
# use either the word or the sign. The negative lookbehind avoids matching
# words such as "copyrightable".
MARKER_RE = re.compile(r"(?<![A-Za-z])(?:Copyright\b(?:\s*©)?|©)")


def allowed_text_id(identifier: str) -> bool:
    if identifier in ("MIT", "MIT-0", "Apache-2.0", "ISC", "Zlib"):
        return True
    if identifier in ("Apache-2.0 WITH LLVM-exception", "Unicode-3.0"):
        return True
    return identifier.startswith("BSD-") or identifier.startswith("Unicode-")


def expression_identifiers(expression: str) -> list[str]:
    """Return the identifiers named by a Cargo licence expression."""
    expression = expression.replace("/", " OR ")
    tokens = re.findall(
        r"\(|\)|\bAND\b|\bOR\b|\bWITH\b|[A-Za-z0-9.+-]+", expression
    )
    result: list[str] = []
    index = 0
    while index < len(tokens):
        token = tokens[index]
        if token in ("(", ")", "AND", "OR"):
            index += 1
            continue
        if token == "WITH":
            index += 1
            continue
        if index + 2 < len(tokens) and tokens[index + 1] == "WITH":
            result.append(f"{token} WITH {tokens[index + 2]}")
            index += 3
        else:
            result.append(token)
            index += 1
    return result


def run_metadata() -> dict:
    command = [
        "cargo",
        "metadata",
        "--format-version",
        "1",
        "--locked",
        "--manifest-path",
        str(ROOT / "Cargo.toml"),
    ]
    result = subprocess.run(
        command,
        cwd=ROOT,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode != 0:
        sys.stderr.write(result.stderr)
        raise SystemExit(2)
    return json.loads(result.stdout)


def dependency_rows(metadata: dict) -> list[dict]:
    rows: list[dict] = []
    for package in metadata.get("packages", []):
        if package.get("source") is None:
            continue
        name = package.get("name", "?")
        version = package.get("version", "?")
        expression = package.get("license") or ""
        identifiers = [
            identifier
            for identifier in expression_identifiers(expression)
            if allowed_text_id(identifier)
        ]
        # Preserve LICENCE_TEXTS order and remove duplicates.
        identifiers = [
            licence
            for licence, _filename, _heading in LICENCE_TEXTS
            if licence in identifiers
        ]
        if not identifiers:
            raise SystemExit(
                f"{name} {version} has no allowed licence identifier in {expression!r}"
            )
        for identifier in identifiers:
            if identifier not in TEXT_BY_ID:
                raise SystemExit(
                    f"{name} {version} uses {identifier!r}, but no reviewed "
                    f"licence text exists in {TEXT_DIR.relative_to(ROOT)}"
                )
        rows.append(
            {
                "name": name,
                "version": version,
                "license": expression,
                "repository": package.get("repository") or "",
                "identifiers": identifiers,
                "manifest_dir": Path(package["manifest_path"]).parent,
            }
        )
    rows.sort(key=lambda row: (row["name"].lower(), row["version"]))
    return rows


def licence_files(package_dir: Path) -> list[Path]:
    if not package_dir.is_dir():
        raise SystemExit(
            f"source package is missing: {package_dir}\n"
            "run `cargo fetch --locked` before generating the licence bundle"
        )
    files = []
    for root, dirs, names in os.walk(package_dir):
        dirs[:] = sorted(
            name for name in dirs if name not in SKIPPED_NOTICE_DIRS
        )
        for name in sorted(names):
            if name.upper().startswith(LICENCE_FILE_PREFIXES):
                files.append(Path(root) / name)
    return files


def strip_email(text: str) -> str:
    # Keep the holder name and drop the contact address so the public audit
    # scanner does not treat a third-party copyright line as a leaked address.
    text = ANGLE_EMAIL_RE.sub("", text)
    return EMAIL_RE.sub("[email omitted]", text)


def paragraphs(text: str) -> list[str]:
    """Return whitespace-normalised paragraphs, joining wrapped notice lines."""
    result: list[str] = []
    for block in re.split(r"\n\s*\n", text):
        joined = " ".join(
            " ".join(line.strip() for line in block.splitlines()).split()
        )
        if joined:
            result.append(joined)
    return result


def notice_status(candidate: str) -> str:
    """Classify text starting at a Copyright/© marker.

    ``notice`` means it names a real holder (usually with a year),
    ``boilerplate`` and ``placeholder`` are licence text or templates, and
    ``unknown`` makes the generator fail instead of silently dropping a notice.
    """
    lower = candidate.lower()
    if any(token in lower for token in NOTICE_PLACEHOLDERS):
        return "placeholder"
    rest = candidate.split(" ", 1)[1].strip() if " " in candidate else ""
    rest_lower = rest.lower()
    if rest_lower.startswith(NOTICE_BOILERPLATE_PREFIXES):
        return "boilerplate"
    probe = re.sub(r"^(?:\(c\)|©)\s*", "", rest, flags=re.IGNORECASE)
    first = probe.split()[0] if probe.split() else ""
    if len(first) > 1 and first.isupper():
        return "boilerplate"
    if re.search(r"\b(19|20)\d{2}\b", rest):
        return "notice"
    if re.match(r"^(?:\(c\)\s*|©\s*)?[A-Z]", rest):
        return "notice"
    return "unknown"


def paragraph_notice_status(paragraph: str) -> list[str]:
    markers = list(MARKER_RE.finditer(paragraph))
    statuses: list[str] = []
    for index, marker in enumerate(markers):
        end = markers[index + 1].start() if index + 1 < len(markers) else len(paragraph)
        statuses.append(notice_status(paragraph[marker.start() : end].strip()))
    return statuses


def copyright_notices(paths: list[Path]) -> list[str]:
    notices: list[str] = []
    for path in paths:
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError as error:
            raise SystemExit(f"cannot read licence file {path}: {error}") from error
        for paragraph in paragraphs(text):
            statuses = paragraph_notice_status(paragraph)
            if "unknown" in statuses:
                raise SystemExit(
                    f"unrecognised copyright notice in {path}: {paragraph[:200]!r}"
                )
            if "notice" not in statuses:
                continue
            notice = strip_email(paragraph)
            if notice not in notices:
                notices.append(notice)
    return notices


def verify_notice_coverage(rows: list[dict], bundle: str) -> list[str]:
    """Fail when a notice shipped by a crate is absent from the bundle."""
    problems: list[str] = []
    for row in rows:
        for path in licence_files(row["manifest_dir"]):
            try:
                text = path.read_text(encoding="utf-8", errors="replace")
            except OSError as error:
                problems.append(f"{path}: cannot read licence file: {error}")
                continue
            for paragraph in paragraphs(text):
                statuses = paragraph_notice_status(paragraph)
                if "unknown" in statuses:
                    problems.append(
                        f"{path}: unrecognised copyright notice: {paragraph[:160]!r}"
                    )
                elif "notice" in statuses:
                    notice = strip_email(paragraph)
                    if notice not in bundle:
                        problems.append(
                            f"{path}: copyright notice missing from bundle: "
                            f"{notice[:160]!r}"
                        )
    return problems


def markdown_cell(value: str) -> str:
    return value.replace("|", "\\|")


def read_licence_text(filename: str) -> str:
    path = TEXT_DIR / filename
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        raise SystemExit(f"cannot read reviewed licence text {path}: {error}") from error
    if not text.strip():
        raise SystemExit(f"reviewed licence text is empty: {path}")
    return text.strip()


def build_bundle(rows: list[dict]) -> str:
    used: list[str] = []
    for row in rows:
        for identifier in row["identifiers"]:
            if identifier not in used:
                used.append(identifier)
    used.sort(key=lambda identifier: [licence for licence, _f, _h in LICENCE_TEXTS].index(identifier))

    lines = [
        "# Third-party licences",
        "",
        "VOWP release binaries include the Rust crates listed below. This notice",
        "bundle is generated from `cargo metadata --format-version 1 --locked` by",
        "`scripts/release/generate-third-party-licenses.py`; do not edit it by hand.",
        "The release workflow checks it is current and copies it into every archive as",
        "`THIRD-PARTY-LICENSES.md`.",
        "",
        "The locked dependency expressions use the allowed licence identifiers "
        + ", ".join(f"`{identifier}`" for identifier in used)
        + ".",
        "",
        "The full texts below cover every licence identifier used by this locked",
        "tree and the other reviewed texts in the audit's allowed set. A locked",
        "identifier without a reviewed text in `scripts/release/license-texts/`",
        "makes the generator fail instead of shipping an incomplete bundle.",
        "",
        "## Dependency index",
        "",
        "| Package | Version | Licence | Repository |",
        "|---|---|---|---|",
    ]
    for row in rows:
        lines.append(
            f"| `{row['name']}` | {row['version']} | "
            f"{markdown_cell(row['license'])} | {markdown_cell(row['repository'])} |"
        )

    lines.extend(["", "## Full licence texts", ""])
    for identifier, filename, heading in LICENCE_TEXTS:
        text = read_licence_text(filename)
        lines.extend(
            [
                f"### {heading}",
                "",
                f"SPDX identifier: `{identifier}`",
                "",
            ]
        )
        if identifier not in used:
            lines.extend(
                [
                    "This text is included for the audit's allowed set; no crate in",
                    "this locked tree selects it today.",
                    "",
                ]
            )
        lines.extend(
            [
                "```text",
                text,
                "```",
                "",
            ]
        )

    lines.extend(
        [
            "## Copyright notices",
            "",
            "These are the licence-file paragraphs that carry a copyright notice,",
            "including notices that follow other text or wrap across lines. A missing",
            "notice means the package does not ship a separate copyright notice; its",
            "repository and licence expression are recorded above.",
            "",
        ]
    )
    for row in rows:
        files = licence_files(row["manifest_dir"])
        notices = copyright_notices(files)
        lines.append(f"### `{row['name']}` {row['version']}")
        lines.append("")
        lines.append(f"- Licence: {row['license']}")
        lines.append(
            "- Repository: " + (row["repository"] if row["repository"] else "not declared")
        )
        if files:
            lines.append(
                "- Licence files: "
                + ", ".join(
                    str(path.relative_to(row["manifest_dir"])) for path in files
                )
            )
        else:
            lines.append("- Licence files: none in the crate package")
        if notices:
            for notice in notices:
                lines.append(f"- {notice}")
        else:
            lines.append(
                "- Copyright: no separate copyright notice in the shipped licence files"
            )
        lines.append("")

    return "\n".join(lines).rstrip() + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail if THIRD-PARTY-LICENSES.md is not the current generated output",
    )
    args = parser.parse_args()

    metadata = run_metadata()
    rows = dependency_rows(metadata)
    bundle = build_bundle(rows)

    problems = verify_notice_coverage(rows, bundle)
    if problems:
        sys.stderr.write("third-party licence bundle is incomplete:\n")
        for problem in problems[:20]:
            sys.stderr.write(f"  - {problem}\n")
        if len(problems) > 20:
            sys.stderr.write(f"  - ... and {len(problems) - 20} more\n")
        return 1

    if args.check:
        try:
            current = OUTPUT.read_text(encoding="utf-8")
        except OSError as error:
            sys.stderr.write(f"missing {OUTPUT.relative_to(ROOT)}: {error}\n")
            return 1
        if current != bundle:
            sys.stderr.write(
                f"{OUTPUT.relative_to(ROOT)} is stale; run "
                "scripts/release/generate-third-party-licenses.py\n"
            )
            return 1
        print(
            f"{OUTPUT.relative_to(ROOT)} is current "
            f"({len(rows)} packages, {len(LICENCE_TEXTS)} licence texts)"
        )
        return 0

    OUTPUT.write_text(bundle, encoding="utf-8")
    print(
        f"wrote {OUTPUT.relative_to(ROOT)} "
        f"({len(rows)} packages, {len(LICENCE_TEXTS)} licence texts)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
