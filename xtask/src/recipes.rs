//! The recipes: each one a plan of steps, the justfile's lines in the
//! justfile's order. Planning is pure (it reads nothing but the arguments and
//! the `Ctx`), so `--dry-run` and the tests see exactly what a run would do.

use crate::cmd::Cmd;
use crate::gates::Gate;
use crate::release::Version;
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
    /// A gate over the repo's files (`gates.rs`), run in this process.
    Gate(Gate),
    /// `cargo doc` for each member's features together and each feature of
    /// the feature-gated crates alone (`gates/doc_features.rs`); every run is
    /// attempted and the failures reported together.
    DocFeatures,
    /// `conformance-inventory <format>`: rewrite an inventory from its spec source.
    ConformanceInventory(String),
    /// `conformance`: the dashboard and badges from per-OS JUnit files
    /// (`gates/conformance_site.rs`).
    Conformance {
        results: PathBuf,
        out: PathBuf,
    },
    /// Run the command with the environment `cargo llvm-cov show-env` prints,
    /// so it builds into the target directory the instrumented tests run from.
    InLlvmCovEnv(Cmd),
    /// `coverage`'s probe: fails with the install hint when cargo-llvm-cov is missing.
    RequireLlvmCov,
    /// `deny`'s probe: fails with the install hint when cargo-deny is missing.
    RequireCargoDeny,
    /// The release edits (`release.rs`): the workspace version, the path
    /// dependency pins and the changelog section.
    Release(Version),
    /// `cargo package --no-verify` over every crate under `crates/`, listed
    /// when the step runs.
    Package,
}

