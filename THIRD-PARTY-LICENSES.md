# Third-party licences

VOWP release binaries include the Rust crates listed below. This notice
bundle is generated from `cargo metadata --format-version 1 --locked` by
`scripts/release/generate-third-party-licenses.py`; do not edit it by hand.
The release workflow checks it is current and copies it into every archive as
`THIRD-PARTY-LICENSES.md`.

The locked dependency expressions use the allowed licence identifiers `MIT`, `MIT-0`, `Apache-2.0`, `Apache-2.0 WITH LLVM-exception`, `BSD-2-Clause`, `BSD-3-Clause`, `Unicode-3.0`, `Zlib`.

The full texts below cover every licence identifier used by this locked
tree and the other reviewed texts in the audit's allowed set. A locked
identifier without a reviewed text in `scripts/release/license-texts/`
makes the generator fail instead of shipping an incomplete bundle.

## Dependency index

| Package | Version | Licence | Repository |
|---|---|---|---|
| `ahash` | 0.8.12 | MIT OR Apache-2.0 | https://github.com/tkaitchuck/ahash |
| `aho-corasick` | 1.1.5 | Unlicense OR MIT | https://github.com/BurntSushi/aho-corasick |
| `allocator-api2` | 0.2.21 | MIT OR Apache-2.0 | https://github.com/zakarumych/allocator-api2 |
| `android_system_properties` | 0.1.6 | MIT OR Apache-2.0 | https://github.com/nical/android_system_properties |
| `anstream` | 1.0.0 | MIT OR Apache-2.0 | https://github.com/rust-cli/anstyle.git |
| `anstyle` | 1.0.14 | MIT OR Apache-2.0 | https://github.com/rust-cli/anstyle.git |
| `anstyle-parse` | 1.0.0 | MIT OR Apache-2.0 | https://github.com/rust-cli/anstyle.git |
| `anstyle-query` | 1.1.5 | MIT OR Apache-2.0 | https://github.com/rust-cli/anstyle.git |
| `anstyle-wincon` | 3.0.11 | MIT OR Apache-2.0 | https://github.com/rust-cli/anstyle.git |
| `anyhow` | 1.0.104 | MIT OR Apache-2.0 | https://github.com/dtolnay/anyhow |
| `atomic-waker` | 1.1.2 | Apache-2.0 OR MIT | https://github.com/smol-rs/atomic-waker |
| `autocfg` | 1.5.1 | Apache-2.0 OR MIT | https://github.com/cuviper/autocfg |
| `axum` | 0.8.9 | MIT | https://github.com/tokio-rs/axum |
| `axum-core` | 0.5.6 | MIT | https://github.com/tokio-rs/axum |
| `bit-set` | 0.8.0 | Apache-2.0 OR MIT | https://github.com/contain-rs/bit-set |
| `bit-vec` | 0.8.0 | Apache-2.0 OR MIT | https://github.com/contain-rs/bit-vec |
| `bitflags` | 2.13.2 | MIT OR Apache-2.0 | https://github.com/bitflags/bitflags |
| `block-buffer` | 0.12.1 | MIT OR Apache-2.0 | https://github.com/RustCrypto/utils |
| `borrow-or-share` | 0.2.4 | MIT-0 | https://github.com/yescallop/borrow-or-share |
| `bstr` | 1.13.1 | MIT OR Apache-2.0 | https://github.com/BurntSushi/bstr |
| `bumpalo` | 3.20.3 | MIT OR Apache-2.0 | https://github.com/fitzgen/bumpalo |
| `bytecount` | 0.6.9 | Apache-2.0/MIT | https://github.com/llogiq/bytecount |
| `bytes` | 1.12.1 | MIT | https://github.com/tokio-rs/bytes |
| `cc` | 1.11.1 | MIT OR Apache-2.0 | https://github.com/rust-lang/cc-rs |
| `cfg-if` | 1.0.5 | MIT OR Apache-2.0 | https://github.com/rust-lang/cfg-if |
| `chacha20` | 0.10.2 | MIT OR Apache-2.0 | https://github.com/RustCrypto/stream-ciphers |
| `chrono` | 0.4.45 | MIT OR Apache-2.0 | https://github.com/chronotope/chrono |
| `clap` | 4.6.7 | MIT OR Apache-2.0 | https://github.com/clap-rs/clap |
| `clap_builder` | 4.6.7 | MIT OR Apache-2.0 | https://github.com/clap-rs/clap |
| `clap_derive` | 4.6.7 | MIT OR Apache-2.0 | https://github.com/clap-rs/clap |
| `clap_lex` | 1.1.1 | MIT OR Apache-2.0 | https://github.com/clap-rs/clap |
| `colorchoice` | 1.0.5 | MIT OR Apache-2.0 | https://github.com/rust-cli/anstyle.git |
| `const-oid` | 0.10.2 | Apache-2.0 OR MIT | https://github.com/RustCrypto/formats |
| `core-foundation-sys` | 0.8.7 | MIT OR Apache-2.0 | https://github.com/servo/core-foundation-rs |
| `cpufeatures` | 0.3.1 | MIT OR Apache-2.0 | https://github.com/RustCrypto/utils |
| `cron` | 0.17.0 | MIT OR Apache-2.0 | https://github.com/zslayton/cron |
| `crypto-common` | 0.2.2 | MIT OR Apache-2.0 | https://github.com/RustCrypto/traits |
| `data-encoding` | 2.11.1 | MIT | https://github.com/ia0/data-encoding |
| `deranged` | 0.5.9 | MIT OR Apache-2.0 | https://github.com/jhpratt/deranged |
| `diff` | 0.1.13 | MIT OR Apache-2.0 | https://github.com/utkarshkukreti/diff.rs |
| `digest` | 0.11.3 | MIT OR Apache-2.0 | https://github.com/RustCrypto/traits |
| `dyn-clone` | 1.0.20 | MIT OR Apache-2.0 | https://github.com/dtolnay/dyn-clone |
| `email_address` | 0.2.9 | MIT | https://github.com/johnstonskj/rust-email_address.git |
| `equivalent` | 1.0.3 | Apache-2.0 OR MIT | https://github.com/indexmap-rs/equivalent |
| `errno` | 0.3.14 | MIT OR Apache-2.0 | https://github.com/lambda-fairy/rust-errno |
| `fallible-iterator` | 0.3.0 | MIT/Apache-2.0 | https://github.com/sfackler/rust-fallible-iterator |
| `fallible-streaming-iterator` | 0.1.9 | MIT/Apache-2.0 | https://github.com/sfackler/fallible-streaming-iterator |
| `fancy-regex` | 0.20.2 | MIT | https://github.com/fancy-regex/fancy-regex |
| `fastrand` | 2.5.0 | Apache-2.0 OR MIT | https://github.com/smol-rs/fastrand |
| `find-msvc-tools` | 0.1.15 | MIT OR Apache-2.0 | https://github.com/rust-lang/cc-rs |
| `fluent-uri` | 0.4.1 | MIT | https://github.com/yescallop/fluent-uri-rs |
| `fnv` | 1.0.7 | Apache-2.0 / MIT | https://github.com/servo/rust-fnv |
| `foldhash` | 0.2.0 | Zlib | https://github.com/orlp/foldhash |
| `form_urlencoded` | 1.2.2 | MIT OR Apache-2.0 | https://github.com/servo/rust-url |
| `fraction` | 0.17.0 | MIT OR Apache-2.0 | https://github.com/dnsl48/fraction.git |
| `futures-core` | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs |
| `futures-task` | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs |
| `futures-util` | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs |
| `getrandom` | 0.3.4 | MIT OR Apache-2.0 | https://github.com/rust-random/getrandom |
| `getrandom` | 0.4.3 | MIT OR Apache-2.0 | https://github.com/rust-random/getrandom |
| `globset` | 0.4.19 | Unlicense OR MIT | https://github.com/BurntSushi/ripgrep/tree/master/crates/globset |
| `hashbrown` | 0.16.1 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown |
| `hashbrown` | 0.17.1 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown |
| `hashlink` | 0.12.2 | MIT OR Apache-2.0 | https://github.com/djc/hashlink |
| `heck` | 0.5.0 | MIT OR Apache-2.0 | https://github.com/withoutboats/heck |
| `hex` | 0.4.3 | MIT OR Apache-2.0 | https://github.com/KokaKiwi/rust-hex |
| `http` | 1.5.0 | MIT OR Apache-2.0 | https://github.com/hyperium/http |
| `http-body` | 1.1.0 | MIT | https://github.com/hyperium/http-body |
| `http-body-util` | 0.1.5 | MIT | https://github.com/hyperium/http-body |
| `httparse` | 1.10.1 | MIT OR Apache-2.0 | https://github.com/seanmonstar/httparse |
| `httpdate` | 1.0.3 | MIT OR Apache-2.0 | https://github.com/pyfisch/httpdate |
| `hybrid-array` | 0.4.15 | MIT OR Apache-2.0 | https://github.com/RustCrypto/hybrid-array |
| `hyper` | 1.12.0 | MIT | https://github.com/hyperium/hyper |
| `hyper-util` | 0.1.21 | MIT | https://github.com/hyperium/hyper-util |
| `iana-time-zone` | 0.1.65 | MIT OR Apache-2.0 | https://github.com/strawlab/iana-time-zone |
| `iana-time-zone-haiku` | 0.1.2 | MIT OR Apache-2.0 | https://github.com/strawlab/iana-time-zone |
| `indexmap` | 2.14.2 | Apache-2.0 OR MIT | https://github.com/indexmap-rs/indexmap |
| `is_terminal_polyfill` | 1.70.2 | MIT OR Apache-2.0 | https://github.com/polyfill-rs/is_terminal_polyfill |
| `itoa` | 1.0.18 | MIT OR Apache-2.0 | https://github.com/dtolnay/itoa |
| `js-sys` | 0.3.107 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/js-sys |
| `jsonschema` | 0.61.1 | MIT | https://github.com/Stranger6667/jsonschema |
| `jsonschema-regex` | 0.61.1 | MIT | https://github.com/Stranger6667/jsonschema |
| `jsonschema-value` | 0.61.1 | MIT | https://github.com/Stranger6667/jsonschema |
| `libc` | 0.2.191 | MIT OR Apache-2.0 | https://github.com/rust-lang/libc |
| `libsqlite3-sys` | 0.38.2 | MIT | https://github.com/rusqlite/rusqlite |
| `linux-raw-sys` | 0.12.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/sunfishcode/linux-raw-sys |
| `lock_api` | 0.4.14 | MIT OR Apache-2.0 | https://github.com/Amanieu/parking_lot |
| `log` | 0.4.35 | MIT OR Apache-2.0 | https://github.com/rust-lang/log |
| `matchit` | 0.8.4 | MIT AND BSD-3-Clause | https://github.com/ibraheemdev/matchit |
| `memchr` | 2.8.3 | Unlicense OR MIT | https://github.com/BurntSushi/memchr |
| `micromap` | 0.3.0 | MIT | https://github.com/yegor256/micromap |
| `mime` | 0.3.18 | MIT OR Apache-2.0 | https://github.com/hyperium/mime |
| `mio` | 1.2.4 | MIT | https://github.com/tokio-rs/mio |
| `num` | 0.4.3 | MIT OR Apache-2.0 | https://github.com/rust-num/num |
| `num-bigint` | 0.4.8 | MIT OR Apache-2.0 | https://github.com/rust-num/num-bigint |
| `num-cmp` | 0.1.0 | MIT/Apache-2.0 | https://github.com/lifthrasiir/num-cmp |
| `num-complex` | 0.4.6 | MIT OR Apache-2.0 | https://github.com/rust-num/num-complex |
| `num-conv` | 0.1.0 | MIT OR Apache-2.0 | https://github.com/jhpratt/num-conv |
| `num-integer` | 0.1.47 | MIT OR Apache-2.0 | https://github.com/rust-num/num-integer |
| `num-iter` | 0.1.46 | MIT OR Apache-2.0 | https://github.com/rust-num/num-iter |
| `num-rational` | 0.4.2 | MIT OR Apache-2.0 | https://github.com/rust-num/num-rational |
| `num-traits` | 0.2.19 | MIT OR Apache-2.0 | https://github.com/rust-num/num-traits |
| `once_cell` | 1.21.4 | MIT OR Apache-2.0 | https://github.com/matklad/once_cell |
| `once_cell_polyfill` | 1.70.2 | MIT OR Apache-2.0 | https://github.com/polyfill-rs/once_cell_polyfill |
| `outref` | 0.5.2 | MIT | https://github.com/Nugine/outref |
| `parking_lot` | 0.12.5 | MIT OR Apache-2.0 | https://github.com/Amanieu/parking_lot |
| `parking_lot_core` | 0.9.12 | MIT OR Apache-2.0 | https://github.com/Amanieu/parking_lot |
| `percent-encoding` | 2.3.2 | MIT OR Apache-2.0 | https://github.com/servo/rust-url/ |
| `phf` | 0.11.3 | MIT | https://github.com/rust-phf/rust-phf |
| `phf_generator` | 0.11.3 | MIT | https://github.com/rust-phf/rust-phf |
| `phf_macros` | 0.11.3 | MIT | https://github.com/rust-phf/rust-phf |
| `phf_shared` | 0.11.3 | MIT | https://github.com/rust-phf/rust-phf |
| `pin-project-lite` | 0.2.17 | Apache-2.0 OR MIT | https://github.com/taiki-e/pin-project-lite |
| `pkg-config` | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/pkg-config-rs |
| `powerfmt` | 0.2.1 | MIT OR Apache-2.0 | https://github.com/jhpratt/powerfmt |
| `ppv-lite86` | 0.2.21 | MIT OR Apache-2.0 | https://github.com/cryptocorrosion/cryptocorrosion |
| `pretty_assertions` | 1.4.1 | MIT OR Apache-2.0 | https://github.com/rust-pretty-assertions/rust-pretty-assertions |
| `proc-macro2` | 1.0.107 | MIT OR Apache-2.0 | https://github.com/dtolnay/proc-macro2 |
| `proptest` | 1.11.0 | MIT OR Apache-2.0 | https://github.com/proptest-rs/proptest |
| `quick-error` | 1.2.3 | MIT/Apache-2.0 | http://github.com/tailhook/quick-error |
| `quote` | 1.0.47 | MIT OR Apache-2.0 | https://github.com/dtolnay/quote |
| `r-efi` | 5.3.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later | https://github.com/r-efi/r-efi |
| `r-efi` | 6.0.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later | https://github.com/r-efi/r-efi |
| `rand` | 0.10.3 | MIT OR Apache-2.0 | https://github.com/rust-random/rand |
| `rand` | 0.8.8 | MIT OR Apache-2.0 | https://github.com/rust-random/rand |
| `rand` | 0.9.5 | MIT OR Apache-2.0 | https://github.com/rust-random/rand |
| `rand_chacha` | 0.9.0 | MIT OR Apache-2.0 | https://github.com/rust-random/rand |
| `rand_core` | 0.10.1 | MIT OR Apache-2.0 | https://github.com/rust-random/rand_core |
| `rand_core` | 0.6.4 | MIT OR Apache-2.0 | https://github.com/rust-random/rand |
| `rand_core` | 0.9.5 | MIT OR Apache-2.0 | https://github.com/rust-random/rand |
| `rand_xorshift` | 0.4.0 | MIT OR Apache-2.0 | https://github.com/rust-random/rngs |
| `redox_syscall` | 0.5.18 | MIT | https://gitlab.redox-os.org/redox-os/syscall |
| `ref-cast` | 1.0.27 | MIT OR Apache-2.0 | https://github.com/dtolnay/ref-cast |
| `ref-cast-impl` | 1.0.27 | MIT OR Apache-2.0 | https://github.com/dtolnay/ref-cast |
| `referencing` | 0.61.1 | MIT | https://github.com/Stranger6667/jsonschema |
| `regex` | 1.13.1 | MIT OR Apache-2.0 | https://github.com/rust-lang/regex |
| `regex-automata` | 0.4.18 | MIT OR Apache-2.0 | https://github.com/rust-lang/regex |
| `regex-syntax` | 0.8.11 | MIT OR Apache-2.0 | https://github.com/rust-lang/regex |
| `rsqlite-vfs` | 0.1.1 | MIT |  |
| `rusqlite` | 0.40.2 | MIT | https://github.com/rusqlite/rusqlite |
| `rustix` | 1.1.6 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/bytecodealliance/rustix |
| `rustversion` | 1.0.23 | MIT OR Apache-2.0 | https://github.com/dtolnay/rustversion |
| `rusty-fork` | 0.3.1 | MIT/Apache-2.0 | https://github.com/altsysrq/rusty-fork |
| `ryu` | 1.0.23 | Apache-2.0 OR BSL-1.0 | https://github.com/dtolnay/ryu |
| `schemars` | 1.2.3 | MIT | https://github.com/GREsau/schemars |
| `schemars_derive` | 1.2.3 | MIT | https://github.com/GREsau/schemars |
| `scopeguard` | 1.2.0 | MIT OR Apache-2.0 | https://github.com/bluss/scopeguard |
| `serde` | 1.0.229 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde |
| `serde_core` | 1.0.229 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde |
| `serde_derive` | 1.0.229 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde |
| `serde_derive_internals` | 0.30.0 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde |
| `serde_json` | 1.0.154 | MIT OR Apache-2.0 | https://github.com/serde-rs/json |
| `serde_path_to_error` | 0.1.20 | MIT OR Apache-2.0 | https://github.com/dtolnay/path-to-error |
| `serde_spanned` | 1.1.3 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml |
| `serde_urlencoded` | 0.7.1 | MIT/Apache-2.0 | https://github.com/nox/serde_urlencoded |
| `serde_yaml_ng` | 0.10.0 | MIT | https://github.com/acatton/serde-yaml-ng |
| `sha2` | 0.11.0 | MIT OR Apache-2.0 | https://github.com/RustCrypto/hashes |
| `shlex` | 2.0.1 | MIT OR Apache-2.0 | https://github.com/comex/rust-shlex |
| `signal-hook` | 0.4.6 | MIT OR Apache-2.0 | https://github.com/vorner/signal-hook |
| `signal-hook-registry` | 1.4.8 | MIT OR Apache-2.0 | https://github.com/vorner/signal-hook |
| `siphasher` | 1.0.4 | MIT OR Apache-2.0 | https://github.com/jedisct1/rust-siphash |
| `slab` | 0.4.12 | MIT | https://github.com/tokio-rs/slab |
| `smallvec` | 1.16.5 | MIT OR Apache-2.0 | https://github.com/servo/rust-smallvec |
| `socket2` | 0.6.5 | MIT OR Apache-2.0 | https://github.com/rust-lang/socket2 |
| `sqlite-wasm-rs` | 0.5.5 | MIT | https://github.com/Spxg/sqlite-wasm-rs |
| `strsim` | 0.11.1 | MIT | https://github.com/rapidfuzz/strsim-rs |
| `strum` | 0.28.0 | MIT | https://github.com/Peternator7/strum |
| `strum_macros` | 0.28.0 | MIT | https://github.com/Peternator7/strum |
| `syn` | 2.0.119 | MIT OR Apache-2.0 | https://github.com/dtolnay/syn |
| `syn` | 3.0.9 | MIT OR Apache-2.0 | https://github.com/dtolnay/syn |
| `sync_wrapper` | 1.0.2 | Apache-2.0 | https://github.com/Actyx/sync_wrapper |
| `tempfile` | 3.27.0 | MIT OR Apache-2.0 | https://github.com/Stebalien/tempfile |
| `thiserror` | 2.0.21 | MIT OR Apache-2.0 | https://github.com/dtolnay/thiserror |
| `thiserror-impl` | 2.0.21 | MIT OR Apache-2.0 | https://github.com/dtolnay/thiserror |
| `time` | 0.3.45 | MIT OR Apache-2.0 | https://github.com/time-rs/time |
| `time-core` | 0.1.7 | MIT OR Apache-2.0 | https://github.com/time-rs/time |
| `time-macros` | 0.2.25 | MIT OR Apache-2.0 | https://github.com/time-rs/time |
| `tokio` | 1.54.1 | MIT | https://github.com/tokio-rs/tokio |
| `tokio-macros` | 2.7.2 | MIT | https://github.com/tokio-rs/tokio |
| `toml` | 1.2.1+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml |
| `toml_datetime` | 1.1.3+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml |
| `toml_parser` | 1.1.5+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml |
| `toml_writer` | 1.2.0+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml |
| `tower` | 0.5.3 | MIT | https://github.com/tower-rs/tower |
| `tower-layer` | 0.3.3 | MIT | https://github.com/tower-rs/tower |
| `tower-service` | 0.3.3 | MIT | https://github.com/tower-rs/tower |
| `tracing` | 0.1.44 | MIT | https://github.com/tokio-rs/tracing |
| `tracing-core` | 0.1.36 | MIT | https://github.com/tokio-rs/tracing |
| `typenum` | 1.20.1 | MIT OR Apache-2.0 | https://github.com/paholg/typenum |
| `ulid` | 3.0.0 | MIT | https://github.com/dylanhart/ulid-rs |
| `unarray` | 0.1.4 | MIT OR Apache-2.0 | https://github.com/cameron1024/unarray |
| `unicode-general-category` | 1.1.0 | Apache-2.0 | https://github.com/yeslogic/unicode-general-category |
| `unicode-ident` | 1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 | https://github.com/dtolnay/unicode-ident |
| `unsafe-libyaml` | 0.2.11 | MIT | https://github.com/dtolnay/unsafe-libyaml |
| `utf8parse` | 0.2.2 | Apache-2.0 OR MIT | https://github.com/alacritty/vte |
| `uuid-simd` | 0.8.0 | MIT | https://github.com/Nugine/simd |
| `vcpkg` | 0.2.15 | MIT/Apache-2.0 | https://github.com/mcgoo/vcpkg-rs |
| `version_check` | 0.9.5 | MIT/Apache-2.0 | https://github.com/SergioBenitez/version_check |
| `vsimd` | 0.8.0 | MIT | https://github.com/Nugine/simd |
| `wait-timeout` | 0.2.1 | MIT/Apache-2.0 | https://github.com/alexcrichton/wait-timeout |
| `wasi` | 0.11.1+wasi-snapshot-preview1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/bytecodealliance/wasi |
| `wasip2` | 1.0.1+wasi-0.2.4 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/bytecodealliance/wasi-rs |
| `wasm-bindgen` | 0.2.130 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen |
| `wasm-bindgen-macro` | 0.2.130 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/macro |
| `wasm-bindgen-macro-support` | 0.2.130 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/main/crates/macro-support |
| `wasm-bindgen-shared` | 0.2.130 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/shared |
| `web-time` | 1.1.0 | MIT OR Apache-2.0 | https://github.com/daxpedda/web-time |
| `windows-core` | 0.62.2 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| `windows-implement` | 0.60.2 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| `windows-interface` | 0.59.3 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| `windows-link` | 0.2.1 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| `windows-result` | 0.4.1 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| `windows-strings` | 0.5.1 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| `windows-sys` | 0.61.2 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| `winnow` | 0.7.15 | MIT | https://github.com/winnow-rs/winnow |
| `winnow` | 1.0.5 | MIT | https://github.com/winnow-rs/winnow |
| `wit-bindgen` | 0.46.0 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/bytecodealliance/wit-bindgen |
| `yansi` | 1.0.1 | MIT OR Apache-2.0 | https://github.com/SergioBenitez/yansi |
| `zerocopy` | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT | https://github.com/google/zerocopy |
| `zerocopy-derive` | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT | https://github.com/google/zerocopy |
| `zmij` | 1.0.23 | MIT | https://github.com/dtolnay/zmij |

