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

# The AU mapping-family claim, which needs third-party Audio Units installed.
# It hard-fails rather than skipping when none is present — deliberately — so it
# is excluded from CI and run on purpose here, on a machine that has them.
test-au-installed:
    cargo test -p tutti-au-host --test au_midi_map \
        no_installed_third_party_unit_implements_the_mapping_family

# The custom-harness editor tests. `cargo test` just execs the binary, which is
# what a `harness = false` main wants.
test-editor:
    cargo test -p tutti-vst3-host --test gui_lifecycle_main
    cargo test -p tutti-au-host --test au_gui_lifecycle_main

# The editor tests minus the one that needs a plugin corpus with both an
# editor-bearing and an editorless plugin — what CI runs. Use this if your
# machine has no third-party VST3s installed.
test-editor-no-corpus:
    TUTTI_GUI_SKIP=has_editor_agrees_with_opening_one \
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
test-all: test test-features test-editor test-doc

# Lint the feature-gated code that no other recipe compiles.
#
# `cargo tree --workspace -e features -i tutti-cpal` reports only "default":
# nothing in this workspace turns `capture`, `midi` or `audio-io` on, so
# `just test` and `just lint` typecheck none of them. That is the same hole
# `check-windows` exists to close, and for the same reason — a cfg block
# nothing compiles is a cfg block nothing lints, and it rots silently.
#
# What was dark until this recipe existed:
#   tutti-cpal/capture  — all of src/mic.rs (MicIn, the capture ring)
#   tutti-cpal/midi     — the pre_block/post_block arms of process_audio,
#                         the ordering the module header calls "the design"
#   bevy-tutti/audio-io — tests/audio_io_pump.rs, 12 tests that had never run
#   bevy-tutti/soundfont — tests/midi_soundfont.rs (implies `midi`); its 5 tests
#                          looked for the .sf2 at the pre-extraction
#                          `crates/tutti/assets/` path and failed on every run,
#                          which no run ever made
#   bevy-tutti/sampler  — src/sampler (the WaveAsset loader, the voice
#                         components) and its tests
#   tutti-io/bevy       — WaveAsset's Asset derive
#   bevy-tutti/plugin   — plugin_host binding against a live plugin
#                         (tests/plugin_capture.rs); `full,plugin` so the
#                         modulation-gated param binding compiles with it
#
# Not `--all-features`: that would pull every plugin-format SDK and (once it
# exists) JACK, which needs libjack on the box. Name the combinations.
check-features:
    cargo clippy -p tutti-cpal --features capture --all-targets -- -D warnings
    cargo clippy -p tutti-cpal --features midi --all-targets -- -D warnings
    cargo clippy -p tutti-cpal --features capture,midi --all-targets -- -D warnings
    cargo clippy -p bevy-tutti --features audio-io --all-targets -- -D warnings
    cargo clippy -p bevy-tutti --features soundfont --all-targets -- -D warnings
    cargo clippy -p bevy-tutti --features sampler --all-targets -- -D warnings
    cargo clippy -p bevy-tutti --features full,plugin --all-targets -- -D warnings
    cargo clippy -p tutti-io --features bevy --all-targets -- -D warnings

# Run the suites `just test` leaves dark. See check-features for which.
# The plugin-server build is for `bevy-tutti/plugin`'s plugin_capture.rs, which
# spawns it; CLAP only, as in CI, so no VST3 SDK is needed.
test-features:
    cargo build -p tutti-plugin-server --no-default-features --features clap
    cargo nextest run -p tutti-cpal --features capture,midi
    cargo nextest run -p bevy-tutti --features audio-io
    cargo nextest run -p bevy-tutti --features soundfont
    cargo nextest run -p bevy-tutti --features sampler
    cargo nextest run -p bevy-tutti --features full,plugin
    cargo nextest run -p tutti-io --features bevy

# JACK, separately: it hard-links libjack at BUILD time, so it needs
# `libjack-jackd2-dev` (Debian) / `jack-audio-connection-kit-devel` (Fedora)
# on the box. Kept out of `check-features` so a dev without those headers can
# still run the rest.
#
# Note what cpal's `jack` feature actually is: NOT a declared feature of cpal
# at all, but the implicit feature of an optional dependency that appears only
# in cpal's Linux/BSD target table. Enabling it on macOS or Windows compiles
# clean and reaches nothing, which is exactly the silent-no-op shape
# `check-windows` exists to catch — hence this recipe.
# Miri over the non-FFI unsafe: the pointer arithmetic in `tutti-node`'s planar
# buffers, the aliasing in `tutti-types`' `AudioThreadCell`, and `RtPublish`'s
# hazard-slot reclamation. See
# `docs/design/012-unsafe-policy.md` for why these two and not the rest — miri
# does not execute FFI at all, so the ~95% of this repo's unsafe that is a C ABI
# is out of its reach by construction, and out-of-process hosting is the
# structural answer there instead.
#
# `denormals.rs`'s two tests are `#[cfg_attr(miri, ignore)]`d: they read MXCSR,
# and miri has no x86 SSE intrinsics.
miri:
    cargo +nightly miri test -p tutti-types --lib
    MIRIFLAGS="-Zmiri-many-seeds=0..16" cargo +nightly miri test -p tutti-types --lib rt::publish
    cargo +nightly miri test -p tutti-node

