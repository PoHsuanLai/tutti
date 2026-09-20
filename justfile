# Tutti audio engine — development commands.
#
# These were carried in the dawai app repo's justfile, where every one of them
# needed a `--manifest-path crates/bevy-tutti/Cargo.toml` to reach across the
# workspace boundary. Here the engine IS the workspace, so they are plain cargo
# invocations.

# Run the full test suite the way CI does.
#
# nextest, not `cargo test`: each test gets its own process, which is what the
# audio suites need — they touch device and global state, and a panic or an
# abort in one no longer takes the whole binary's results with it. It is also
# per-test parallel rather than per-binary, and prints a leak/slow/timeout line
# instead of hanging silently.
#
#   cargo install cargo-nextest --locked
# The two GUI-lifecycle binaries are excluded: they are `harness = false` custom
# mains driving a plugin editor on the real main thread. nextest enumerates a
# binary by running it with `--list --format terse`; these ignore the flag, run
# their whole suite and exit non-zero, so nextest fails at *list* time having
# already executed them. `just test-editor` runs them properly.
test *ARGS: plugin-server
    cargo nextest run --workspace {{ARGS}} \
        -E 'not binary(gui_lifecycle_main) and not binary(au_gui_lifecycle_main)'

# The custom-harness editor tests. `cargo test` just execs the binary, which is
# what a `harness = false` main wants.
test-editor:
    cargo test -p tutti-vst3-host --test gui_lifecycle_main
    cargo test -p tutti-au-host --test au_gui_lifecycle_main

# The doctests, which nextest does NOT run — and says nothing about skipping.
#
# That is not a rounding error: these are the only tests covering the `///`
# examples, so a green `just test` is not a green workspace. `cargo test --doc`
# is the only way to run them; nextest has no doctest support at all.
test-doc *ARGS:
    cargo test --doc --workspace {{ARGS}}

# Everything.
test-all: test test-editor test-doc

# Build the out-of-process plugin host.
#
# MUST exist before the test run. `tutti-plugin`'s `clap_pdc_alignment`,
# `clap_crash_recovery` and `real_stall_tests` drive the reference CLAP plugin
# through a real out-of-process host, so they spawn this binary.
#
# It cannot be a dev-dependency that cargo would build for them —
# tutti-plugin-server depends on tutti-plugin, so the edge would be a dependency
# *cycle* — and nothing else in a test run builds it. Skipping it makes those
# three suites hard-fail with this exact command rather than silently skip,
# which is deliberate: a suite that skips on a missing fixture is one typo away
# from reporting success having executed nothing.
#
# (`tutti-clap-host`'s own suites, `clap_param_gain` among them, load the same
# plugin *in-process* and need none of this.)
plugin-server:
    cargo build -p tutti-plugin-server --bin plugin-server

plugin-server-release:
    cargo build --release -p tutti-plugin-server --bin plugin-server

# The Bevy-free floor.
#
# Every engine crate that has a `bevy` feature has it OFF by default, so
# `--no-default-features` does NOT test this — it drops the codec and midi
# features instead and proves nothing. Naming the feature is what tests both
# sides.
check-bevy-free:
    cargo check -p tutti-core
    cargo check -p tutti-core --features bevy
    cargo check -p tutti-types
    cargo check -p tutti-types --features bevy

# Import-path gate. Engine types are imported from a crate's root or prelude,
# never through its modules. Neither rustc nor clippy has a lint for a redundant
# path, and doc comments in ```text / ```ignore blocks are never compiled — so
# this grep is the only enforcement there is.
check-paths *ARGS:
    scripts/check-canonical-paths.sh {{ARGS}}

lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --exclude fundsp-tutti --exclude rustysynth-tutti -- -D warnings

# The rustdoc gate. The workspace sets broken_intra_doc_links and
# private_intra_doc_links to deny, but a plain build never runs rustdoc, so this
# is where that is actually enforced.
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps \
        --exclude fundsp-tutti --exclude rustysynth-tutti

# The audio-correctness harnesses: a Rust example renders audio to a directory,
# and a Python judge grades it.
#
# Why Python at all, when the engine is Rust — these scripts are a *second
# opinion*. Four defects in the sampler were invisible to its whole Rust suite
# because the tests were written against the same understanding of the DSP as
# the code, so they could not contradict it. A judge that re-derives the
# expected answer from the signal, using libraries with no knowledge of tutti,
# has no such shared blind spot.
#
# Needs `uv`; deps are declared in pyproject.toml.
verify-audio OUT="/tmp/tutti-verify":
    cargo run --release -p tutti-sampler   --example render_cases           -- {{OUT}}/sampler
    cargo run --release -p tutti-export    --example render_export_cases    -- {{OUT}}/export
    cargo run --release -p tutti-analysis  --example render_analysis_cases  -- {{OUT}}/analysis
    cargo run --release -p tutti-polysynth --example render_synth_cases     -- {{OUT}}/synth
    uv sync --quiet
    uv run python crates/dsp/tutti-sampler/examples/verify_sampler.py   {{OUT}}/sampler
    uv run python crates/core/tutti-export/examples/verify_export.py    {{OUT}}/export
    uv run python crates/dsp/tutti-analysis/examples/verify_analysis.py {{OUT}}/analysis
    uv run python crates/dsp/tutti-polysynth/examples/verify_synth.py   {{OUT}}/synth

# Everything CI runs, in CI's order.
ci: lint check-paths check-bevy-free test test-editor test-doc doc