## Full licence texts

### MIT License

SPDX identifier: `MIT`

```text
Permission is hereby granted, free of charge, to any
person obtaining a copy of this software and associated
documentation files (the "Software"), to deal in the
Software without restriction, including without
limitation the rights to use, copy, modify, merge,
publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software
is furnished to do so, subject to the following
conditions:

The above copyright notice and this permission notice
shall be included in all copies or substantial portions
of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
```

### MIT No Attribution

SPDX identifier: `MIT-0`

```text
MIT No Attribution

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

### Apache License 2.0

SPDX identifier: `Apache-2.0`

```text
Apache License
                        Version 2.0, January 2004
                     http://www.apache.org/licenses/

TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION

1. Definitions.

   "License" shall mean the terms and conditions for use, reproduction,
   and distribution as defined by Sections 1 through 9 of this document.

   "Licensor" shall mean the copyright owner or entity authorized by
   the copyright owner that is granting the License.

   "Legal Entity" shall mean the union of the acting entity and all
   other entities that control, are controlled by, or are under common
   control with that entity. For the purposes of this definition,
   "control" means (i) the power, direct or indirect, to cause the
   direction or management of such entity, whether by contract or
   otherwise, or (ii) ownership of fifty percent (50%) or more of the
   outstanding shares, or (iii) beneficial ownership of such entity.

   "You" (or "Your") shall mean an individual or Legal Entity
   exercising permissions granted by this License.

   "Source" form shall mean the preferred form for making modifications,
   including but not limited to software source code, documentation
   source, and configuration files.

   "Object" form shall mean any form resulting from mechanical
   transformation or translation of a Source form, including but
   not limited to compiled object code, generated documentation,
   and conversions to other media types.

   "Work" shall mean the work of authorship, whether in Source or
   Object form, made available under the License, as indicated by a
   copyright notice that is included in or attached to the work
   (an example is provided in the Appendix below).

   "Derivative Works" shall mean any work, whether in Source or Object
   form, that is based on (or derived from) the Work and for which the
   editorial revisions, annotations, elaborations, or other modifications
   represent, as a whole, an original work of authorship. For the purposes
   of this License, Derivative Works shall not include works that remain
   separable from, or merely link (or bind by name) to the interfaces of,
   the Work and Derivative Works thereof.

   "Contribution" shall mean any work of authorship, including
   the original version of the Work and any modifications or additions
   to that Work or Derivative Works thereof, that is intentionally
   submitted to Licensor for inclusion in the Work by the copyright owner
   or by an individual or Legal Entity authorized to submit on behalf of
   the copyright owner. For the purposes of this definition, "submitted"
   means any form of electronic, verbal, or written communication sent
   to the Licensor or its representatives, including but not limited to
   communication on electronic mailing lists, source code control systems,
   and issue tracking systems that are managed by, or on behalf of, the
   Licensor for the purpose of discussing and improving the Work, but
   excluding communication that is conspicuously marked or otherwise
   designated in writing by the copyright owner as "Not a Contribution."

   "Contributor" shall mean Licensor and any individual or Legal Entity
   on behalf of whom a Contribution has been received by Licensor and
   subsequently incorporated within the Work.