# The loom models: `RtPublish`'s reclamation protocol and `PosRing`'s no-tear
# protocol (against the shipped code) and the plugin shm header protocol (a
# replica). `--cfg loom` is global, so
# each runs on its own target. This is what CI runs: the `RtPublish` models at a
# preemption bound of 4, about a minute and a half, and the `PosRing` models at
# the same bound, about five minutes.
loom:
    LOOM_MAX_PREEMPTIONS=4 RUSTFLAGS="--cfg loom" cargo test -p tutti-types --release --test rt_publish_loom
    LOOM_MAX_PREEMPTIONS=4 RUSTFLAGS="--cfg loom" cargo test -p tutti-types --release --test pos_ring_loom
    RUSTFLAGS="--cfg loom" cargo test -p tutti-shm-model --release

# The `RtPublish` loom models exhaustively (bar `xthread_overflow`, which is
# always bounded): about 17 minutes. Run it after changing `rt/publish.rs`.
loom-full:
    RUSTFLAGS="--cfg loom" cargo test -p tutti-types --release --test rt_publish_loom

check-jack:
    cargo clippy -p tutti-cpal --features jack --all-targets -- -D warnings

# The out-of-process plugin bridge at real callback pacing: mean/p50/p99/worst,
# over-deadline and non-silent counts, scaled over 1/2/4/8 instances.
#
# Uses installed third-party plugins when present and the reference CLAP probe
# otherwise, so it runs on a bare checkout; the run prints which. `plugin-server`
# is built first because the suite spawns it and nothing else in a test run does.
pressure:
    cargo build -p tutti-plugin-server
    cargo nextest run -p tutti-plugin --features clap --test real_plugin_pressure --no-capture

# Benchmarks.
#
# Targets are named one per line rather than swept with `--workspace`. A bare
# `cargo bench` — and `--benches` too — also runs every lib and integration
# test target in the RELEASE profile, where `debug_assert!` is compiled out,
# so `tutti-types`' `pump_rejects_a_width_mismatch` (a `#[should_panic]` over
# exactly such an assert) fails. Those tests belong to `just test`, which runs
# them in debug where their asserts exist.
#
# Adding a bench means adding a line here, the same contract `check-features`
# has. There is no sweep that would pick one up silently.
bench *ARGS:
    cargo bench -p tutti-nodes     --bench engine_render   {{ARGS}}
    cargo bench -p tutti-graph     --bench graph_render    {{ARGS}}
    cargo bench -p tutti-cpal      --bench audio_callback  {{ARGS}}
    cargo bench -p tutti-export    --bench offline_render  {{ARGS}}
    cargo bench -p tutti-polysynth --bench polysynth       {{ARGS}}
    cargo bench -p tutti-sampler   --bench voice_pool      {{ARGS}}
    # `--features vst2`: the only plugin path criterion can measure honestly.
    # Everything else is a subprocess — see the bench's own header.
    cargo bench -p tutti-plugin --features vst2 --bench vst2_in_process {{ARGS}}

# Record a named baseline on THIS machine, then compare against it later.
#
# Baselines never travel. A number from one box says nothing about another,
# and docs/benchmarks.md records which machine its figures came from for
# exactly that reason.
bench-save NAME="main":
    just bench -- --save-baseline {{NAME}}

bench-cmp BASE="main":
    just bench -- --baseline {{BASE}}

# Run every bench exactly once, no measurement.
#
# This is the gate: it proves the harnesses still RUN, which
# `cargo clippy --all-targets` (compile only) does not. A bench that panics on
# a changed API is invisible to every other recipe here.
bench-smoke:
    just bench -- --test

# The profiling harnesses. These deliberately do NOT use criterion — read
# their module docs for why (81x wall-clock spread; the question is the tail,
# not the mean).
profile-stretch:
    cargo build -p tutti-sampler --profile profiling --example profile_stretch_clone
    samply record target/profiling/examples/profile_stretch_clone

