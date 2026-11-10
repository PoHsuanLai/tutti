//! The recipes: each one a plan of steps, the justfile's lines in the
//! justfile's order. Planning is pure (it reads nothing but the arguments and
//! the `Ctx`), so `--dry-run` and the tests see exactly what a run would do.

use crate::cmd::Cmd;
use crate::gates::Gate;
use std::collections::HashSet;
use std::path::PathBuf;

/// Where the plan runs: the repo root and the cargo target directory.
#[derive(Debug, Clone)]
pub(crate) struct Ctx {
    pub(crate) root: PathBuf,
    pub(crate) target: PathBuf,
}

/// One thing a recipe does, stopping the recipe if it fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    Run(Cmd),
    /// Write a file (`check-windows`'s stub header), creating its directory.
    WriteFile {
        path: PathBuf,
        contents: String,
    },
    /// `check-bevy-free`'s dependency tree check: fails if the tree printed by
    /// the command names a bevy crate. The command's own status is ignored, as
    /// the justfile's pipeline ignored it (the `cargo check` before it had
    /// already built the same graph).
    NoBevyInTree(Cmd),
    /// A gate over the repo's files (`gates.rs`), run in this process.
    Gate(Gate),
    /// `cargo doc` for each feature of the feature-gated crates, one at a
    /// time (`gates/doc_features.rs`); every run is attempted and the
    /// failures reported together.
    DocFeatures,
    /// `deny`'s probe: fails with the install hint when cargo-deny is missing.
    RequireCargoDeny,
}

impl Step {
    /// The step as a line a shell could read (for `--dry-run` and the echo).
    pub(crate) fn display(&self) -> String {
        match self {
            Step::Run(c) => c.display(),
            Step::WriteFile { path, .. } => format!("write {}", path.display()),
            Step::NoBevyInTree(c) => format!(
                "{} | fail if a line matches '\\bbevy(_[a-z]+)?\\b'",
                c.display()
            ),
            Step::Gate(g) => g.display(),
            Step::DocFeatures => "doc-features: cargo doc --locked -p <crate> --no-deps --features <f>, for each feature of tutti-session and tutti".to_owned(),
            Step::RequireCargoDeny => {
                "cargo deny --version || fail (cargo install cargo-deny --version 0.20.2 --locked)"
                    .to_owned()
            }
        }
    }
}

