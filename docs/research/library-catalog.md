# Library catalog — 2026-09-08

This is a dated research snapshot, not a dependency installation list. See the [research method](2026-09-08-cli-libraries.md) and [current library guide](../library-guide.md). The original MSRV comparison was against 1.85.1; the template now requires 1.98.1. Full features and observations are in [the JSON snapshot](library-catalog.json).

| Crate | Release | Released | Declared MSRV | Source HEAD | Selection scope at research time |
| --- | --- | --- | --- | --- | --- |
| [abscissa_core](https://crates.io/crates/abscissa_core) | 0.9.0 | 2025-11-06 | 1.85 | [2026-02-17](https://github.com/iqlusioninc/abscissa) | Alternative framework or parser to evaluate |
| [anyhow](https://crates.io/crates/anyhow) | 1.0.104 | 2026-07-18 | 1.68 | [2026-08-22](https://github.com/dtolnay/anyhow) | Situational library |
| [assert_cmd](https://crates.io/crates/assert_cmd) | 2.2.2 | 2026-05-11 | 1.85 | [2026-09-01](https://github.com/assert-rs/assert_cmd) | Development/test dependency when needed |
| [assert_fs](https://crates.io/crates/assert_fs) | 1.1.4 | 2026-05-26 | 1.85 | [2026-09-01](https://github.com/assert-rs/assert_fs) | Development/test dependency when needed |
| [atomic-write-file](https://crates.io/crates/atomic-write-file) | 0.3.1 | 2026-08-11 | 1.85 | [2026-08-11](https://github.com/andreacorbellini/rust-atomic-write-file) | Situational library |
| [bpaf](https://crates.io/crates/bpaf) | 0.9.27 | 2026-07-29 | 1.68 | [2026-07-29](https://github.com/pacak/bpaf) | Alternative framework or parser to evaluate |
| [bstr](https://crates.io/crates/bstr) | 1.13.1 | 2026-08-10 | 1.65 | [2026-08-10](https://github.com/BurntSushi/bstr) | Preferred helper candidate for matching work |
| [bytes](https://crates.io/crates/bytes) | 1.12.1 | 2026-07-08 | 1.57 | [2026-09-03](https://github.com/tokio-rs/bytes) | Situational library |
| [camino](https://crates.io/crates/camino) | 1.2.5 | 2026-07-28 | 1.61.0 | [2026-07-28](https://github.com/camino-rs/camino) | Situational library |
| [clap](https://crates.io/crates/clap) | 4.6.6 | 2026-08-06 | 1.85 | [2026-09-01](https://github.com/clap-rs/clap) | Already in template runtime |
| [clap-cargo](https://crates.io/crates/clap-cargo) | 0.19.0 | 2026-08-18 | 1.86 | [2026-09-01](https://github.com/crate-ci/clap-cargo) | Alternative framework or parser to evaluate |
| [clap_complete](https://crates.io/crates/clap_complete) | 4.6.9 | 2026-08-06 | 1.85 | [2026-09-01](https://github.com/clap-rs/clap) | Already in template runtime |
| [clap_mangen](https://crates.io/crates/clap_mangen) | 0.3.3 | 2026-08-12 | 1.85 | [2026-09-01](https://github.com/clap-rs/clap) | Situational library |
| [cliclack](https://crates.io/crates/cliclack) | 0.5.6 | 2026-08-10 | Not declared | [2026-08-10](https://github.com/fadeevab/cliclack) | Situational library |
| [cling](https://crates.io/crates/cling) | 0.1.3 | 2025-05-04 | 1.77.0 | [2026-04-17](https://github.com/AhmedSoliman/cling) | Alternative framework or parser to evaluate |
| [color-eyre](https://crates.io/crates/color-eyre) | 0.6.5 | 2025-05-30 | 1.65.0 | [2026-08-10](https://github.com/eyre-rs/eyre) | Situational library |
| [config](https://crates.io/crates/config) | 0.15.25 | 2026-06-26 | 1.85.0 | [2026-09-01](https://github.com/rust-cli/config-rs) | Situational library |
| [console](https://crates.io/crates/console) | 0.16.4 | 2026-07-01 | 1.71 | [2026-09-05](https://github.com/console-rs/console) | Situational library |
| [crossterm](https://crates.io/crates/crossterm) | 0.29.0 | 2025-04-05 | 1.63.0 | [2026-08-21](https://github.com/crossterm-rs/crossterm) | Situational library |
| [csv](https://crates.io/crates/csv) | 1.4.0 | 2025-10-17 | 1.73 | [2026-08-04](https://github.com/BurntSushi/rust-csv) | Preferred helper candidate for matching work |
| [ctrlc](https://crates.io/crates/ctrlc) | 3.5.2 | 2026-02-10 | 1.69.0 | [2026-07-22](https://github.com/Detegr/rust-ctrlc) | Situational library |
| [derive_more](https://crates.io/crates/derive_more) | 2.1.1 | 2025-12-22 | 1.81.0 | [2026-05-09](https://github.com/JelteF/derive_more) | Situational library |
| [dialoguer](https://crates.io/crates/dialoguer) | 0.12.0 | 2025-08-23 | 1.66 | [2026-07-08](https://github.com/console-rs/dialoguer) | Situational library |
| [directories](https://crates.io/crates/directories) | 6.0.0 | 2025-01-12 | Not declared | [2026-03-20](https://codeberg.org/dirs/directories-rs) (Codeberg) | Situational library |
| [directories-next](https://crates.io/crates/directories-next) | 2.0.0 | 2020-10-22 | Not declared | [2021-04-23](https://github.com/xdg-rs/dirs) | Situational library; quieter observed activity |
| [dirs](https://crates.io/crates/dirs) | 7.0.0 | 2026-09-05 | Not declared | [2026-09-05](https://codeberg.org/dirs/dirs-rs) (Codeberg) | Situational library |
| [duct](https://crates.io/crates/duct) | 1.1.2 | 2026-09-03 | Not declared | [2026-09-03](https://github.com/oconnor663/duct.rs) | Situational library |
| [dunce](https://crates.io/crates/dunce) | 1.0.5 | 2024-08-04 | Not declared | Not independently checked | Situational library |
| [encoding_rs](https://crates.io/crates/encoding_rs) | 0.8.40 | 2026-09-07 | 1.88 | [2026-09-07](https://github.com/hsivonen/encoding_rs) | Situational library |
| [env_logger](https://crates.io/crates/env_logger) | 0.11.11 | 2026-06-25 | 1.71 | [2026-09-01](https://github.com/rust-cli/env_logger) | Situational library |
| [etcetera](https://crates.io/crates/etcetera) | 0.11.0 | 2025-10-28 | 1.87.0 | [2026-08-04](https://github.com/lunacookies/etcetera) | Situational library |
| [figment](https://crates.io/crates/figment) | 0.10.19 | 2024-05-17 | Not declared | [2024-09-13](https://github.com/SergioBenitez/Figment) | Situational library; quieter observed activity |
| [fs-err](https://crates.io/crates/fs-err) | 3.3.1 | 2026-07-03 | Not declared | [2026-07-03](https://github.com/andrewhickman/fs-err) | Situational library |
| [fs4](https://crates.io/crates/fs4) | 1.1.0 | 2026-04-28 | 1.75.0 | [2026-04-28](https://github.com/al8n/fs4) | Situational library |
| [globset](https://crates.io/crates/globset) | 0.4.20 | 2026-08-04 | 1.88 | [2026-08-04](https://github.com/BurntSushi/ripgrep) | Preferred helper candidate for matching work |
| [humantime](https://crates.io/crates/humantime) | 2.4.0 | 2026-07-02 | 1.60 | [2026-07-13](https://github.com/chronotope/humantime) | Preferred helper candidate for matching work |
| [ignore](https://crates.io/crates/ignore) | 0.4.33 | 2026-08-04 | 1.88 | [2026-08-04](https://github.com/BurntSushi/ripgrep) | Situational library |
| [indexmap](https://crates.io/crates/indexmap) | 2.14.2 | 2026-09-05 | 1.85 | [2026-09-05](https://github.com/indexmap-rs/indexmap) | Preferred helper candidate for matching work |
| [indicatif](https://crates.io/crates/indicatif) | 0.18.6 | 2026-07-01 | 1.85 | [2026-09-07](https://github.com/console-rs/indicatif) | Situational library |
| [inquire](https://crates.io/crates/inquire) | 0.9.4 | 2026-02-24 | 1.80.0 | [2026-02-24](https://github.com/mikaelmello/inquire) | Situational library |
| [insta](https://crates.io/crates/insta) | 1.48.0 | 2026-06-11 | 1.66.0 | [2026-09-06](https://github.com/mitsuhiko/insta) | Development/test dependency when needed |
| [itertools](https://crates.io/crates/itertools) | 0.15.0 | 2026-06-16 | 1.63.0 | [2026-09-05](https://github.com/rust-itertools/itertools) | Preferred helper candidate for matching work |
| [jiff](https://crates.io/crates/jiff) | 0.2.35 | 2026-07-25 | 1.70 | [2026-08-07](https://github.com/BurntSushi/jiff) | Preferred helper candidate for matching work |
| [lexopt](https://crates.io/crates/lexopt) | 0.3.2 | 2026-02-28 | Not declared | [2026-02-28](https://github.com/blyxxyz/lexopt) | Alternative framework or parser to evaluate |
| [log](https://crates.io/crates/log) | 0.4.34 | 2026-08-22 | 1.71.0 | [2026-08-22](https://github.com/rust-lang/log) | Situational library |
| [memchr](https://crates.io/crates/memchr) | 2.8.3 | 2026-07-08 | 1.61 | [2026-08-10](https://github.com/BurntSushi/memchr) | Already in template runtime |
| [miette](https://crates.io/crates/miette) | 7.6.0 | 2025-04-27 | 1.70.0 | [2026-06-25](https://github.com/zkat/miette) | Situational library |
| [once_cell](https://crates.io/crates/once_cell) | 1.21.4 | 2026-03-12 | 1.65 | [2026-03-12](https://github.com/matklad/once_cell) | Situational library |
| [pico-args](https://crates.io/crates/pico-args) | 0.5.0 | 2022-06-04 | Not declared | [2023-10-19](https://github.com/RazrFalcon/pico-args) | Situational library; quieter observed activity |
| [predicates](https://crates.io/crates/predicates) | 3.1.4 | 2026-02-11 | 1.74 | [2026-09-01](https://github.com/assert-rs/predicates-rs) | Development/test dependency when needed |
| [process-wrap](https://crates.io/crates/process-wrap) | 10.0.0 | 2026-08-24 | 1.87.0 | [2026-08-24](https://github.com/watchexec/process-wrap) | Situational library |
| [proptest](https://crates.io/crates/proptest) | 1.11.0 | 2026-03-24 | 1.85 | [2026-08-21](https://github.com/proptest-rs/proptest) | Development/test dependency when needed |
| [ratatui](https://crates.io/crates/ratatui) | 0.30.2 | 2026-06-19 | 1.88.0 | [2026-09-06](https://github.com/ratatui/ratatui) | Situational library |
| [rayon](https://crates.io/crates/rayon) | 1.12.0 | 2026-04-14 | 1.80 | [2026-08-28](https://github.com/rayon-rs/rayon) | Situational library |
| [regex](https://crates.io/crates/regex) | 1.13.1 | 2026-07-15 | 1.65 | [2026-08-10](https://github.com/rust-lang/regex) | Preferred helper candidate for matching work |
| [regex-lite](https://crates.io/crates/regex-lite) | 0.1.9 | 2026-02-03 | 1.65 | [2026-08-10](https://github.com/rust-lang/regex) | Preferred helper candidate for matching work |
| [reqwest](https://crates.io/crates/reqwest) | 0.13.4 | 2026-05-25 | 1.85.0 | [2026-09-07](https://github.com/seanmonstar/reqwest) | Situational library |
| [semver](https://crates.io/crates/semver) | 1.0.28 | 2026-04-04 | 1.68 | [2026-06-24](https://github.com/dtolnay/semver) | Preferred helper candidate for matching work |
| [serde](https://crates.io/crates/serde) | 1.0.229 | 2026-07-18 | 1.56 | [2026-08-25](https://github.com/serde-rs/serde) | Already in template runtime |
| [serde_json](https://crates.io/crates/serde_json) | 1.0.151 | 2026-07-20 | 1.71 | [2026-08-08](https://github.com/serde-rs/json) | Already in template runtime |
| [signal-hook](https://crates.io/crates/signal-hook) | 0.4.4 | 2026-04-04 | 1.66 | [2026-04-04](https://github.com/vorner/signal-hook) | Situational library |
| [smallvec](https://crates.io/crates/smallvec) | 1.16.0 | 2026-09-01 | Not declared | [2026-09-08](https://github.com/servo/rust-smallvec) | Situational library |
| [snapbox](https://crates.io/crates/snapbox) | 1.2.2 | 2026-05-26 | 1.85 | [2026-09-01](https://github.com/assert-rs/snapbox) | Development/test dependency when needed |
| [strum](https://crates.io/crates/strum) | 0.28.0 | 2026-02-22 | 1.71 | [2026-03-07](https://github.com/Peternator7/strum) | Situational library |
| [tabled](https://crates.io/crates/tabled) | 0.22.0 | 2026-09-05 | Not declared | [2026-09-05](https://github.com/zhiburt/tabled) | Situational library |
| [tap](https://crates.io/crates/tap) | 1.0.1 | 2021-02-13 | Not declared | [2021-02-13](https://github.com/myrrlyn/tap) | Situational library; quieter observed activity |
| [tempfile](https://crates.io/crates/tempfile) | 3.27.0 | 2026-03-11 | 1.63 | [2026-09-05](https://github.com/Stebalien/tempfile) | Development/test dependency when needed |
| [thiserror](https://crates.io/crates/thiserror) | 2.0.20 | 2026-08-08 | 1.71 | [2026-09-05](https://github.com/dtolnay/thiserror) | Already in template runtime |
| [tokio](https://crates.io/crates/tokio) | 1.53.1 | 2026-07-20 | 1.71 | [2026-09-08](https://github.com/tokio-rs/tokio) | Situational library |
| [toml](https://crates.io/crates/toml) | 1.1.5+spec-1.1.0 | 2026-09-02 | 1.85 | [2026-09-03](https://github.com/toml-rs/toml) | Already in template runtime |
| [tracing](https://crates.io/crates/tracing) | 0.1.44 | 2025-12-18 | 1.65.0 | [2026-05-30](https://github.com/tokio-rs/tracing) | Situational library |
| [tracing-subscriber](https://crates.io/crates/tracing-subscriber) | 0.3.23 | 2026-03-13 | 1.65.0 | [2026-05-30](https://github.com/tokio-rs/tracing) | Situational library |
| [trycmd](https://crates.io/crates/trycmd) | 1.2.1 | 2026-07-21 | 1.85 | [2026-09-01](https://github.com/assert-rs/snapbox) | Development/test dependency when needed |
| [ureq](https://crates.io/crates/ureq) | 3.4.1 | 2026-09-06 | 1.85 | [2026-09-06](https://github.com/algesten/ureq) | Situational library |
| [url](https://crates.io/crates/url) | 2.5.8 | 2026-01-05 | 1.63 | [2026-07-31](https://github.com/servo/rust-url) | Preferred helper candidate for matching work |
| [walkdir](https://crates.io/crates/walkdir) | 2.5.0 | 2024-03-01 | Not declared | [2024-12-31](https://github.com/BurntSushi/walkdir) | Situational library; quieter observed activity |
| [which](https://crates.io/crates/which) | 8.0.6 | 2026-08-26 | 1.70 | [2026-09-01](https://github.com/harryfei/which-rs) | Situational library |
| [xdg](https://crates.io/crates/xdg) | 3.0.0 | 2025-05-04 | 1.60.0 | [2025-05-04](https://github.com/whitequark/rust-xdg) | Situational library |

A missing declared MSRV does not prove compatibility. Download counts and Git activity are supporting observations rather than guarantees of maintenance or quality. For migrated projects, check the current source host.