# Samply a criterion bench. `--profile-time` turns criterion's own analysis
# off, so the profile is of the code rather than of the statistics.
profile-bench BENCH="engine_render" PKG="tutti-nodes" SECS="10":
    cargo bench -p {{PKG}} --bench {{BENCH}} --profile profiling --no-run
    samply record target/profiling/deps/{{BENCH}}-* --bench --profile-time {{SECS}}

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
    # The facade, and the assertion the two `cargo check`s above cannot make.
    # A compile only proves the crate builds; it says nothing about what came
    # in with it. This is a NEGATIVE dependency check — no bevy crate may
    # appear anywhere under `tutti`, at any depth, with everything on.
    #
    # `-e normal`: `-e all` would walk dev-dependency edges, and `bevy-tutti`
    # is a workspace member, so it would false-positive.
    #
    # `--features full` and not `--all-features`: the latter pulls the VST3
    # and AU SDKs, which have nothing to do with this question.
    cargo check -p tutti
    cargo check -p tutti --features full
    @! cargo tree -p tutti --features full -e normal | grep -qE '\bbevy(_[a-z]+)?\b' \
        || (cargo tree -p tutti --features full -e normal | grep -nE '\bbevy(_[a-z]+)?\b'; \
            echo "FAIL: a bevy crate reached the Bevy-free facade."; exit 1)

# Import-path gate. Engine types are imported from a crate's root or prelude,
# never through its modules. Neither rustc nor clippy has a lint for a redundant
# path, and doc comments in ```text / ```ignore blocks are never compiled — so
# this grep is the only enforcement there is.
check-paths *ARGS:
    scripts/check-canonical-paths.sh {{ARGS}}

lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --exclude fundsp-tutti --exclude rustysynth-tutti -- -D warnings

# Typecheck and lint the Windows cfg paths, from Linux or macOS.
#
# Windows-only code is invisible to every other recipe here: a `#[cfg(windows)]`
# block is not compiled, so it is not typechecked and not linted. That is how a
# change shipped that took Windows from 37 failures to 47. This needs no Windows
# machine — only `rustup target add x86_64-pc-windows-msvc`, once.
#
# Clippy, not `test`: the tests cannot RUN here. This catches the
# compile-shaped half, which is the half that was being missed.
#
# Needs `clang` and `llvm-ar` too. criterion (a dev-dependency, so every
# `--all-targets`) builds a C shim through `cc`, which picks gcc, and gcc
# cannot target MSVC; clang can, and wants only a one-line `malloc.h` stub for
# `_alloca`. Clippy never links, so the stub has only to let the C compile.
#
# This is the part of CI's `clippy (windows)` job that Linux can do: the
# workspace minus five members, then the plugin crates with every format
# feature. The excluded members cannot cross-compile, because they need a real
# Windows C toolchain and headers, not a stub. `ogg_next_sys` (the vorbis codec,
# via tutti-export, which `tutti` and `bevy-tutti` depend on) includes
# `<string.h>`. `tutti-vst3-host`'s `conformance` feature (on in its own tests
# and `tutti-plugin-server`'s) compiles and links the `audio-probe` DLL against
# the SDK's Win32 sources. That is why the CI job runs on `windows-latest`.
# Their lib targets are still checked below where they can be.
# `au` is macOS-only and compiles to nothing here.
check-windows:
    #!/usr/bin/env bash
    set -euo pipefail
    stub="${TMPDIR:-/tmp}/tutti-winstub"
    mkdir -p "$stub"
    printf '#include <stddef.h>\nvoid *_alloca(size_t);\n' > "$stub/malloc.h"
    export CC_x86_64_pc_windows_msvc=clang AR_x86_64_pc_windows_msvc=llvm-ar
    export CFLAGS_x86_64_pc_windows_msvc="-I$stub"
    win() { cargo clippy --target x86_64-pc-windows-msvc "$@" -- -D warnings; }
    win --workspace --all-targets --exclude fundsp-tutti --exclude rustysynth-tutti \
        --exclude tutti-export --exclude tutti --exclude bevy-tutti \
        --exclude tutti-vst3-host --exclude tutti-plugin-server
    win -p tutti-plugin -p tutti-plugin-types --features tutti-plugin/clap,tutti-plugin/vst3,tutti-plugin/vst2,tutti-plugin/json --all-targets
    win -p tutti-clap-host --features clap-extras --all-targets
    win -p tutti-vst2-host --all-targets
    win -p tutti-vst3-host -p tutti-plugin-server --lib --bins

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
ci: lint check-paths check-bevy-free check-features test test-features test-editor test-doc doc