impl Step {
    /// The step as a line a shell could read (for `--dry-run` and the echo).
    pub(crate) fn display(&self) -> String {
        match self {
            Step::Run(c) => c.display(),
            Step::WriteFile { path, .. } => format!("write {}", path.display()),
            Step::Gate(g) => g.display(),
            Step::DocFeatures => "doc-features: cargo doc --locked -p <crate> --no-deps --features <fs>, each member with its features together (private items), then each feature of tutti-session and tutti alone".to_owned(),
            Step::ConformanceInventory(f) => format!("conformance-inventory {f}"),
            Step::Conformance { results, out } => format!(
                "conformance: join {}/<os>.xml to the inventories, write {}/index.html, ../index.html and ../badges/*.json",
                results.display(),
                out.display()
            ),
            Step::InLlvmCovEnv(c) => format!("(cargo llvm-cov show-env) {}", c.display()),
            Step::RequireLlvmCov => {
                "cargo llvm-cov --version || fail (cargo install cargo-llvm-cov --locked)".to_owned()
            }
            Step::RequireCargoDeny => {
                "cargo deny --version || fail (cargo install cargo-deny --version 0.20.2 --locked)"
                    .to_owned()
            }
            Step::Package => {
                "cargo package --locked --no-verify -p <each crate under crates/>".to_owned()
            }
            Step::Release(v) => format!(
                "release {}: set the workspace version and path pins, write the changelog section",
                v.display()
            ),
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
        "test, test-features, test-doc, test-release",
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
    ("miri", "", "miri over tutti-rt (needs nightly + miri)"),
    (
        "loom",
        "[<model>...]",
        "the loom models, as CI runs them (or only those named, e.g. handoff_loom)",
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
        "check-session",
        "",
        "tutti-session re-exports no crate whole",
    ),
    (
        "check-guards",
        "",
        "every plugin host callback runs under the panic guard",
    ),
    (
        "check-ffi",
        "",
        "unsafe lives only in the -ffi crates (plugin formats and MIDI), everything else forbids it",
    ),
    (
        "check-errors",
        "",
        "every error type derives thiserror::Error, and a public one is an Error",
    ),
    (
        "check-lints",
        "",
        "every crate takes the workspace lints and missing_debug_implementations denies",
    ),
    (
        "check-value-traits",
        "",
        "a fieldless public Eq enum derives Hash",
    ),
    (
        "check-layout",
        "",
        "the repo layout, the crate layering, the codec boundary and the depth-free test paths",
    ),
    (
        "check-doc-refs",
        "",
        "prose names crate paths and design-doc sections that exist",
    ),
    (
        "check-pointers",
        "",
        "no file:line pointer in code prose; one under docs/ names a line that exists",
    ),
    (
        "check-platform-seam",
        "",
        "a platform cfg stands only on a mod declaration, bar the temporary rows",
    ),
    (
        "check-conformance",
        "",
        "spec inventories are well formed, test tags name rows, the CLAP inventory matches clap-sys and the VST3 one the SDK",
    ),
    (
        "conformance",
        "--results <dir> [--out site/conformance]",
        "the spec conformance dashboard and badges (one table per inventory area) from per-OS nextest JUnit files (<dir>/linux.xml, macos.xml, windows.xml)",
    ),
    (
        "coverage",
        "[--out site/coverage] [--package a,b]",
        "line coverage of the workspace as HTML (needs cargo-llvm-cov)",
    ),
    (
        "conformance-inventory",
        "[clap|vst3|midi|sf2]",
        "regenerate an inventory's generated columns from the spec source (vst3: the vendored SDK; midi: TUTTI_MIDI_SPECS; sf2: TUTTI_SF2_SPEC)",
    ),
    (
        "check-range-types",
        "",
        "no plain derived Deserialize on a private tuple field",
    ),
    (
        "check-hosting",
        "",
        "every InProcess/OutOfProcess impl is listed in the plugin contract",
    ),
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
        "rustdoc over every member's features together, and per feature for tutti-session and tutti",
    ),
    (
        "gates",
        "",
        "rustfmt, xtask's own tests and every source gate: no workspace build (the pre-push hook)",
    ),
    ("fmt", "", "cargo fmt --check"),
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
    (
        "book",
        "[OUT]",
        "build the book with mdbook into book/build, or into OUT (relative to book/)",
    ),
    (
        "package",
        "",
        "package every crate under crates/ the way crates.io takes it, without building",
    ),
    (
        "release",
        "<version>",
        "a release pull request's edits: the shared version, the path pins and the changelog section",
    ),
    ("ci", "", "everything CI runs"),
];

/// `--key value` pairs, each key one of `allowed` and given once.
fn options(
    recipe: &str,
    args: &[String],
    allowed: &[&str],
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let mut out = std::collections::BTreeMap::new();
    let mut it = args.iter();
    while let Some(key) = it.next() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!(
                "`{recipe}` takes {}, got `{key}`",
                allowed.join(", ")
            ));
        }
        let value = it
            .next()
            .ok_or_else(|| format!("`{recipe}`: `{key}` needs a value"))?;
        if out.insert(key.clone(), value.clone()).is_some() {
            return Err(format!("`{recipe}`: `{key}` is given twice"));
        }
    }
    Ok(out)
}

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
                | "check-paths"
                | "loom"
                | "bench-save"
                | "bench-cmp"
                | "profile-bench"
                | "verify-audio"
                | "book"
                | "release"
                | "conformance-inventory"
                | "conformance"
                | "coverage"
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
                self.run(Cmd::sh("cargo nextest run --locked --workspace").args(args));
            }
            "test-doc" => {
                self.run(Cmd::sh("cargo test --locked --doc --workspace").args(args));
                self.run(Cmd::sh("cargo test --locked --doc -p tutti-session --all-features").args(args));
            }
            "test-release" => self.lines(&[
                "cargo nextest run --locked --release -p tutti-types -p tutti-rt -p tutti-graph",
            ]),
            "test-all" => {
                for d in ["test", "test-features", "test-doc", "test-release"] {
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
            }
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
            "check-session" => self.out.push(Step::Gate(Gate::Session)),
            "check-guards" => self.out.push(Step::Gate(Gate::Guards)),
            "check-ffi" => self.out.push(Step::Gate(Gate::Ffi)),
            "check-errors" => self.out.push(Step::Gate(Gate::Errors)),
            "check-layout" => self.out.push(Step::Gate(Gate::Layout)),
            "check-lints" => self.out.push(Step::Gate(Gate::Lints)),
            "check-value-traits" => self.out.push(Step::Gate(Gate::ValueTraits)),
            "check-range-types" => self.out.push(Step::Gate(Gate::RangeTypes)),
            "check-doc-refs" => self.out.push(Step::Gate(Gate::DocRefs)),
            "check-pointers" => self.out.push(Step::Gate(Gate::Pointers)),
            "check-platform-seam" => self.out.push(Step::Gate(Gate::PlatformSeam)),
            "check-conformance" => self.out.push(Step::Gate(Gate::Conformance)),
            "conformance" => {
                let o = options(name, args, &["--results", "--out"])?;
                let results = o.get("--results").map(String::as_str).ok_or("`conformance` needs `--results <dir>`")?;
                let out = o.get("--out").map_or("site/conformance", String::as_str);
                self.out.push(Step::Conformance {
                    results: self.ctx.root.join(results),
                    out: self.ctx.root.join(out),
                });
            }
            "coverage" => {
                let o = options(name, args, &["--out", "--package"])?;
                let out = o.get("--out").map_or("site/coverage", String::as_str);
                self.out.push(Step::RequireLlvmCov);
                // The fixtures look for the plugin binaries beside the running
                // test binary, which llvm-cov builds under its own target
                // directory: build them there.
                for bin in ["plugin-server", "plugin-probe"] {
                    self.out.push(Step::InLlvmCovEnv(Cmd::sh(&format!(
                        "cargo build --locked -p tutti-{bin} --bin {bin}"
                    ))));
                }
                let mut run = Cmd::sh("cargo llvm-cov nextest --locked --ignore-run-fail");
                match o.get("--package") {
                    Some(list) => {
                        for p in list.split(',') {
                            run = run.arg("-p").arg(p);
                        }
                    }
                    None => run = run.arg("--workspace"),
                }
                self.run(run.arg("--html").arg("--output-dir").arg(out));
            }
            "conformance-inventory" => {
                let [f] = Self::params(name, args, ["clap"])?;
                self.out.push(Step::ConformanceInventory(f));
            }
            "check-submodule-pins" => self.out.push(Step::Gate(Gate::SubmodulePins)),
            "check-doc-features" => self.out.push(Step::DocFeatures),
            "check-hosting" => self.out.push(Step::Gate(Gate::Hosting)),
            "check-paths" => self.out.push(Step::Gate(Gate::CanonicalPaths(args.to_vec()))),
            "fmt" => self.lines(&["cargo fmt --all --check"]),
            "lint" => {
                self.dep("fmt")?;
                self.lines(&["cargo clippy --locked --workspace --all-targets -- -D warnings"]);
            }
            "check-windows" => self.check_windows(),
            "doc" => {
                self.lines(&[
                    r#"RUSTDOCFLAGS="-D warnings --document-private-items" cargo doc --locked --workspace --no-deps"#,
                    r#"RUSTDOCFLAGS="-D warnings --document-private-items" cargo doc --locked -p tutti-session -p tutti --no-deps --all-features"#,
                    r#"RUSTDOCFLAGS="-D warnings" cargo doc --locked -p tutti-graph --no-deps --features contract"#,
                    r#"RUSTDOCFLAGS="-D warnings" cargo doc --locked -p tutti-dsp --no-deps --features contract"#,
                    r#"RUSTDOCFLAGS="-D warnings" cargo doc --locked -p tutti-plugin-types --no-deps --features contract"#,
                ]);
                self.out.push(Step::DocFeatures);
                self.lines(&[
                    r#"RUSTDOCFLAGS="-D warnings --cfg docsrs" cargo +nightly-2026-10-14 doc --locked --workspace --no-deps"#,
                ]);
            }
            "check-loom" => self.lines(&[
                r#"RUSTFLAGS="--cfg loom" cargo clippy --locked -p tutti-rt -p tutti-shm-model --all-targets -- -D warnings"#,
            ]),
            "book" => {
                let [out] = Self::params(name, args, [""])?;
                if out.is_empty() {
                    self.lines(&["mdbook build book"]);
                } else {
                    self.lines(&[format!("mdbook build book --dest-dir {out}")]);
                }
            }
            "verify-audio" => {
                let [out] = Self::params(name, args, ["/tmp/tutti-verify"])?;
                self.lines(&[
                    format!("cargo run --locked --release -p tutti-verify --bin render_sampler_cases -- {out}/sampler"),
                    format!("cargo run --locked --release -p tutti-verify --bin render_export_cases -- {out}/export"),
                    format!("cargo run --locked --release -p tutti-verify --bin render_analysis_cases -- {out}/analysis"),
                    format!("cargo run --locked --release -p tutti-verify --bin render_synth_cases -- {out}/synth"),
                    format!("cargo run --locked --release -p tutti-verify --bin render_dsp_cases -- {out}/dsp"),
                    "uv sync --quiet".to_owned(),
                    format!("uv run python testing/tutti-verify/py/verify_sampler.py {out}/sampler"),
                    format!("uv run python testing/tutti-verify/py/verify_export.py {out}/export"),
                    format!("uv run python testing/tutti-verify/py/verify_analysis.py {out}/analysis"),
                    format!("uv run python testing/tutti-verify/py/verify_synth.py {out}/synth"),
                    format!("uv run python testing/tutti-verify/py/verify_dsp.py {out}/dsp"),
                ]);
            }
            "release" => {
                let [version] = Self::params(name, args, [""])?;
                let v = Version::parse(&version).ok_or_else(|| {
                    format!("`release` takes a version MAJOR.MINOR.PATCH, got `{version}`")
                })?;
                self.out.push(Step::Release(v));
            }
            "package" => self.out.push(Step::Package),
            "deny" => {
                self.out.push(Step::RequireCargoDeny);
                self.lines(&["cargo deny --locked check advisories"]);
            }
            "gates" => {
                self.dep("fmt")?;
                self.lines(&["cargo test --locked -p xtask"]);
                for d in [
                    "check-paths", "check-session", "check-guards", "check-ffi", "check-errors",
                    "check-layout", "check-range-types", "check-hosting", "check-lints",
                    "check-value-traits", "check-submodule-pins", "check-doc-refs",
                    "check-pointers", "check-platform-seam",
                ] {
                    self.dep(d)?;
                }
            }
            "ci" => {
                for d in [
                    "gates", "lint", "deny", "check-conformance",
                    "check-features", "package",
                    "check-loom", "test", "test-features", "test-doc",
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
            "--workspace --all-targets --exclude tutti-codec --exclude tutti-export --exclude tutti --exclude tutti-vst3-host --exclude tutti-plugin-server --exclude tutti-plugin-probe --exclude tutti-shared-plugin-vst3-cpp --exclude tutti-plugin-contract --exclude tutti-verify",
        );
        for a in [
            "-p tutti-shared-plugin --all-targets",
            "-p tutti-export --lib",
            "-p tutti-codec --features encode-wav,encode-aiff,encode-flac,decode-wav,decode-flac,decode-mp3,decode-ogg,serde --all-targets",
            "-p tutti-plugin -p tutti-plugin-types --features tutti-plugin/clap,tutti-plugin/vst3,tutti-plugin/vst2,tutti-plugin/serde --all-targets",
            "-p tutti-plugin-db --features serde --all-targets",
            "-p tutti-vst2-host --all-targets",
            "-p tutti-vst3-host -p tutti-plugin-server -p tutti-plugin-probe --lib --bins",
        ] {
            win(a);
        }
    }
}