2. Grant of Copyright License. Subject to the terms and conditions of
   this License, each Contributor hereby grants to You a perpetual,
   worldwide, non-exclusive, no-charge, royalty-free, irrevocable
   copyright license to reproduce, prepare Derivative Works of,
   publicly display, publicly perform, sublicense, and distribute the
   Work and such Derivative Works in Source or Object form.

3. Grant of Patent License. Subject to the terms and conditions of
   this License, each Contributor hereby grants to You a perpetual,
   worldwide, non-exclusive, no-charge, royalty-free, irrevocable
   (except as stated in this section) patent license to make, have made,
   use, offer to sell, sell, import, and otherwise transfer the Work,
   where such license applies only to those patent claims licensable
   by such Contributor that are necessarily infringed by their
   Contribution(s) alone or by combination of their Contribution(s)
   with the Work to which such Contribution(s) was submitted. If You
   institute patent litigation against any entity (including a
   cross-claim or counterclaim in a lawsuit) alleging that the Work
   or a Contribution incorporated within the Work constitutes direct
   or contributory patent infringement, then any patent licenses
   granted to You under this License for that Work shall terminate
   as of the date such litigation is filed.

4. Redistribution. You may reproduce and distribute copies of the
   Work or Derivative Works thereof in any medium, with or without
   modifications, and in Source or Object form, provided that You
   meet the following conditions:

   (a) You must give any other recipients of the Work or
       Derivative Works a copy of this License; and

   (b) You must cause any modified files to carry prominent notices
       stating that You changed the files; and

   (c) You must retain, in the Source form of any Derivative Works
       that You distribute, all copyright, patent, trademark, and
       attribution notices from the Source form of the Work,
       excluding those notices that do not pertain to any part of
       the Derivative Works; and

   (d) If the Work includes a "NOTICE" text file as part of its
       distribution, then any Derivative Works that You distribute must
       include a readable copy of the attribution notices contained
       within such NOTICE file, excluding those notices that do not
       pertain to any part of the Derivative Works, in at least one
       of the following places: within a NOTICE text file distributed
       as part of the Derivative Works; within the Source form or
       documentation, if provided along with the Derivative Works; or,
       within a display generated by the Derivative Works, if and
       wherever such third-party notices normally appear. The contents
       of the NOTICE file are for informational purposes only and
       do not modify the License. You may add Your own attribution
       notices within Derivative Works that You distribute, alongside
       or as an addendum to the NOTICE text from the Work, provided
       that such additional attribution notices cannot be construed
       as modifying the License.

   You may add Your own copyright statement to Your modifications and
   may provide additional or different license terms and conditions
   for use, reproduction, or distribution of Your modifications, or
   for any such Derivative Works as a whole, provided Your use,
   reproduction, and distribution of the Work otherwise complies with
   the conditions stated in this License.

5. Submission of Contributions. Unless You explicitly state otherwise,
   any Contribution intentionally submitted for inclusion in the Work
   by You to the Licensor shall be under the terms and conditions of
   this License, without any additional terms or conditions.
   Notwithstanding the above, nothing herein shall supersede or modify
   the terms of any separate license agreement you may have executed
   with Licensor regarding such Contributions.

