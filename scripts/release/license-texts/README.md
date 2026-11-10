# Reviewed licence texts

These are the full licence texts used by
`scripts/release/generate-third-party-licenses.py` to build the root
`THIRD-PARTY-LICENSES.md` bundle. Keep them byte-for-byte stable unless the
underlying licence text genuinely changes; the generator embeds them verbatim
and `--check` fails when the bundle is stale.

| File | Licence identifier | Source |
|---|---|---|
| `MIT.txt` | MIT | `anyhow` 1.0.104 `LICENSE-MIT` (the standard MIT text without a copyright line) |
| `Apache-2.0.txt` | Apache-2.0 | `anyhow` 1.0.104 `LICENSE-APACHE` |
| `Apache-2.0-WITH-LLVM-exception.txt` | Apache-2.0 WITH LLVM-exception | `linux-raw-sys` 0.12.1 `LICENSE-Apache-2.0_WITH_LLVM-exception` |
| `BSD-2-Clause.txt` | BSD-2-Clause | `zerocopy` 0.8.63 `LICENSE-BSD` |
| `BSD-3-Clause.txt` | BSD-3-Clause | Standard SPDX BSD-3-Clause template (no locked crate selects it today) |
| `ISC.txt` | ISC | Standard SPDX ISC template (no locked crate selects it today) |
| `Unicode-3.0.txt` | Unicode-3.0 | `unicode-ident` 1.0.27 `LICENSE-UNICODE` |
| `Zlib.txt` | Zlib | `foldhash` 0.2.0 `LICENSE` |

The per-package copyright notices in the generated bundle come from the
`LICENSE*`, `LICENCE*`, `COPYING*`, `COPYRIGHT*`, `UNLICENSE*` and `NOTICE*`
files shipped inside each crate's source package. The generator keeps the whole
paragraph that carries a notice, so notices that follow other text or wrap
across lines are retained verbatim.
