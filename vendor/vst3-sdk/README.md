# vst3-sdk

Steinberg's VST3 SDK, as three git submodules pinned at **`v3.8.0_build_66`**.

| path | upstream |
|---|---|
| `pluginterfaces` | https://github.com/steinbergmedia/vst3_pluginterfaces |
| `base` | https://github.com/steinbergmedia/vst3_base |
| `public.sdk` | https://github.com/steinbergmedia/vst3_public_sdk |

MIT licensed (Steinberg Media Technologies GmbH); each repo carries its own
`LICENSE.txt`.

## What it is for

`tutti-vst3-host`'s `build.rs` compiles `tests/support/audio-probe` — *our*
reference VST3 plugin — against these sources, plus the SDK's own `hostchecker`
and `adelay` samples. Without an SDK the probe cannot be built, and without the
probe the VST3 conformance suites have nothing to load.

That used to mean an external `VST3_SDK_DIR` checkout, which made
`--features conformance` unusable on a fresh clone: `vst3_audio_correctness` and
`vst3_misbehaving_plugin` read `env!("VST3_PROBE_DIR")` and **panicked** when it
was empty. As submodules the SDK is simply present, and those suites run
anywhere.

## Why not the `vst3sdk` superproject

Steinberg publishes a superproject that aggregates seven submodules. Using it
would drag in `doc/` (154M), `vstgui4/` (14M), `cmake/` and `tutorials/` — none
of which any code path here touches — and its `.gitmodules` uses **relative**
URLs (`../vst3_base`), which resolve against whatever host the superproject was
cloned from. Naming the three source repos directly is smaller and states the
dependency explicitly.

Nothing here is patched. `build.rs` only reads: the `SDK_SOURCES` list names
compile inputs, `.include(sdk)` is a header search path, and the one generated
file (`projectversion.h`, normally produced by the SDK's CMake) is written to
`OUT_DIR`. Keep it that way — a local edit would be invisible to the pin below
and lost on the next re-sync.

## Checking it out

Submodules are not fetched by a plain `git clone`:

```bash
git submodule update --init --recursive
```

`build.rs` asserts with that exact command if the directories are empty, so a
missed init is a named build failure rather than a confusing missing-header
error. CI passes `submodules: recursive` to `actions/checkout`.

## Using a different SDK

Set `VST3_SDK_DIR` to an external checkout; it takes precedence and is validated
to look like an SDK (a `pluginterfaces/` directory inside). Useful for testing
the host against another SDK revision without touching the pins.

## Re-syncing

```bash
cd <submodule> && git fetch --tags && git checkout <new-tag>
cd - && git add <submodule> && git commit
```

Move all three together — they are released in lockstep, and the build compiles
sources from all three into one binary, so a mixed set is a compile error at
best. After bumping, re-run:

```bash
cargo test \
  -p tutti-vst3-host --features conformance
```