const CHECK_FEATURES: &[&str] = &[
    r#"cargo clippy --locked -p tutti-cpal --features capture --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-session --features audio-io --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-session --features clap,export,modulation,midi-hardware,synth,soundfont,sampler,vst2,device,audio-io,serde --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti --features clap,export,modulation,sampler,dsp,midi-hardware,synth --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti --features audio-io --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti --no-default-features --features io --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-export --lib -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-session --lib --features export,encode-flac,encode-aiff,encode-vorbis -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-session --lib --features vst3,au -- -D warnings"#,
    r#"cargo clippy --locked -p tutti --features device,soundfont,decode-wav,decode-flac,decode-mp3,decode-ogg,export,encode-flac,encode-aiff,encode-vorbis --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-sampler --lib -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-sampler --lib --features decode-wav,decode-flac,decode-mp3,decode-ogg -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-plugin --features vst3,au,serde --all-targets -- -D warnings"#,
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
    r#"cargo clippy --locked -p tutti-plugin-types --lib --features contract -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-midi-hardware --features sequencer-tests --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-clap-ffi --lib -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-plugin --features clap,vst2,serde --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-plugin-db --features serde --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-plugin-contract --features server --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-plugin-server --no-default-features --features vst3 --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-plugin-server --no-default-features --features clap --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-plugin-probe --no-default-features --features vst2 --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-plugin-probe --no-default-features --features vst3 --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-plugin-probe --no-default-features --features clap --all-targets -- -D warnings"#,
];

