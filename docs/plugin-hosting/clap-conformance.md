# CLAP host-conformance harness

An in-process **pluginval for our host**: a known-good reference plugin that
observes what `tutti-clap-host` hands it across the real CLAP FFI, so the host
can be asserted against the spec. The host-side analog of pluginval (which is a
host that tortures plugins) — here a plugin watches the host.

## Why this exists

The hermetic fakes in `tutti-plugin-server` (`NanPlugin`, `EchoProbe`) plug into
the unified `PluginInstance` trait and so test the *pipeline / bus-split* logic —
but they bypass the CLAP FFI entirely. Nothing else proved that our host builds
`clap_process` / `clap_audio_buffer` / the input-event list correctly. JUCE has no
reusable host-conformance suite (its tests validate plugins and are GPL C++ bound
to `AudioProcessor`); pluginval likewise tests plugins, not hosts. CLAP is the
right first target: MIT/Apache, and `clap-sys` 0.5 is host+plugin symmetric, so a
reference plugin is ~300 lines of pure Rust with no new deps.

## What it proves (this pass — core FFI correctness)

Driven through one real `process()` block, the reference plugin records and the
test (`tutti-clap-host/tests/clap_conformance.rs`) asserts:

- **Buffer geometry** — `frames_count`, in/out bus counts, per-port
  `channel_count`, and that the `data32`/`data64` split matches the negotiated
  sample format (f32 → `data32` live, `data64` null).
- **Event ordering** — input events arrive in non-decreasing `header.time`
  (sample offset). Feeds note-ons out of order (offsets 200/50/100) and asserts
  the plugin sees 50/100/200, proving the host's `sort_by_time`
  (`instance/audio.rs`) reaches the FFI boundary.
- **Param points** — a `PARAM_VALUE` event carries the right `param_id`, `value`,
  and sample-offset `time`, sorted.
- **Transport** — a supplied `TransportInfo` becomes a non-null
  `clap_event_transport` with the tempo intact.
- **Host callbacks** — the plugin's `request_callback()` is recorded by the host
  (`poll_callback_requested()` is true).

## How it's wired

- `crates/tutti-clap-test-plugin` — a `crate-type = ["cdylib"]` reference plugin.
  Exports `clap_entry`; records into a process-global capture; exposes it via the
  exported C symbol `tutti_test_plugin_capture`.
- `crates/tutti-clap-host/build.rs` — builds the reference plugin
  (`cargo build -p tutti-clap-test-plugin`, honoring `CARGO_TARGET_DIR`/profile)
  and emits its artifact path as `TUTTI_CLAP_TEST_PLUGIN`. If the build fails it
  emits empty and the test **skips with a message** — it never breaks the host
  build. Override with `TUTTI_SKIP_CLAP_TEST_PLUGIN=1`.
- The test loads the bare dylib via `ClapInstance::load_with_library(path,
  Some(path), …)` (no `.clap` bundle needed), drives `process`, then `dlopen`s the
  same path again to read the capture. dyld dedupes images by path, so both loads
  share one image and one capture.

The capture struct is `#[repr(C)]` and **mirrored by hand** in both crates — keep
the two definitions in sync if you extend it.

## Running

```bash
# From the tutti sub-workspace (crates/tutti). Runs in the DEFAULT suite;
# build.rs builds the reference plugin transitively.
cargo test -p tutti-clap-host --test clap_conformance -- --nocapture
```

## Follow-ups (not in this pass)

- **Multi-bus / sidechain through real CLAP FFI** — extend the reference plugin to
  declare a second input bus and assert the host presents it with the right layout
  (exercises the VST3-multibus equivalent at the CLAP boundary).
- **State round-trip** — assert `get_state`/`set_state` byte round-trip.
- **Latency / restart** — plugin reports a latency change + `request_restart`;
  assert the host's restart consumer reacts.
- **VST3 / AU** — their own reference plugins, or lean on the SDK validators
  (`auval` for AU, the VST3 SDK `validator`) which are the canonical conformance
  authorities for those formats.