6. Trademarks. This License does not grant permission to use the trade
   names, trademarks, service marks, or product names of the Licensor,
   except as required for reasonable and customary use in describing the
   origin of the Work and reproducing the content of the NOTICE file.

7. Disclaimer of Warranty. Unless required by applicable law or
   agreed to in writing, Licensor provides the Work (and each
   Contributor provides its Contributions) on an "AS IS" BASIS,
   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or
   implied, including, without limitation, any warranties or conditions
   of TITLE, NON-INFRINGEMENT, MERCHANTABILITY, or FITNESS FOR A
   PARTICULAR PURPOSE. You are solely responsible for determining the
   appropriateness of using or redistributing the Work and assume any
   risks associated with Your exercise of permissions under this License.

8. Limitation of Liability. In no event and under no legal theory,
   whether in tort (including negligence), contract, or otherwise,
   unless required by applicable law (such as deliberate and grossly
   negligent acts) or agreed to in writing, shall any Contributor be
   liable to You for damages, including any direct, indirect, special,
   incidental, or consequential damages of any character arising as a
   result of this License or out of the use or inability to use the
   Work (including but not limited to damages for loss of goodwill,
   work stoppage, computer failure or malfunction, or any and all
   other commercial damages or losses), even if such Contributor
   has been advised of the possibility of such damages.

9. Accepting Warranty or Additional Liability. While redistributing
   the Work or Derivative Works thereof, You may choose to offer,
   and charge a fee for, acceptance of support, warranty, indemnity,
   or other liability obligations and/or rights consistent with this
   License. However, in accepting such obligations, You may act only
   on Your own behalf and on Your sole responsibility, not on behalf
   of any other Contributor, and only if You agree to indemnify,
   defend, and hold each Contributor harmless for any liability
   incurred by, or claims asserted against, such Contributor by reason
   of your accepting any such warranty or additional liability.

END OF TERMS AND CONDITIONS
```

### Apache License 2.0 with LLVM exception

SPDX identifier: `Apache-2.0 WITH LLVM-exception`

```text
Apache License
                           Version 2.0, January 2004
                        http://www.apache.org/licenses/

   TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION

   1. Definitions.

      "License" shall mean the terms and conditions for use, reproduction,
      and distribution as defined by Sections 1 through 9 of this document.

      "Licensor" shall mean the copyright owner or entity authorized by
      the copyright owner that is granting the License.

      "Legal Entity" shall mean the union of the acting entity and all
      other entities that control, are controlled by, or are under common
      control with that entity. For the purposes of this definition,
      "control" means (i) the power, direct or indirect, to cause the
      direction or management of such entity, whether by contract or
      otherwise, or (ii) ownership of fifty percent (50%) or more of the
      outstanding shares, or (iii) beneficial ownership of such entity.

      "You" (or "Your") shall mean an individual or Legal Entity
      exercising permissions granted by this License.

      "Source" form shall mean the preferred form for making modifications,
      including but not limited to software source code, documentation
      source, and configuration files.

      "Object" form shall mean any form resulting from mechanical
      transformation or translation of a Source form, including but
      not limited to compiled object code, generated documentation,
      and conversions to other media types.

      "Work" shall mean the work of authorship, whether in Source or
      Object form, made available under the License, as indicated by a
      copyright notice that is included in or attached to the work
      (an example is provided in the Appendix below).

      "Derivative Works" shall mean any work, whether in Source or Object
      form, that is based on (or derived from) the Work and for which the
      editorial revisions, annotations, elaborations, or other modifications
      represent, as a whole, an original work of authorship. For the purposes
      of this License, Derivative Works shall not include works that remain
      separable from, or merely link (or bind by name) to the interfaces of,
      the Work and Derivative Works thereof.

      "Contribution" shall mean any work of authorship, including
      the original version of the Work and any modifications or additions
      to that Work or Derivative Works thereof, that is intentionally
      submitted to Licensor for inclusion in the Work by the copyright owner
      or by an individual or Legal Entity authorized to submit on behalf of
      the copyright owner. For the purposes of this definition, "submitted"
      means any form of electronic, verbal, or written communication sent
      to the Licensor or its representatives, including but not limited to
      communication on electronic mailing lists, source code control systems,
      and issue tracking systems that are managed by, or on behalf of, the
      Licensor for the purpose of discussing and improving the Work, but
      excluding communication that is conspicuously marked or otherwise
      designated in writing by the copyright owner as "Not a Contribution."

      "Contributor" shall mean Licensor and any individual or Legal Entity
      on behalf of whom a Contribution has been received by Licensor and
      subsequently incorporated within the Work.

   2. Grant of Copyright License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      copyright license to reproduce, prepare Derivative Works of,
      publicly display, publicly perform, sublicense, and distribute the
      Work and such Derivative Works in Source or Object form.

   3. Grant of Patent License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      (except as stated in this section) patent license to make, have made,
      use, offer to sell, sell, import, and otherwise transfer the Work,
      where such license applies only to those patent claims licensable
      by such Contributor that are necessarily infringed by their
      Contribution(s) alone or by combination of their Contribution(s)
      with the Work to which such Contribution(s) was submitted. If You
      institute patent litigation against any entity (including a
      cross-claim or counterclaim in a lawsuit) alleging that the Work
      or a Contribution incorporated within the Work constitutes direct
      or contributory patent infringement, then any patent licenses
      granted to You under this License for that Work shall terminate
      as of the date such litigation is filed.

   4. Redistribution. You may reproduce and distribute copies of the
      Work or Derivative Works thereof in any medium, with or without
      modifications, and in Source or Object form, provided that You
      meet the following conditions:

      (a) You must give any other recipients of the Work or
          Derivative Works a copy of this License; and

      (b) You must cause any modified files to carry prominent notices
          stating that You changed the files; and

      (c) You must retain, in the Source form of any Derivative Works
          that You distribute, all copyright, patent, trademark, and
          attribution notices from the Source form of the Work,
          excluding those notices that do not pertain to any part of
          the Derivative Works; and

      (d) If the Work includes a "NOTICE" text file as part of its
          distribution, then any Derivative Works that You distribute must
          include a readable copy of the attribution notices contained
          within such NOTICE file, excluding those notices that do not
          pertain to any part of the Derivative Works, in at least one
          of the following places: within a NOTICE text file distributed
          as part of the Derivative Works; within the Source form or
          documentation, if provided along with the Derivative Works; or,
          within a display generated by the Derivative Works, if and
          wherever such third-party notices normally appear. The contents
          of the NOTICE file are for informational purposes only and
          do not modify the License. You may add Your own attribution
          notices within Derivative Works that You distribute, alongside
          or as an addendum to the NOTICE text from the Work, provided
          that such additional attribution notices cannot be construed
          as modifying the License.

      You may add Your own copyright statement to Your modifications and
      may provide additional or different license terms and conditions
      for use, reproduction, or distribution of Your modifications, or
      for any such Derivative Works as a whole, provided Your use,
      reproduction, and distribution of the Work otherwise complies with
      the conditions stated in this License.

   5. Submission of Contributions. Unless You explicitly state otherwise,
      any Contribution intentionally submitted for inclusion in the Work
      by You to the Licensor shall be under the terms and conditions of
      this License, without any additional terms or conditions.
      Notwithstanding the above, nothing herein shall supersede or modify
      the terms of any separate license agreement you may have executed
      with Licensor regarding such Contributions.

   6. Trademarks. This License does not grant permission to use the trade
      names, trademarks, service marks, or product names of the Licensor,
      except as required for reasonable and customary use in describing the
      origin of the Work and reproducing the content of the NOTICE file.

   7. Disclaimer of Warranty. Unless required by applicable law or
      agreed to in writing, Licensor provides the Work (and each
      Contributor provides its Contributions) on an "AS IS" BASIS,
      WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or
      implied, including, without limitation, any warranties or conditions
      of TITLE, NON-INFRINGEMENT, MERCHANTABILITY, or FITNESS FOR A
      PARTICULAR PURPOSE. You are solely responsible for determining the
      appropriateness of using or redistributing the Work and assume any
      risks associated with Your exercise of permissions under this License.

   8. Limitation of Liability. In no event and under no legal theory,
      whether in tort (including negligence), contract, or otherwise,
      unless required by applicable law (such as deliberate and grossly
      negligent acts) or agreed to in writing, shall any Contributor be
      liable to You for damages, including any direct, indirect, special,
      incidental, or consequential damages of any character arising as a
      result of this License or out of the use or inability to use the
      Work (including but not limited to damages for loss of goodwill,
      work stoppage, computer failure or malfunction, or any and all
      other commercial damages or losses), even if such Contributor
      has been advised of the possibility of such damages.

   9. Accepting Warranty or Additional Liability. While redistributing
      the Work or Derivative Works thereof, You may choose to offer,
      and charge a fee for, acceptance of support, warranty, indemnity,
      or other liability obligations and/or rights consistent with this
      License. However, in accepting such obligations, You may act only
      on Your own behalf and on Your sole responsibility, not on behalf
      of any other Contributor, and only if You agree to indemnify,
      defend, and hold each Contributor harmless for any liability
      incurred by, or claims asserted against, such Contributor by reason
      of your accepting any such warranty or additional liability.

   END OF TERMS AND CONDITIONS

   APPENDIX: How to apply the Apache License to your work.

      To apply the Apache License to your work, attach the following
      boilerplate notice, with the fields enclosed by brackets "[]"
      replaced with your own identifying information. (Don't include
      the brackets!)  The text should be enclosed in the appropriate
      comment syntax for the file format. We also recommend that a
      file or class name and description of purpose be included on the
      same "printed page" as the copyright notice for easier
      identification within third-party archives.

   Copyright [yyyy] [name of copyright owner]

   Licensed under the Apache License, Version 2.0 (the "License");
   you may not use this file except in compliance with the License.
   You may obtain a copy of the License at

       http://www.apache.org/licenses/LICENSE-2.0

   Unless required by applicable law or agreed to in writing, software
   distributed under the License is distributed on an "AS IS" BASIS,
   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
   See the License for the specific language governing permissions and
   limitations under the License.


--- LLVM Exceptions to the Apache 2.0 License ----