/// `(name, arguments, what it does)`, in the justfile's order.
pub(crate) const RECIPES: &[(&str, &str, &str)] = &[
    (
        "test",
        "[nextest args]",
        "the workspace suite the way CI runs it (nextest; builds plugin-server and plugin-probe first)",
    ),
    (
        "test-au-installed",
        "",
        "the AU mapping-family claim; needs third-party Audio Units installed (macOS)",
    ),
    (
        "test-editor",
        "",
        "the custom-harness editor tests (VST3 and AU)",
    ),
    (
        "test-editor-no-corpus",
        "",
        "test-editor without the installed-plugin corpus case",
    ),
    (
        "test-doc",
        "[cargo test args]",
        "doctests (nextest does not run them)",
    ),
    (
        "test-release",
        "",
        "the release-profile suites of the lock-free crates",
    ),
    (
        "test-all",
        "",
        "test, test-features, test-editor, test-au-installed, test-doc, test-release",
    ),
    (
        "check-features",
        "",
        "clippy for every feature combination a workspace run leaves dark",
    ),
    (
        "test-features",
        "",
        "run the suites a workspace run leaves dark (see check-features)",
    ),
    (
        "miri",
        "",
        "miri over tutti-rt and arc-lend (needs nightly + miri)",
    ),
    (
        "loom",
        "[<model>...]",
        "the loom models, as CI runs them (or only those named, e.g. handoff_loom)",
    ),
    (
        "loom-full",
        "",
        "the ArcLend loom models at the deeper preemption bound",
    ),
    (
        "check-jack",
        "",
        "clippy for tutti-cpal's jack feature (needs libjack headers)",
    ),
    (
        "pressure",
        "",
        "the out-of-process plugin bridge at real callback pacing",
    ),
    (
        "bench",
        "[criterion args]",
        "every criterion bench, one per line",
    ),
    (
        "bench-save",
        "[NAME=main]",
        "record a named baseline on this machine",
    ),
    (
        "bench-cmp",
        "[BASE=main]",
        "compare against a recorded baseline",
    ),
    (
        "bench-smoke",
        "",
        "run every bench exactly once, no measurement",
    ),
    (
        "bench-compare",
        "[harness args]",
        "arc-lend against arc-swap, basedrop, left-right and locks",
    ),
    (
        "profile-bench",
        "[BENCH=engine_render [PKG=tutti-nodes [SECS=10]]]",
        "samply a criterion bench",
    ),
    ("plugin-server", "", "build the plugin-server binary"),
    ("plugin-probe", "", "build the plugin-probe binary"),
    (
        "plugin-server-release",
        "",
        "build plugin-server in release",
    ),
    (
        "check-bevy-free",
        "",
        "the Bevy-free floor: builds and the dependency tree",
    ),
    ("check-adapter", "", "scripts/check-adapter.sh"),
    ("check-guards", "", "scripts/check-guards.sh"),
    ("check-plugin-ffi", "", "scripts/check-plugin-ffi.sh"),
    ("check-layout", "", "scripts/check-layout.sh"),
    (
        "check-range-types",
        "",
        "no plain derived Deserialize on a private tuple field",
    ),
    ("check-hosting", "", "scripts/check-hosting.sh"),
    (
        "check-paths",
        "[<crate> <module>...]",
        "engine types are imported from a crate's root, never through its modules (also searches *.md)",
    ),
    (
        "check-submodule-pins",
        "",
        "the VST3 SDK submodules are at the v3.8.0_build_66 SHAs",
    ),
    (
        "check-doc-features",
        "",
        "rustdoc per feature for tutti-session and tutti",
    ),
    ("lint", "", "cargo fmt --check and workspace clippy"),
    (
        "check-windows",
        "",
        "cross-lint for Windows (needs the msvc target, clang, llvm-ar)",
    ),
    (
        "doc",
        "",
        "rustdoc as CI does, per feature, and the docs.rs build on a dated nightly",
    ),
    ("check-loom", "", "clippy under --cfg loom"),
    (
        "verify-audio",
        "[OUT=/tmp/tutti-verify]",
        "render the audio cases and verify them with the Python checks (needs uv)",
    ),
    ("deny", "", "RustSec advisories (needs cargo-deny)"),
    ("ci", "", "everything CI runs"),
];

/// Plans `name` with `args`; an error is a usage error (exit 2).
pub(crate) fn plan(name: &str, args: &[String], ctx: &Ctx) -> Result<Vec<Step>, String> {
    let mut p = Planner {
        ctx,
        done: HashSet::new(),
        out: Vec::new(),
    };
    p.done.insert(name.to_owned());
    p.recipe(name, args)?;
    Ok(p.out)
}

struct Planner<'a> {
    ctx: &'a Ctx,
    /// Recipes already planned in this invocation: a dependency runs once.
    done: HashSet<String>,
    out: Vec<Step>,
}

const TARGET: &str = "x86_64-pc-windows-msvc";