const TEST_FEATURES: &[&str] = &[
    r#"cargo build --locked -p tutti-plugin-server --no-default-features --features clap,vst3"#,
    r#"cargo build --locked -p tutti-plugin-probe"#,
    r#"cargo nextest run --locked -p tutti-cpal --features capture"#,
    r#"cargo nextest run --locked -p tutti-session --features audio-io"#,
    r#"cargo nextest run --locked -p tutti-session --features clap,export,modulation,midi-hardware,synth,soundfont,sampler,vst2,device,audio-io,serde"#,
    r#"cargo nextest run --locked -p tutti --features clap,export,modulation,sampler,dsp,midi-hardware,synth"#,
    r#"cargo nextest run --locked -p tutti --features audio-io"#,
    r#"cargo nextest run --locked -p tutti --no-default-features --features io"#,
    r#"cargo nextest run --locked -p tutti-session --features export,encode-flac,encode-aiff,encode-vorbis"#,
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
    r#"cargo nextest run --locked -p tutti-plugin-types --features testing"#,
    r#"cargo nextest run --locked -p tutti-plugin-db --features serde"#,
    r#"cargo nextest run --locked -p tutti-plugin-contract --features server"#,
    r#"cargo nextest run --locked -p tutti-plugin --features clap,vst2,serde"#,
];

const MIRI: &[&str] = &[
    r#"cargo +nightly miri test --locked -p tutti-rt --lib"#,
    r#"MIRIFLAGS="-Zmiri-many-seeds=0..16" cargo +nightly miri test --locked -p tutti-rt --lib edits"#,
];

const LOOM: &[&str] = &[
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
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test dirty_set_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test edit_ring_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test drain_clock_loom -- --nocapture"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test fault_latch_loom -- --nocapture"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test wake_flag_loom"#,
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
    r#"cargo bench --locked -p tutti-plugin-host --bench vst2_in_process"#,
];

#[cfg(test)]
mod tests;