As an exception, if, as a result of your compiling your source code, portions
of this Software are embedded into an Object form of such source code, you
may redistribute such embedded portions in such Object form without complying
with the conditions of Sections 4(a), 4(b) and 4(d) of the License.

In addition, if you combine or link compiled forms of this Software with
software that is licensed under the GPLv2 ("Combined Software") and if a
court of competent jurisdiction determines that the patent provision (Section
3), the indemnity provision (Section 9) or other Section of the License
conflicts with the conditions of the GPLv2, you may retroactively and
prospectively choose to deem waived or otherwise exclude such Section(s) of
the License, but only in their entirety and only with respect to the Combined
Software.
```

### BSD 2-Clause License

SPDX identifier: `BSD-2-Clause`

```text
Copyright 2019 The Fuchsia Authors.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

   * Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.
   * Redistributions in binary form must reproduce the above
copyright notice, this list of conditions and the following disclaimer
in the documentation and/or other materials provided with the
distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

### BSD 3-Clause License

SPDX identifier: `BSD-3-Clause`

```text
Copyright (c) <year> <copyright holder>

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its contributors
   may be used to endorse or promote products derived from this software
   without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

### ISC License

SPDX identifier: `ISC`

This text is included for the audit's allowed set; no crate in
this locked tree selects it today.

```text
ISC License

Copyright (c) <year> <copyright holders>

Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH
REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY
AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT,
INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM
LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR
OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR
PERFORMANCE OF THIS SOFTWARE.
```

### Unicode License V3

SPDX identifier: `Unicode-3.0`

```text
UNICODE LICENSE V3

COPYRIGHT AND PERMISSION NOTICE

Copyright © 1991-2023 Unicode, Inc.

NOTICE TO USER: Carefully read the following legal agreement. BY
DOWNLOADING, INSTALLING, COPYING OR OTHERWISE USING DATA FILES, AND/OR
SOFTWARE, YOU UNEQUIVOCALLY ACCEPT, AND AGREE TO BE BOUND BY, ALL OF THE
TERMS AND CONDITIONS OF THIS AGREEMENT. IF YOU DO NOT AGREE, DO NOT
DOWNLOAD, INSTALL, COPY, DISTRIBUTE OR USE THE DATA FILES OR SOFTWARE.

Permission is hereby granted, free of charge, to any person obtaining a
copy of data files and any associated documentation (the "Data Files") or
software and any associated documentation (the "Software") to deal in the
Data Files or Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, and/or sell
copies of the Data Files or Software, and to permit persons to whom the
Data Files or Software are furnished to do so, provided that either (a)
this copyright and permission notice appear with all copies of the Data
Files or Software, or (b) this copyright and permission notice appear in
associated Documentation.

THE DATA FILES AND SOFTWARE ARE PROVIDED "AS IS", WITHOUT WARRANTY OF ANY
KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT OF
THIRD PARTY RIGHTS.

IN NO EVENT SHALL THE COPYRIGHT HOLDER OR HOLDERS INCLUDED IN THIS NOTICE
BE LIABLE FOR ANY CLAIM, OR ANY SPECIAL INDIRECT OR CONSEQUENTIAL DAMAGES,
OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS,
WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION,
ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THE DATA
FILES OR SOFTWARE.

Except as contained in this notice, the name of a copyright holder shall
not be used in advertising or otherwise to promote the sale, use or other
dealings in these Data Files or Software without prior written
authorization of the copyright holder.
```

### Zlib License

SPDX identifier: `Zlib`

```text
Copyright (c) 2024 Orson Peters

This software is provided 'as-is', without any express or implied warranty. In
no event will the authors be held liable for any damages arising from the use of
this software.

Permission is granted to anyone to use this software for any purpose, including
commercial applications, and to alter it and redistribute it freely, subject to
the following restrictions:

1. The origin of this software must not be misrepresented; you must not claim
    that you wrote the original software. If you use this software in a product,
    an acknowledgment in the product documentation would be appreciated but is
    not required.

2. Altered source versions must be plainly marked as such, and must not be
    misrepresented as being the original software.