impl Planner<'_> {
    fn run(&mut self, c: Cmd) {
        self.out.push(Step::Run(c));
    }

    fn lines<S: AsRef<str>>(&mut self, lines: &[S]) {
        for l in lines {
            self.run(Cmd::sh(l.as_ref()));
        }
    }

    /// A dependency: planned once per invocation, with no arguments.
    fn dep(&mut self, name: &str) -> Result<(), String> {
        if self.done.insert(name.to_owned()) {
            self.recipe(name, &[])?;
        }
        Ok(())
    }

    /// Exactly `N` positional parameters, each with its default.
    fn params<const N: usize>(
        name: &str,
        args: &[String],
        defaults: [&str; N],
    ) -> Result<[String; N], String> {
        if args.len() > N {
            return Err(format!(
                "`{name}` takes at most {N} argument(s), got {}",
                args.len()
            ));
        }
        Ok(std::array::from_fn(|i| {
            args.get(i)
                .cloned()
                .unwrap_or_else(|| defaults[i].to_owned())
        }))
    }

    fn recipe(&mut self, name: &str, args: &[String]) -> Result<(), String> {
        if !RECIPES.iter().any(|r| r.0 == name) {
            return Err(format!("no recipe `{name}`; `cargo xtask help` lists them"));
        }
        // Only these take free-form arguments or named parameters.
        let takes_args = matches!(
            name,
            "test"
                | "test-doc"
                | "bench"
                | "bench-compare"
                | "check-paths"
                | "loom"
                | "bench-save"
                | "bench-cmp"
                | "profile-bench"
                | "verify-audio"
        );
        if !takes_args && !args.is_empty() {
            return Err(format!(
                "`{name}` takes no arguments, got {}",
                args.join(" ")
            ));
        }
        match name {
            "test" => {
                self.dep("plugin-server")?;
                self.dep("plugin-probe")?;
                self.run(
                    Cmd::sh("cargo nextest run --locked --workspace").args(args).args([
                        "-E",
                        "not binary(gui_lifecycle_main) and not binary(au_gui_lifecycle_main)",
                    ]),
                );
            }
            "test-au-installed" => self.run(
                Cmd::sh("cargo nextest run --locked -p tutti-au-host --features installed-units --no-tests=pass")
                    .args(["-E", "test(no_installed_third_party_unit_implements_the_mapping_family)"]),
            ),
            "test-editor" => self.lines(&[
                "cargo test --locked -p tutti-vst3-host --test gui_lifecycle_main",
                "cargo test --locked -p tutti-au-host --test au_gui_lifecycle_main",
            ]),
            "test-editor-no-corpus" => self.lines(&[
                "TUTTI_GUI_SKIP=has_editor_agrees_with_opening_one cargo test --locked -p tutti-vst3-host --test gui_lifecycle_main",
                "cargo test --locked -p tutti-au-host --test au_gui_lifecycle_main",
            ]),
            "test-doc" => {
                self.run(Cmd::sh("cargo test --locked --doc --workspace").args(args));
                self.run(Cmd::sh("cargo test --locked --doc -p tutti-session --features device").args(args));
            }
            "test-release" => self.lines(&[
                "cargo nextest run --locked --release -p tutti-types -p tutti-rt -p arc-lend -p tutti-graph",
            ]),
            "test-all" => {
                for d in ["test", "test-features", "test-editor", "test-au-installed", "test-doc", "test-release"] {
                    self.dep(d)?;
                }
            }
            "check-features" => self.lines(CHECK_FEATURES),
            "test-features" => self.lines(TEST_FEATURES),
            "miri" => self.lines(MIRI),
            "loom" if args.is_empty() => self.lines(LOOM),
            "loom" => {
                // The whole name: `--test pos_ring_loom` names no `pos_ring`.
                let runs = |l: &str, a: &str| {
                    l.contains(&format!("--test {a} ")) || l.ends_with(&format!("--test {a}"))
                };
                if let Some(unknown) = args.iter().find(|a| !LOOM.iter().any(|l| runs(l, a))) {
                    return Err(format!("`loom` runs no model `{unknown}`"));
                }
                let chosen: Vec<&str> = LOOM
                    .iter()
                    .copied()
                    .filter(|l| args.iter().any(|a| runs(l, a)))
                    .collect();
                self.lines(&chosen);
            }
            "loom-full" => self.lines(LOOM_FULL),
            "check-jack" => self.lines(&[
                "cargo clippy --locked -p tutti-cpal --features jack --all-targets -- -D warnings",
            ]),
            "pressure" => self.lines(&[
                "cargo build --locked -p tutti-plugin-server",
                "cargo nextest run --locked -p tutti-plugin --features clap --test real_plugin_pressure --no-capture",
            ]),
            "bench" => self.bench(args),
            "bench-save" => {
                let [n] = Self::params(name, args, ["main"])?;
                self.bench(&["--".into(), "--save-baseline".into(), n]);
            }
            "bench-cmp" => {
                let [b] = Self::params(name, args, ["main"])?;
                self.bench(&["--".into(), "--baseline".into(), b]);
            }
            "bench-smoke" => {
                self.bench(&["--".into(), "--test".into()]);
                self.bench_compare(&["--test".into()]);
            }
            "bench-compare" => self.bench_compare(args),
            "profile-bench" => {
                let [bench, pkg, secs] =
                    Self::params(name, args, ["engine_render", "tutti-nodes", "10"])?;
                self.run(
                    Cmd::sh("cargo bench --locked -p")
                        .arg(&pkg)
                        .args(["--bench", &bench, "--profile", "profiling", "--no-run"]),
                );
                let mut samply = Cmd::sh("samply record")
                    .arg(format!("target/profiling/deps/{bench}-*"))
                    .args(["--bench", "--profile-time", &secs]);
                samply.glob = true;
                self.run(samply);
            }
            "plugin-server" => self.lines(&["cargo build --locked -p tutti-plugin-server --bin plugin-server"]),
            "plugin-probe" => self.lines(&["cargo build --locked -p tutti-plugin-probe --bin plugin-probe"]),
            "plugin-server-release" => {
                self.lines(&["cargo build --locked --release -p tutti-plugin-server --bin plugin-server"]);
            }
            "check-bevy-free" => {
                self.lines(&[
                    "cargo check --locked -p tutti-transport -p tutti-metering -p tutti-engine",
                    "cargo check --locked -p tutti-types",
                    "cargo check --locked -p tutti-types --features bevy",
                    "cargo check --locked -p tutti",
                    "cargo check --locked -p tutti --features full",
                ]);
                self.out.push(Step::NoBevyInTree(Cmd::sh(
                    "cargo tree --locked -p tutti --features full -e normal",
                )));
                self.lines(&["scripts/check-bevy-free.sh"]);
            }
            "check-adapter" => self.lines(&["scripts/check-adapter.sh"]),
            "check-guards" => self.lines(&["scripts/check-guards.sh"]),
            "check-plugin-ffi" => self.lines(&["scripts/check-plugin-ffi.sh"]),
            "check-layout" => self.lines(&["scripts/check-layout.sh"]),
            "check-range-types" => self.out.push(Step::Gate(Gate::RangeTypes)),
            "check-submodule-pins" => self.out.push(Step::Gate(Gate::SubmodulePins)),
            "check-doc-features" => self.out.push(Step::DocFeatures),
            "check-hosting" => self.lines(&["scripts/check-hosting.sh"]),
            "check-paths" => self.out.push(Step::Gate(Gate::CanonicalPaths(args.to_vec()))),
            "lint" => self.lines(&[
                "cargo fmt --all --check",
                "cargo clippy --locked --workspace --all-targets -- -D warnings",
            ]),
            "check-windows" => self.check_windows(),
            "doc" => {
                self.lines(&[
                    r#"RUSTDOCFLAGS="-D warnings --document-private-items" cargo doc --locked --workspace --no-deps"#,
                    r#"RUSTDOCFLAGS="-D warnings" cargo doc --locked -p tutti-graph --no-deps --features contract"#,
                    r#"RUSTDOCFLAGS="-D warnings" cargo doc --locked -p tutti-dsp --no-deps --features contract"#,
                ]);
                self.out.push(Step::DocFeatures);
                self.lines(&[
                    r#"RUSTDOCFLAGS="-D warnings --cfg docsrs" cargo +nightly-2026-10-14 doc --locked --workspace --no-deps"#,
                ]);
            }
            "check-loom" => self.lines(&[
                r#"RUSTFLAGS="--cfg loom" cargo clippy --locked -p arc-lend -p tutti-rt -p tutti-shm-model --all-targets -- -D warnings"#,
            ]),
            "verify-audio" => {
                let [out] = Self::params(name, args, ["/tmp/tutti-verify"])?;
                self.lines(&[
                    format!("cargo run --locked --release -p tutti-sampler --example render_cases -- {out}/sampler"),
                    format!("cargo run --locked --release -p tutti-export --example render_export_cases -- {out}/export"),
                    format!("cargo run --locked --release -p tutti-measure --example render_analysis_cases -- {out}/analysis"),
                    format!("cargo run --locked --release -p tutti-polysynth --example render_synth_cases -- {out}/synth"),
                    "uv sync --quiet".to_owned(),
                    format!("uv run python crates/tutti-sampler/examples/verify_sampler.py {out}/sampler"),
                    format!("uv run python crates/tutti-export/examples/verify_export.py {out}/export"),
                    format!("uv run python crates/tutti-measure/examples/verify_analysis.py {out}/analysis"),
                    format!("uv run python crates/tutti-polysynth/examples/verify_synth.py {out}/synth"),
                ]);
            }
            "deny" => {
                self.out.push(Step::RequireCargoDeny);
                self.lines(&["cargo deny --locked check advisories"]);
            }
            "ci" => {
                for d in [
                    "lint", "deny", "check-paths", "check-bevy-free", "check-adapter", "check-guards",
                    "check-plugin-ffi", "check-layout", "check-range-types", "check-hosting",
                    "check-submodule-pins",
                    "check-features",
                    "check-loom", "test", "test-features", "test-editor", "test-doc",
                    "test-release", "doc",
                ] {
                    self.dep(d)?;
                }
            }
            _ => unreachable!("RECIPES and this match list the same names"),
        }
        Ok(())
    }

    /// Every bench, one per line; `args` follow each.
    fn bench(&mut self, args: &[String]) {
        for l in BENCH {
            self.run(Cmd::sh(l).args(args));
        }
    }

    fn bench_compare(&mut self, args: &[String]) {
        self.run(Cmd::sh("cargo bench --locked -p arc-lend-compare --bench compare --").args(args));
    }

    /// The justfile wrote the stub header to `${TMPDIR:-/tmp}/tutti-winstub`;
    /// it is under the target directory now, so a run touches nothing outside
    /// the repo's build output.
    fn check_windows(&mut self) {
        let stub = self.ctx.target.join("tutti-winstub");
        self.out.push(Step::WriteFile {
            path: stub.join("malloc.h"),
            contents: "#include <stddef.h>\nvoid *_alloca(size_t);\n".to_owned(),
        });
        let cflags = format!("-I{}", stub.display());
        let mut win = |args: &str| {
            self.out.push(Step::Run(
                Cmd::sh(&format!(
                    "cargo clippy --locked --target {TARGET} {args} -- -D warnings"
                ))
                .env("CC_x86_64_pc_windows_msvc", "clang")
                .env("AR_x86_64_pc_windows_msvc", "llvm-ar")
                .env("CFLAGS_x86_64_pc_windows_msvc", cflags.clone()),
            ));
        };
        win(
            "--workspace --all-targets --exclude tutti-codec --exclude tutti-export --exclude tutti --exclude tutti-vst3-host --exclude tutti-plugin-server --exclude tutti-plugin-probe --exclude tutti-shared-plugin-vst3-cpp --exclude tutti-plugin-contract",
        );
        for a in [
            "-p tutti-shared-plugin --all-targets",
            "-p tutti-export --lib --features wav,flac,aiff",
            "-p tutti-codec --features encode-wav,encode-aiff,encode-flac,decode-wav,decode-flac,decode-mp3,decode-ogg,serde --all-targets",
            "-p tutti-plugin -p tutti-plugin-types --features tutti-plugin/clap,tutti-plugin/vst3,tutti-plugin/vst2,tutti-plugin/json --all-targets",
            "-p tutti-plugin-db --features json --all-targets",
            "-p tutti-clap-host --features clap-extras --all-targets",
            "-p tutti-vst2-host --all-targets",
            "-p tutti-vst3-host -p tutti-plugin-server -p tutti-plugin-probe --lib --bins",
        ] {
            win(a);
        }
    }
}

const CHECK_FEATURES: &[&str] = &[
    r#"cargo clippy --locked -p tutti-au-host --features installed-units --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-cpal --features capture --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-session --features audio-io --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-session --features clap,export,modulation,midi-hardware,synth,soundfont,sampler,vst2,device --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti --features clap,export,modulation --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti --features audio-io --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti --no-default-features --features io --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-export --lib -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-export --lib --features wav -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-export --lib --features flac -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-export --lib --features aiff -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-export --lib --features ogg -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-session --lib --features export-flac,export-aiff,export-ogg -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-codec --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-codec --features encode-wav --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-codec --features encode-aiff --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-codec --features encode-flac --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-codec --features encode-vorbis --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-codec --lib --features decode-wav -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-codec --lib --features decode-flac -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-codec --lib --features decode-mp3 -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-codec --lib --features decode-ogg -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-codec --all-features --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-ump --features serde --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-midi-protocol --features serde --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-midi-device --features serde --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-tempo --features serde --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-dsp --lib -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-dsp --lib --features contract -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-midi-hardware --features sequencer-tests --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-clap-ffi --lib -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-clap-host --features clap-extras --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-plugin --features clap,vst2,json --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-plugin-db --features json --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-plugin-contract --features server --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-plugin-server --no-default-features --features vst2 --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-plugin-server --no-default-features --features vst3 --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-plugin-server --no-default-features --features clap --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-plugin-probe --no-default-features --features vst2 --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-plugin-probe --no-default-features --features vst3 --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-plugin-probe --no-default-features --features clap --all-targets -- -D warnings"#,
];