3. This notice may not be removed or altered from any source distribution.
```

## Copyright notices

These are the licence-file paragraphs that carry a copyright notice,
including notices that follow other text or wrap across lines. A missing
notice means the package does not ship a separate copyright notice; its
repository and licence expression are recorded above.

### `ahash` 0.8.12

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/tkaitchuck/ahash
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2018 Tom Kaitchuck

### `aho-corasick` 1.1.5

- Licence: Unlicense OR MIT
- Repository: https://github.com/BurntSushi/aho-corasick
- Licence files: COPYING, LICENSE-MIT, UNLICENSE
- Copyright (c) 2015 Andrew Gallant

### `allocator-api2` 0.2.21

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/zakarumych/allocator-api2
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `android_system_properties` 0.1.6

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/nical/android_system_properties
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright 2016 Nicolas Silva
- Copyright (c) 2013 Nicolas Silva

### `anstream` 1.0.0

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-cli/anstyle.git
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) Individual contributors

### `anstyle` 1.0.14

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-cli/anstyle.git
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) Individual contributors

### `anstyle-parse` 1.0.0

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-cli/anstyle.git
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) Individual contributors

### `anstyle-query` 1.1.5

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-cli/anstyle.git
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) Individual contributors

### `anstyle-wincon` 3.0.11

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-cli/anstyle.git
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) Individual contributors

### `anyhow` 1.0.104

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/dtolnay/anyhow
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `atomic-waker` 1.1.2

- Licence: Apache-2.0 OR MIT
- Repository: https://github.com/smol-rs/atomic-waker
- Licence files: LICENSE-APACHE, LICENSE-MIT, LICENSE-THIRD-PARTY
- Copyright (c) 2016 Alex Crichton Copyright (c) 2017 The Tokio Authors

### `autocfg` 1.5.1

- Licence: Apache-2.0 OR MIT
- Repository: https://github.com/cuviper/autocfg
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2018 Josh Stone

### `axum` 0.8.9

- Licence: MIT
- Repository: https://github.com/tokio-rs/axum
- Licence files: LICENSE
- Copyright (c) 2019 axum Contributors

### `axum-core` 0.5.6

- Licence: MIT
- Repository: https://github.com/tokio-rs/axum
- Licence files: LICENSE
- Copyright (c) 2019–2025 axum Contributors

### `bit-set` 0.8.0

- Licence: Apache-2.0 OR MIT
- Repository: https://github.com/contain-rs/bit-set
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2023 The Rust Project Developers

### `bit-vec` 0.8.0

- Licence: Apache-2.0 OR MIT
- Repository: https://github.com/contain-rs/bit-vec
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2023 The Rust Project Developers

### `bitflags` 2.13.2

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/bitflags/bitflags
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 The Rust Project Developers

### `block-buffer` 0.12.1

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/RustCrypto/utils
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2018-2025 The RustCrypto Project Developers

### `borrow-or-share` 0.2.4

- Licence: MIT-0
- Repository: https://github.com/yescallop/borrow-or-share
- Licence files: LICENSE
- Copyright 2024 Scallop Ye

### `bstr` 1.13.1

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/BurntSushi/bstr
- Licence files: COPYING, LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2018-2019 Andrew Gallant

### `bumpalo` 3.20.3

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/fitzgen/bumpalo
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2019 Nick Fitzgerald

### `bytecount` 0.6.9

- Licence: Apache-2.0/MIT
- Repository: https://github.com/llogiq/bytecount
- Licence files: LICENSE.Apache2, LICENSE.MIT
- Copyright (c) 2017 The bytecount Developers

### `bytes` 1.12.1

- Licence: MIT
- Repository: https://github.com/tokio-rs/bytes
- Licence files: LICENSE
- Copyright (c) 2018 Carl Lerche

### `cc` 1.11.1

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-lang/cc-rs
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 Alex Crichton

### `cfg-if` 1.0.5

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-lang/cfg-if
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 Alex Crichton

### `chacha20` 0.10.2

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/RustCrypto/stream-ciphers
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2019-2026 The RustCrypto Project Developers

### `chrono` 0.4.45

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/chronotope/chrono
- Licence files: LICENSE.txt
- Rust-chrono is dual-licensed under The MIT License [1] and Apache 2.0 License [2]. Copyright (c) 2014--2026, Kang Seonghoon and contributors.
- Copyright (c) 2014, Kang Seonghoon.

### `clap` 4.6.7

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/clap-rs/clap
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright Individual contributors
- Copyright (c) Individual contributors

### `clap_builder` 4.6.7

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/clap-rs/clap
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright Individual contributors
- Copyright (c) Individual contributors

### `clap_derive` 4.6.7

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/clap-rs/clap
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright Individual contributors
- Copyright (c) Individual contributors

### `clap_lex` 1.1.1

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/clap-rs/clap
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright Individual contributors
- Copyright (c) Individual contributors

### `colorchoice` 1.0.5

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-cli/anstyle.git
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) Individual contributors

### `const-oid` 0.10.2

- Licence: Apache-2.0 OR MIT
- Repository: https://github.com/RustCrypto/formats
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2020-2026 The RustCrypto Project Developers

### `core-foundation-sys` 0.8.7

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/servo/core-foundation-rs
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2012-2013 Mozilla Foundation

### `cpufeatures` 0.3.1

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/RustCrypto/utils
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2020-2026 The RustCrypto Project Developers

### `cron` 0.17.0

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/zslayton/cron
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2015 Zack Slayton

### `crypto-common` 0.2.2

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/RustCrypto/traits
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2021-2026 RustCrypto Developers

### `data-encoding` 2.11.1

- Licence: MIT
- Repository: https://github.com/ia0/data-encoding
- Licence files: LICENSE
- Copyright (c) 2015-2020 Julien Cretin Copyright (c) 2017-2020 Google Inc.

### `deranged` 0.5.9

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/jhpratt/deranged
- Licence files: LICENSE-Apache, LICENSE-MIT
- Copyright 2024 Jacob Pratt et al.
- Copyright (c) 2024 Jacob Pratt et al.

### `diff` 0.1.13

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/utkarshkukreti/diff.rs
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2015 Utkarsh Kukreti

### `digest` 0.11.3

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/RustCrypto/traits
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2017-2025 RustCrypto Developers Copyright (c) 2017 Artyom Pavlov

### `dyn-clone` 1.0.20

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/dtolnay/dyn-clone
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `email_address` 0.2.9

- Licence: MIT
- Repository: https://github.com/johnstonskj/rust-email_address.git
- Licence files: LICENSE
- Copyright (c) 2019 Simon Johnston

### `equivalent` 1.0.3

- Licence: Apache-2.0 OR MIT
- Repository: https://github.com/indexmap-rs/equivalent
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2016--2023

### `errno` 0.3.14

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/lambda-fairy/rust-errno
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 Chris Wong

### `fallible-iterator` 0.3.0

- Licence: MIT/Apache-2.0
- Repository: https://github.com/sfackler/rust-fallible-iterator
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2015 The rust-openssl-verify Developers

### `fallible-streaming-iterator` 0.1.9

- Licence: MIT/Apache-2.0
- Repository: https://github.com/sfackler/fallible-streaming-iterator
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2016 The fallible-streaming-iterator Developers

### `fancy-regex` 0.20.2

- Licence: MIT
- Repository: https://github.com/fancy-regex/fancy-regex
- Licence files: LICENSE
- Copyright 2015 The Fancy Regex Authors.

### `fastrand` 2.5.0

- Licence: Apache-2.0 OR MIT
- Repository: https://github.com/smol-rs/fastrand
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `find-msvc-tools` 0.1.15

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-lang/cc-rs
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 Alex Crichton

### `fluent-uri` 0.4.1

- Licence: MIT
- Repository: https://github.com/yescallop/fluent-uri-rs
- Licence files: LICENSE
- Copyright (c) 2024 Scallop Ye

### `fnv` 1.0.7

- Licence: Apache-2.0 / MIT
- Repository: https://github.com/servo/rust-fnv
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2017 Contributors

### `foldhash` 0.2.0

- Licence: Zlib
- Repository: https://github.com/orlp/foldhash
- Licence files: LICENSE
- Copyright (c) 2024 Orson Peters

### `form_urlencoded` 1.2.2

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/servo/rust-url
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2013-2016 The rust-url developers

### `fraction` 0.17.0

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/dnsl48/fraction.git
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 The Rust Project Developers

### `futures-core` 0.3.34

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-lang/futures-rs
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2016 Alex Crichton Copyright (c) 2017 The Tokio Authors

### `futures-task` 0.3.34

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-lang/futures-rs
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2016 Alex Crichton Copyright (c) 2017 The Tokio Authors

### `futures-util` 0.3.34

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-lang/futures-rs
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2016 Alex Crichton Copyright (c) 2017 The Tokio Authors

### `getrandom` 0.3.4

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-random/getrandom
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2018-2025 The rust-random Project Developers Copyright (c) 2014 The Rust Project Developers

### `getrandom` 0.4.3

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-random/getrandom
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2018-2026 The rust-random Project Developers Copyright (c) 2014 The Rust Project Developers

### `globset` 0.4.19

- Licence: Unlicense OR MIT
- Repository: https://github.com/BurntSushi/ripgrep/tree/master/crates/globset
- Licence files: COPYING, LICENSE-MIT, UNLICENSE
- Copyright (c) 2015 Andrew Gallant

### `hashbrown` 0.16.1

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-lang/hashbrown
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2016 Amanieu d'Antras

### `hashbrown` 0.17.1

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-lang/hashbrown
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2016 Amanieu d'Antras

### `hashlink` 0.12.2

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/djc/hashlink
- Licence files: LICENSE-APACHE, LICENSE-MIT
- This work is derived in part from the `linked-hash-map` crate, Copyright (c) 2015 The Rust Project Developers

### `heck` 0.5.0

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/withoutboats/heck
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2015 The Rust Project Developers

### `hex` 0.4.3

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/KokaKiwi/rust-hex
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2013-2014 The Rust Project Developers. Copyright (c) 2015-2020 The rust-hex Developers

### `http` 1.5.0

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/hyperium/http
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright 2017 http-rs authors
- Copyright (c) 2017 http-rs authors

### `http-body` 1.1.0

- Licence: MIT
- Repository: https://github.com/hyperium/http-body
- Licence files: LICENSE
- Copyright (c) 2019-2026 Sean McArthur & Hyper Contributors

### `http-body-util` 0.1.5

- Licence: MIT
- Repository: https://github.com/hyperium/http-body
- Licence files: LICENSE
- Copyright (c) 2019-2026 Sean McArthur & Hyper Contributors

### `httparse` 1.10.1

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/seanmonstar/httparse
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2015-2025 Sean McArthur

### `httpdate` 1.0.3

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/pyfisch/httpdate
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2016 Pyfisch

### `hybrid-array` 0.4.15

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/RustCrypto/hybrid-array
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2022-2026 The RustCrypto Project Developers

### `hyper` 1.12.0

- Licence: MIT
- Repository: https://github.com/hyperium/hyper
- Licence files: LICENSE
- Copyright (c) 2014-2026 Sean McArthur

### `hyper-util` 0.1.21

- Licence: MIT
- Repository: https://github.com/hyperium/hyper-util
- Licence files: LICENSE
- Copyright (c) 2023-2025 Sean McArthur

### `iana-time-zone` 0.1.65

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/strawlab/iana-time-zone
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright 2020 Andrew Straw
- Copyright (c) 2020 Andrew D. Straw

### `iana-time-zone-haiku` 0.1.2

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/strawlab/iana-time-zone
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright 2020 Andrew Straw
- Copyright (c) 2020 Andrew D. Straw

### `indexmap` 2.14.2

- Licence: Apache-2.0 OR MIT
- Repository: https://github.com/indexmap-rs/indexmap
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2016--2017

### `is_terminal_polyfill` 1.70.2

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/polyfill-rs/is_terminal_polyfill
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) Individual contributors

### `itoa` 1.0.18

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/dtolnay/itoa
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `js-sys` 0.3.107

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/js-sys
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 Alex Crichton

### `jsonschema` 0.61.1

- Licence: MIT
- Repository: https://github.com/Stranger6667/jsonschema
- Licence files: LICENSE
- Copyright (c) 2020-2026 Dmitry Dygalo

### `jsonschema-regex` 0.61.1

- Licence: MIT
- Repository: https://github.com/Stranger6667/jsonschema
- Licence files: none in the crate package
- Copyright: no separate copyright notice in the shipped licence files

### `jsonschema-value` 0.61.1

- Licence: MIT
- Repository: https://github.com/Stranger6667/jsonschema
- Licence files: none in the crate package
- Copyright: no separate copyright notice in the shipped licence files

### `libc` 0.2.191

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-lang/libc
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) The Rust Project Developers

### `libsqlite3-sys` 0.38.2

- Licence: MIT
- Repository: https://github.com/rusqlite/rusqlite
- Licence files: LICENSE, sqlcipher/LICENSE
- Copyright (c) 2014 The rusqlite developers
- Copyright (c) 2008-2020 Zetetic LLC All rights reserved.

### `linux-raw-sys` 0.12.1

- Licence: Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT
- Repository: https://github.com/sunfishcode/linux-raw-sys
- Licence files: COPYRIGHT, LICENSE-APACHE, LICENSE-Apache-2.0_WITH_LLVM-exception, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `lock_api` 0.4.14

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/Amanieu/parking_lot
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2016 The Rust Project Developers

### `log` 0.4.35

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-lang/log
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 The Rust Project Developers

### `matchit` 0.8.4

- Licence: MIT AND BSD-3-Clause
- Repository: https://github.com/ibraheemdev/matchit
- Licence files: LICENSE, LICENSE.httprouter
- Copyright (c) 2022 Ibraheem Ahmed
- Copyright (c) 2013, Julien Schmidt All rights reserved.

### `memchr` 2.8.3

- Licence: Unlicense OR MIT
- Repository: https://github.com/BurntSushi/memchr
- Licence files: COPYING, LICENSE-MIT, UNLICENSE
- Copyright (c) 2015 Andrew Gallant

### `micromap` 0.3.0

- Licence: MIT
- Repository: https://github.com/yegor256/micromap
- Licence files: LICENSE.txt, .github/workflows/copyrights.yml
- Copyright (c) 2023-2026 Yegor Bugayenko
- # SPDX-FileCopyrightText: Copyright (c) 2023-2026 Yegor Bugayenko # SPDX-License-Identifier: MIT --- # yamllint disable rule:line-length name: copyrights 'on': push: pull_request: jobs: copyrights: timeout-minutes: 15 runs-on: ubuntu-24.04 steps: - uses: actions/checkout@v6 - uses: yegor256/copyrights-action@0.0.12

### `mime` 0.3.18

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/hyperium/mime
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 Sean McArthur

### `mio` 1.2.4

- Licence: MIT
- Repository: https://github.com/tokio-rs/mio
- Licence files: LICENSE
- Copyright (c) 2014 Carl Lerche and other MIO contributors

### `num` 0.4.3

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-num/num
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 The Rust Project Developers

### `num-bigint` 0.4.8

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-num/num-bigint
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 The Rust Project Developers

### `num-cmp` 0.1.0

- Licence: MIT/Apache-2.0
- Repository: https://github.com/lifthrasiir/num-cmp
- Licence files: LICENSE.txt
- In order to allow the maximal compatibility with Rust projects, this source code is also dual-licensed under the MIT License [2] and Apache 2.0 License [3]. Copyright (c) 2017, Kang Seonghoon.
- Copyright (c) 2017, Kang Seonghoon.

### `num-complex` 0.4.6

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-num/num-complex
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 The Rust Project Developers

### `num-conv` 0.1.0

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/jhpratt/num-conv
- Licence files: LICENSE-Apache, LICENSE-MIT
- Copyright 2023 Jacob Pratt
- Copyright (c) 2023 Jacob Pratt

### `num-integer` 0.1.47

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-num/num-integer
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 The Rust Project Developers

### `num-iter` 0.1.46

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-num/num-iter
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 The Rust Project Developers

### `num-rational` 0.4.2

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-num/num-rational
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 The Rust Project Developers

### `num-traits` 0.2.19

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-num/num-traits
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 The Rust Project Developers

### `once_cell` 1.21.4

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/matklad/once_cell
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `once_cell_polyfill` 1.70.2

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/polyfill-rs/once_cell_polyfill
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) Individual contributors

### `outref` 0.5.2

- Licence: MIT
- Repository: https://github.com/Nugine/outref
- Licence files: LICENSE
- Copyright (c) 2022 Nugine

### `parking_lot` 0.12.5

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/Amanieu/parking_lot
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2016 The Rust Project Developers

### `parking_lot_core` 0.9.12

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/Amanieu/parking_lot
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2016 The Rust Project Developers

### `percent-encoding` 2.3.2

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/servo/rust-url/
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2013-2025 The rust-url developers

### `phf` 0.11.3

- Licence: MIT
- Repository: https://github.com/rust-phf/rust-phf
- Licence files: LICENSE
- Copyright (c) 2014-2022 Steven Fackler, Yuki Okushi

### `phf_generator` 0.11.3

- Licence: MIT
- Repository: https://github.com/rust-phf/rust-phf
- Licence files: LICENSE
- Copyright (c) 2014-2022 Steven Fackler, Yuki Okushi

### `phf_macros` 0.11.3

- Licence: MIT
- Repository: https://github.com/rust-phf/rust-phf
- Licence files: LICENSE
- Copyright (c) 2014-2022 Steven Fackler, Yuki Okushi

### `phf_shared` 0.11.3

- Licence: MIT
- Repository: https://github.com/rust-phf/rust-phf
- Licence files: LICENSE
- Copyright (c) 2014-2022 Steven Fackler, Yuki Okushi

### `pin-project-lite` 0.2.17

- Licence: Apache-2.0 OR MIT
- Repository: https://github.com/taiki-e/pin-project-lite
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `pkg-config` 0.3.34

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-lang/pkg-config-rs
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 Alex Crichton

### `powerfmt` 0.2.1

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/jhpratt/powerfmt
- Licence files: LICENSE-Apache, LICENSE-MIT
- Copyright 2023 Jacob Pratt et al.
- Copyright (c) 2023 Jacob Pratt et al.

### `ppv-lite86` 0.2.21

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/cryptocorrosion/cryptocorrosion
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright 2019 The CryptoCorrosion Contributors
- Copyright (c) 2019 The CryptoCorrosion Contributors

### `pretty_assertions` 1.4.1

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-pretty-assertions/rust-pretty-assertions
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2016 rust-derive-builder contributors

### `proc-macro2` 1.0.107

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/dtolnay/proc-macro2
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `proptest` 1.11.0

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/proptest-rs/proptest
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2016 FullContact, Inc

### `quick-error` 1.2.3

- Licence: MIT/Apache-2.0
- Repository: http://github.com/tailhook/quick-error
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2015 The quick-error Developers

### `quote` 1.0.47

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/dtolnay/quote
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `r-efi` 5.3.0

- Licence: MIT OR Apache-2.0 OR LGPL-2.1-or-later
- Repository: https://github.com/r-efi/r-efi
- Licence files: none in the crate package
- Copyright: no separate copyright notice in the shipped licence files

### `r-efi` 6.0.0

- Licence: MIT OR Apache-2.0 OR LGPL-2.1-or-later
- Repository: https://github.com/r-efi/r-efi
- Licence files: none in the crate package
- Copyright: no separate copyright notice in the shipped licence files

### `rand` 0.10.3

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-random/rand
- Licence files: COPYRIGHT, LICENSE-APACHE, LICENSE-MIT
- Copyright 2018 Developers of the Rand project Copyright (c) 2014 The Rust Project Developers

### `rand` 0.8.8

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-random/rand
- Licence files: COPYRIGHT, LICENSE-APACHE, LICENSE-MIT
- Copyright 2018 Developers of the Rand project Copyright (c) 2014 The Rust Project Developers

### `rand` 0.9.5

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-random/rand
- Licence files: COPYRIGHT, LICENSE-APACHE, LICENSE-MIT
- Copyright 2018 Developers of the Rand project Copyright (c) 2014 The Rust Project Developers

### `rand_chacha` 0.9.0

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-random/rand
- Licence files: COPYRIGHT, LICENSE-APACHE, LICENSE-MIT
- Copyright 2018 Developers of the Rand project Copyright (c) 2014 The Rust Project Developers

### `rand_core` 0.10.1

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-random/rand_core
- Licence files: COPYRIGHT, LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2018-2026 The Rand Project Developers

### `rand_core` 0.6.4

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-random/rand
- Licence files: COPYRIGHT, LICENSE-APACHE, LICENSE-MIT
- Copyright 2018 Developers of the Rand project Copyright (c) 2014 The Rust Project Developers

### `rand_core` 0.9.5

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-random/rand
- Licence files: COPYRIGHT, LICENSE-APACHE, LICENSE-MIT
- Copyright 2018 Developers of the Rand project Copyright (c) 2014 The Rust Project Developers

### `rand_xorshift` 0.4.0

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-random/rngs
- Licence files: COPYRIGHT, LICENSE-APACHE, LICENSE-MIT
- Copyright 2018 Developers of the Rand project Copyright (c) 2014 The Rust Project Developers

### `redox_syscall` 0.5.18

- Licence: MIT
- Repository: https://gitlab.redox-os.org/redox-os/syscall
- Licence files: LICENSE
- Copyright (c) 2017 Redox OS Developers

### `ref-cast` 1.0.27

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/dtolnay/ref-cast
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `ref-cast-impl` 1.0.27

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/dtolnay/ref-cast
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `referencing` 0.61.1

- Licence: MIT
- Repository: https://github.com/Stranger6667/jsonschema
- Licence files: LICENSE
- Copyright (c) 2020-2026 Dmitry Dygalo

### `regex` 1.13.1

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-lang/regex
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 The Rust Project Developers

### `regex-automata` 0.4.18

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-lang/regex
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 The Rust Project Developers

### `regex-syntax` 0.8.11

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-lang/regex
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 The Rust Project Developers

### `rsqlite-vfs` 0.1.1

- Licence: MIT
- Repository: not declared
- Licence files: none in the crate package
- Copyright: no separate copyright notice in the shipped licence files

### `rusqlite` 0.40.2

- Licence: MIT
- Repository: https://github.com/rusqlite/rusqlite
- Licence files: LICENSE
- Copyright (c) 2014 The rusqlite developers

### `rustix` 1.1.6

- Licence: Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT
- Repository: https://github.com/bytecodealliance/rustix
- Licence files: COPYRIGHT, LICENSE-APACHE, LICENSE-Apache-2.0_WITH_LLVM-exception, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `rustversion` 1.0.23

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/dtolnay/rustversion
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `rusty-fork` 0.3.1

- Licence: MIT/Apache-2.0
- Repository: https://github.com/altsysrq/rusty-fork
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2016 FullContact, Inc

### `ryu` 1.0.23

- Licence: Apache-2.0 OR BSL-1.0
- Repository: https://github.com/dtolnay/ryu
- Licence files: LICENSE-APACHE, LICENSE-BOOST
- Copyright: no separate copyright notice in the shipped licence files

### `schemars` 1.2.3

- Licence: MIT
- Repository: https://github.com/GREsau/schemars
- Licence files: LICENSE
- Copyright (c) 2019 Graham Esau

### `schemars_derive` 1.2.3

- Licence: MIT
- Repository: https://github.com/GREsau/schemars
- Licence files: LICENSE
- Copyright (c) 2019 Graham Esau

### `scopeguard` 1.2.0

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/bluss/scopeguard
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2016-2019 Ulrik Sverdrup "bluss" and scopeguard developers

### `serde` 1.0.229

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/serde-rs/serde
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `serde_core` 1.0.229

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/serde-rs/serde
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `serde_derive` 1.0.229

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/serde-rs/serde
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `serde_derive_internals` 0.30.0

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/serde-rs/serde
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `serde_json` 1.0.154

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/serde-rs/json
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `serde_path_to_error` 0.1.20

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/dtolnay/path-to-error
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `serde_spanned` 1.1.3

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/toml-rs/toml
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright Individual contributors
- Copyright (c) Individual contributors

### `serde_urlencoded` 0.7.1

- Licence: MIT/Apache-2.0
- Repository: https://github.com/nox/serde_urlencoded
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2016 Anthony Ramine

### `serde_yaml_ng` 0.10.0

- Licence: MIT
- Repository: https://github.com/acatton/serde-yaml-ng
- Licence files: LICENSE
- Copyright 2024 Antoine Catton Copyright 2016-2024 David Tolnay

### `sha2` 0.11.0

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/RustCrypto/hashes
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2016-2026 The RustCrypto Project Developers Copyright (c) 2016 Artyom Pavlov Copyright (c) 2009-2013 Mozilla Foundation Copyright (c) 2006-2009 Graydon Hoare

### `shlex` 2.0.1

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/comex/rust-shlex
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright 2015 Nicholas Allegra (comex).
- Copyright (c) 2015 Nicholas Allegra (comex).

### `signal-hook` 0.4.6

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/vorner/signal-hook
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2017 tokio-jsonrpc developers

### `signal-hook-registry` 1.4.8

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/vorner/signal-hook
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2017 tokio-jsonrpc developers

### `siphasher` 1.0.4

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/jedisct1/rust-siphash
- Licence files: COPYING, LICENSE-APACHE, LICENSE-MIT
- Copyright 2012-2016 The Rust Project Developers. Copyright 2016-2026 Frank Denis.

### `slab` 0.4.12

- Licence: MIT
- Repository: https://github.com/tokio-rs/slab
- Licence files: LICENSE
- Copyright (c) 2019 Carl Lerche

### `smallvec` 1.16.5

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/servo/rust-smallvec
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2018 The Servo Project Developers

### `socket2` 0.6.5

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/rust-lang/socket2
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 Alex Crichton

### `sqlite-wasm-rs` 0.5.5

- Licence: MIT
- Repository: https://github.com/Spxg/sqlite-wasm-rs
- Licence files: LICENSE
- Copyright (c) 2024 Spxg

### `strsim` 0.11.1

- Licence: MIT
- Repository: https://github.com/rapidfuzz/strsim-rs
- Licence files: LICENSE
- Copyright (c) 2015 Danny Guo Copyright (c) 2016 Titus Wormer Copyright (c) 2018 Akash Kurdekar

### `strum` 0.28.0

- Licence: MIT
- Repository: https://github.com/Peternator7/strum
- Licence files: LICENSE
- Copyright (c) 2019 Peter Glotfelty

### `strum_macros` 0.28.0

- Licence: MIT
- Repository: https://github.com/Peternator7/strum
- Licence files: LICENSE
- Copyright (c) 2019 Peter Glotfelty

### `syn` 2.0.119

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/dtolnay/syn
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `syn` 3.0.9

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/dtolnay/syn
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `sync_wrapper` 1.0.2

- Licence: Apache-2.0
- Repository: https://github.com/Actyx/sync_wrapper
- Licence files: LICENSE
- Copyright: no separate copyright notice in the shipped licence files

### `tempfile` 3.27.0

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/Stebalien/tempfile
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2015 Steven Allen

### `thiserror` 2.0.21

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/dtolnay/thiserror
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `thiserror-impl` 2.0.21

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/dtolnay/thiserror
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `time` 0.3.45

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/time-rs/time
- Licence files: LICENSE-Apache, LICENSE-MIT
- Copyright (c) Jacob Pratt et al.

### `time-core` 0.1.7

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/time-rs/time
- Licence files: LICENSE-Apache, LICENSE-MIT
- Copyright (c) Jacob Pratt et al.

### `time-macros` 0.2.25

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/time-rs/time
- Licence files: LICENSE-Apache, LICENSE-MIT
- Copyright (c) Jacob Pratt et al.

### `tokio` 1.54.1

- Licence: MIT
- Repository: https://github.com/tokio-rs/tokio
- Licence files: LICENSE
- Copyright (c) Tokio Contributors

### `tokio-macros` 2.7.2

- Licence: MIT
- Repository: https://github.com/tokio-rs/tokio
- Licence files: LICENSE
- Copyright (c) 2019 Yoshua Wuyts Copyright (c) Tokio Contributors

### `toml` 1.2.1+spec-1.1.0

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/toml-rs/toml
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright Individual contributors
- Copyright (c) Individual contributors

### `toml_datetime` 1.1.3+spec-1.1.0

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/toml-rs/toml
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright Individual contributors
- Copyright (c) Individual contributors

### `toml_parser` 1.1.5+spec-1.1.0

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/toml-rs/toml
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright Individual contributors
- Copyright (c) Individual contributors

### `toml_writer` 1.2.0+spec-1.1.0

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/toml-rs/toml
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright Individual contributors
- Copyright (c) Individual contributors

### `tower` 0.5.3

- Licence: MIT
- Repository: https://github.com/tower-rs/tower
- Licence files: LICENSE
- Copyright (c) 2019 Tower Contributors

### `tower-layer` 0.3.3

- Licence: MIT
- Repository: https://github.com/tower-rs/tower
- Licence files: LICENSE
- Copyright (c) 2019 Tower Contributors

### `tower-service` 0.3.3

- Licence: MIT
- Repository: https://github.com/tower-rs/tower
- Licence files: LICENSE
- Copyright (c) 2019 Tower Contributors

### `tracing` 0.1.44

- Licence: MIT
- Repository: https://github.com/tokio-rs/tracing
- Licence files: LICENSE
- Copyright (c) 2019 Tokio Contributors

### `tracing-core` 0.1.36

- Licence: MIT
- Repository: https://github.com/tokio-rs/tracing
- Licence files: LICENSE
- Copyright (c) 2019 Tokio Contributors

### `typenum` 1.20.1

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/paholg/typenum
- Licence files: LICENSE, LICENSE-APACHE, LICENSE-MIT
- Copyright 2014 Paho Lurie-Gregg
- Copyright (c) 2014 Paho Lurie-Gregg

### `ulid` 3.0.0

- Licence: MIT
- Repository: https://github.com/dylanhart/ulid-rs
- Licence files: LICENSE
- Copyright (c) 2017 Dylan Hart

### `unarray` 0.1.4

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/cameron1024/unarray
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `unicode-general-category` 1.1.0

- Licence: Apache-2.0
- Repository: https://github.com/yeslogic/unicode-general-category
- Licence files: LICENSE
- Copyright: no separate copyright notice in the shipped licence files

### `unicode-ident` 1.0.27

- Licence: (MIT OR Apache-2.0) AND Unicode-3.0
- Repository: https://github.com/dtolnay/unicode-ident
- Licence files: LICENSE-APACHE, LICENSE-MIT, LICENSE-UNICODE
- Copyright © 1991-2023 Unicode, Inc.

### `unsafe-libyaml` 0.2.11

- Licence: MIT
- Repository: https://github.com/dtolnay/unsafe-libyaml
- Licence files: LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `utf8parse` 0.2.2

- Licence: Apache-2.0 OR MIT
- Repository: https://github.com/alacritty/vte
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2016 Joe Wilm

### `uuid-simd` 0.8.0

- Licence: MIT
- Repository: https://github.com/Nugine/simd
- Licence files: none in the crate package
- Copyright: no separate copyright notice in the shipped licence files

### `vcpkg` 0.2.15

- Licence: MIT/Apache-2.0
- Repository: https://github.com/mcgoo/vcpkg-rs
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2017 Jim McGrath

### `version_check` 0.9.5

- Licence: MIT/Apache-2.0
- Repository: https://github.com/SergioBenitez/version_check
- Licence files: LICENSE-APACHE, LICENSE-MIT
- The MIT License (MIT) Copyright (c) 2017-2018 Sergio Benitez

### `vsimd` 0.8.0

- Licence: MIT
- Repository: https://github.com/Nugine/simd
- Licence files: none in the crate package
- Copyright: no separate copyright notice in the shipped licence files

### `wait-timeout` 0.2.1

- Licence: MIT/Apache-2.0
- Repository: https://github.com/alexcrichton/wait-timeout
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 Alex Crichton

### `wasi` 0.11.1+wasi-snapshot-preview1

- Licence: Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT
- Repository: https://github.com/bytecodealliance/wasi
- Licence files: LICENSE-APACHE, LICENSE-Apache-2.0_WITH_LLVM-exception, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `wasip2` 1.0.1+wasi-0.2.4

- Licence: Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT
- Repository: https://github.com/bytecodealliance/wasi-rs
- Licence files: none in the crate package
- Copyright: no separate copyright notice in the shipped licence files

### `wasm-bindgen` 0.2.130

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/wasm-bindgen/wasm-bindgen
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 Alex Crichton

### `wasm-bindgen-macro` 0.2.130

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/macro
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 Alex Crichton

### `wasm-bindgen-macro-support` 0.2.130

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/wasm-bindgen/wasm-bindgen/tree/main/crates/macro-support
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 Alex Crichton

### `wasm-bindgen-shared` 0.2.130

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/shared
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright (c) 2014 Alex Crichton

### `web-time` 1.1.0

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/daxpedda/web-time
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright 2023 dAxpeDDa
- Copyright (c) 2023 dAxpeDDa

### `windows-core` 0.62.2

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/microsoft/windows-rs
- Licence files: license-apache-2.0, license-mit
- Copyright (c) Microsoft Corporation.

### `windows-implement` 0.60.2

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/microsoft/windows-rs
- Licence files: license-apache-2.0, license-mit
- Copyright (c) Microsoft Corporation.

### `windows-interface` 0.59.3

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/microsoft/windows-rs
- Licence files: license-apache-2.0, license-mit
- Copyright (c) Microsoft Corporation.

### `windows-link` 0.2.1

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/microsoft/windows-rs
- Licence files: license-apache-2.0, license-mit
- Copyright (c) Microsoft Corporation.

### `windows-result` 0.4.1

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/microsoft/windows-rs
- Licence files: license-apache-2.0, license-mit
- Copyright (c) Microsoft Corporation.

### `windows-strings` 0.5.1

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/microsoft/windows-rs
- Licence files: license-apache-2.0, license-mit
- Copyright (c) Microsoft Corporation.

### `windows-sys` 0.61.2

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/microsoft/windows-rs
- Licence files: license-apache-2.0, license-mit
- Copyright (c) Microsoft Corporation.

### `winnow` 0.7.15

- Licence: MIT
- Repository: https://github.com/winnow-rs/winnow
- Licence files: LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `winnow` 1.0.5

- Licence: MIT
- Repository: https://github.com/winnow-rs/winnow
- Licence files: LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `wit-bindgen` 0.46.0

- Licence: Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT
- Repository: https://github.com/bytecodealliance/wit-bindgen
- Licence files: LICENSE-APACHE, LICENSE-Apache-2.0_WITH_LLVM-exception, LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files

### `yansi` 1.0.1

- Licence: MIT OR Apache-2.0
- Repository: https://github.com/SergioBenitez/yansi
- Licence files: LICENSE-APACHE, LICENSE-MIT
- Copyright 2017 Sergio Benitez
- The MIT License (MIT) Copyright (c) 2017 Sergio Benitez

### `zerocopy` 0.8.63

- Licence: BSD-2-Clause OR Apache-2.0 OR MIT
- Repository: https://github.com/google/zerocopy
- Licence files: LICENSE-APACHE, LICENSE-BSD, LICENSE-MIT
- Copyright 2023 The Fuchsia Authors
- Copyright 2019 The Fuchsia Authors.

### `zerocopy-derive` 0.8.63

- Licence: BSD-2-Clause OR Apache-2.0 OR MIT
- Repository: https://github.com/google/zerocopy
- Licence files: LICENSE-APACHE, LICENSE-BSD, LICENSE-MIT
- Copyright 2023 The Fuchsia Authors
- Copyright 2019 The Fuchsia Authors.

### `zmij` 1.0.23

- Licence: MIT
- Repository: https://github.com/dtolnay/zmij
- Licence files: LICENSE-MIT
- Copyright: no separate copyright notice in the shipped licence files