const TEST_FEATURES: &[&str] = &[
    r#"cargo nextest run --locked -p tutti-au-host --features installed-units --no-tests=pass"#,
    r#"cargo build --locked -p tutti-plugin-server --no-default-features --features clap,vst2,vst3"#,
    r#"cargo build --locked -p tutti-plugin-probe"#,
    r#"cargo nextest run --locked -p tutti-cpal --features capture"#,
    r#"cargo nextest run --locked -p tutti-session --features audio-io"#,
    r#"cargo nextest run --locked -p tutti-session --features clap,export,modulation,midi-hardware,synth,soundfont,sampler,vst2,device"#,
    r#"cargo nextest run --locked -p tutti --features clap,export,modulation"#,
    r#"cargo nextest run --locked -p tutti --features audio-io"#,
    r#"cargo nextest run --locked -p tutti --no-default-features --features io"#,
    r#"cargo nextest run --locked -p tutti-session --features export-flac,export-aiff,export-ogg"#,
    r#"cargo nextest run --locked -p tutti-codec"#,
    r#"cargo nextest run --locked -p tutti-codec --features encode-wav"#,
    r#"cargo nextest run --locked -p tutti-codec --features encode-aiff"#,
    r#"cargo nextest run --locked -p tutti-codec --features encode-flac"#,
    r#"cargo nextest run --locked -p tutti-codec --features encode-vorbis"#,
    r#"cargo nextest run --locked -p tutti-codec --all-features"#,
    r#"cargo nextest run --locked -p tutti-ump --features serde"#,
    r#"cargo nextest run --locked -p tutti-midi-protocol --features serde"#,
    r#"cargo nextest run --locked -p tutti-midi-device --features serde"#,
    r#"cargo nextest run --locked -p tutti-tempo --features serde"#,
    r#"cargo nextest run --locked -p tutti-midi-hardware --features sequencer-tests"#,
    r#"cargo nextest run --locked -p tutti-clap-host --features clap-extras"#,
    r#"cargo nextest run --locked -p tutti-plugin-types --features test-support"#,
    r#"cargo nextest run --locked -p tutti-plugin-db --features json"#,
    r#"cargo nextest run --locked -p tutti-plugin-contract --features server"#,
    r#"cargo nextest run --locked -p tutti-plugin --features clap,vst2,json"#,
];

const MIRI: &[&str] = &[
    r#"cargo +nightly miri test --locked -p tutti-rt --lib"#,
    r#"MIRIFLAGS="-Zmiri-many-seeds=0..16" cargo +nightly miri test --locked -p tutti-rt --lib edits"#,
    r#"cargo +nightly miri test --locked -p arc-lend"#,
    r#"MIRIFLAGS="-Zmiri-many-seeds=0..16" cargo +nightly miri test --locked -p arc-lend --lib"#,
];

const LOOM: &[&str] = &[
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p arc-lend --release --test loom -- --skip registering_and_dropping_readers"#,
    r#"LOOM_MAX_PREEMPTIONS=4 RUSTFLAGS="--cfg loom" cargo test --locked -p arc-lend --release --test loom -- registering_and_dropping_readers --exact"#,
    r#"LOOM_MAX_PREEMPTIONS=4 RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test pos_ring_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test packed_atomic_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test seq_cell_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test cell_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test handoff_loom -- --nocapture"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-shm-model --release"#,
    r#"LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test split_loom"#,
    r#"LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test job_gate_loom"#,
    r#"LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test task_graph_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test doorbell_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test request_word_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test edit_ring_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test drain_clock_loom -- --nocapture"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test fault_latch_loom -- --nocapture"#,
];

const LOOM_FULL: &[&str] = &[
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p arc-lend --release --test loom -- --skip registering_and_dropping_readers"#,
    r#"LOOM_MAX_PREEMPTIONS=5 RUSTFLAGS="--cfg loom" cargo test --locked -p arc-lend --release --test loom -- registering_and_dropping_readers --exact"#,
];

/// Adding a bench means adding a line here: there is no sweep that would pick
/// one up silently (the justfile's contract, which CI mirrors).
const BENCH: &[&str] = &[
    r#"cargo bench --locked -p tutti-nodes     --bench engine_render"#,
    r#"cargo bench --locked -p tutti-graph     --bench graph_render"#,
    r#"cargo bench --locked -p tutti-cpal      --bench audio_callback"#,
    r#"cargo bench --locked -p tutti-export    --bench offline_render"#,
    r#"cargo bench --locked -p tutti-polysynth --bench polysynth"#,
    r#"cargo bench --locked -p tutti-ump       --bench codec"#,
    r#"cargo bench --locked -p tutti-midi-runtime --bench clip_node"#,
    r#"cargo bench --locked -p tutti-sampler   --bench voice_pool"#,
    r#"cargo bench --locked -p tutti-plugin --features vst2 --bench vst2_in_process"#,
    r#"cargo bench --locked -p arc-lend --bench lend"#,
];

#[cfg(test)]
mod tests;
