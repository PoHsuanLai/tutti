//! End-to-end test of the in-process VST2 backend.
//!
//! Loads the reference probe (`tutti-vst2-test-plugin`), which is built from
//! this tree by a dev-dependency edge in the same `cargo test` invocation.
//!
//! Nothing here needs a commercial synth: the subject is the *backend*, and the
//! probe can be told to declare parameters, an editor, chunk-based state and a
//! channel count on demand. That is what lets these run by default on every
//! machine — pinned to a hardcoded plugin path they would be `#[ignore]`d, never
//! run, and free to drift out of sync with the handle API.

#![cfg(feature = "vst2")]

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use tutti_plugin::error::StateError;
use tutti_plugin::handles::PluginHandle;
use tutti_plugin::server::EditorPresence;
use tutti_plugin_types::{Normalized, ParamAddress};

#[path = "support/probe_path.rs"]
mod probe_path;

const SAMPLE_RATE: f64 = 48_000.0;

/// VST2 addresses parameters by position, so every id here is an `Index`.
const PARAM_0: ParamAddress = ParamAddress::Index(0);

/// The parameter surface takes [`Normalized`], not a bare float — the domain is
/// in the type, so a caller looking at a `[10, 22050]` Hz range on a
/// `ParameterInfo` cannot write `20_000.0` and silently land at full scale.
fn n(v: f64) -> Normalized {
    Normalized::new(v)
}

/// Serializes against the probe's process-global switches and environment, and
/// against racing `VSTPluginMain` calls on one library.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

fn lock_probe() -> MutexGuard<'static, ()> {
    PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Every `TUTTI_VST2_PROBE_*` key this file sets, cleared between loads so a
/// leaked variable cannot reshape an unrelated test's plugin.
const PROBE_ENV_KEYS: &[&str] = &[
    "TUTTI_VST2_PROBE_PARAMS",
    "TUTTI_VST2_PROBE_EDITOR",
    "TUTTI_VST2_PROBE_OUTPUTS",
    "TUTTI_VST2_PROBE_MIDI_INPUTS",
    "TUTTI_VST2_PROBE_MIDI_OUTPUTS",
    "TUTTI_VST2_PROBE_TAIL_SIZE",
];

fn clear_probe_env() {
    for key in PROBE_ENV_KEYS {
        // SAFETY: callers hold `PROBE_LOCK`, so no other test thread is
        // touching the environment concurrently.
        unsafe { std::env::remove_var(key) };
    }
}

/// Load the probe through the in-process VST2 path, keeping only the handle —
/// which is what a real host is left holding once the node is in a graph.
fn load_handle(env: &[(&str, &str)]) -> PluginHandle {
    let path: PathBuf = probe_path::probe_path().clone();
    clear_probe_env();
    for (k, v) in env {
        // SAFETY: as above.
        unsafe { std::env::set_var(k, v) };
    }
    let (_unit, handle) = tutti_plugin::in_process_vst2_client(&path, SAMPLE_RATE)
        .unwrap_or_else(|e| panic!("in-process VST2 load failed for {path:?}: {e:?}"));
    // The AEffect is built; clearing now keeps the variable from outliving it.
    clear_probe_env();
    handle
}

#[test]
fn load_in_process_returns_audio_unit_and_handle() {
    let _lock = lock_probe();
    let handle = load_handle(&[("TUTTI_VST2_PROBE_EDITOR", "1")]);

    let descriptor = handle.descriptor();
    assert!(!descriptor.name.is_empty());
    assert_eq!(handle.loaded().total_outputs(), 2);
    assert_eq!(descriptor.editor, EditorPresence::Present);
}

/// A parameter written through the handle reads back through the handle.
///
/// The round trip is the point: `set_parameter_value` is fire-and-forget, so
/// only a read proves the write reached the plugin rather than being dropped at
/// the `try_lock` inside the in-process backend.
#[test]
fn handle_parameter_roundtrip() {
    let _lock = lock_probe();
    let handle = load_handle(&[("TUTTI_VST2_PROBE_PARAMS", "4")]);

    let params = handle
        .params()
        .parameter_descriptors()
        .expect("the in-process VST2 backend enumerates parameters");
    assert_eq!(params.len(), 4, "the probe declared 4 parameters");

    handle.params().set_parameter_value(PARAM_0, n(0.5));
    let v = handle
        .params()
        .parameter_value(PARAM_0)
        .expect("param 0 should exist");
    assert!((v - 0.5).abs() < 0.01, "expected ~0.5, got {v}");
}

/// A state blob crosses the backend and comes back byte-for-byte.
///
/// # What this does and does not prove
///
/// It proves the chunk reaches the plugin and returns unaltered — no
/// truncation, no UTF-8 assumption, no re-encoding of an opaque blob — and that
/// `load_state` actually lands, since the blob loaded differs from the one
/// saved.
///
/// It does **not** prove a loaded state restores parameter values, and cannot.
/// The probe's `effSetChunk` writes an opaque byte store with no connection to
/// its parameters (`load_bank_data` in `tutti-vst2-test-plugin`), so moving a
/// parameter and restoring would assert a behaviour the fixture does not
/// implement — which is exactly what an earlier draft of this test did, and it
/// failed for that reason rather than finding a host bug.
///
/// Restoring *values* is covered against real plugins in `tutti-au-host`'s
/// `au_third_party.rs` and `tutti-vst2-host`'s `integration_tests.rs`, both of
/// which are gated on installed plugins. Stating the gap here rather than
/// leaving an assertion that looks like it covers it.
#[test]
fn handle_state_roundtrip() {
    let _lock = lock_probe();
    let handle = load_handle(&[("TUTTI_VST2_PROBE_PARAMS", "4")]);

    let original = handle
        .state()
        .save_state()
        .expect("the probe declares chunk-based state");
    assert!(!original.is_empty(), "a declared chunk must not be empty");

    // Load a blob that is *different* from the one saved. Round-tripping the
    // same bytes would pass even if `load_state` did nothing at all — that
    // mutation was run against an earlier draft of this test and survived,
    // because the probe's chunk store is only rewritten by a load that lands.
    let mut modified = original.clone();
    modified.extend_from_slice(b"tutti-roundtrip-marker");

    handle
        .state()
        .load_state(&modified)
        .expect("the probe accepts a chunk it just produced");
    let after = handle
        .state()
        .save_state()
        .expect("state must still be readable after a load");

    assert_eq!(
        after, modified,
        "the loaded blob must reach the plugin and come back unaltered"
    );
    assert_ne!(
        after, original,
        "a load that did nothing would leave the original bytes"
    );
}

/// A plugin that refuses a chunk says so, and says why.
///
/// The common case in the field: a preset saved by an older build of a plugin,
/// a truncated file, a chunk belonging to a different plugin. A `-> ()`
/// signature makes it unreportable — no expression a caller can write tells a
/// rejected load from an accepted one, and the DAW shows a restored plugin
/// sitting at its defaults.
///
/// The probe refuses an empty blob (`load_bank_data` in
/// `tutti-vst2-test-plugin` returns `false` for one, deliberately, so a host
/// can be seen ignoring a refusal). That makes the refusal reachable without a
/// second plugin or a version skew.
#[test]
fn a_refused_chunk_reports_the_refusal() {
    let _lock = lock_probe();
    let handle = load_handle(&[]);

    let err = handle
        .state()
        .load_state(&[])
        .expect_err("the probe refuses an empty chunk");

    // `Rejected` specifically: the plugin was asked and said no. `NoStateRoute`
    // would mean this backend cannot carry state at all, and `PluginCrashed`
    // that it is gone — three different things a caller acts on differently,
    // which is the whole reason this is not a bool.
    assert!(
        matches!(err, StateError::Rejected(_)),
        "an in-process refusal must be Rejected, got {err:?}"
    );
    assert!(
        !err.to_string().is_empty(),
        "the refusal must carry a message a user can be shown"
    );
}

/// A blob the plugin accepts reports success, so the test above is not merely
/// observing that everything fails.
#[test]
fn an_accepted_chunk_reports_success() {
    let _lock = lock_probe();
    let handle = load_handle(&[]);

    let blob = handle.state().save_state().expect("probe declares state");
    handle
        .state()
        .load_state(&blob)
        .expect("a chunk the plugin just produced must be accepted");
}

#[test]
fn handle_is_not_crashed_in_process() {
    let _lock = lock_probe();
    let handle = load_handle(&[]);
    assert!(!handle.is_crashed());
}

#[test]
fn open_routes_a_vst2_to_the_in_process_backend() {
    // `Plugin::open` detects the format from the path and routes a VST2 to
    // the in-process backend (with the `vst2` feature on, which the
    // integration-test gate requires). Confirm by checking has_editor reports
    // true and no crash.
    //
    // This replaces `vst2_builder_routes_to_in_process`, which asserted the
    // same routing through `tutti_plugin::vst2(..).build()`. That builder is
    // gone; it matched only `Some("vst") | Some("VST")` while
    // `format_from_path` — which `open` uses — also maps `.dll` and `.so` to
    // VST2, so the two disagreed on every Windows and Linux VST2. Routing had
    // to keep a test either way, so it moved here rather than being deleted
    // with the builder.
    let _lock = lock_probe();

    // `open` routes on the *extension*, and the probe is built as a plain
    // cdylib — `.dylib` on macOS, which is not a VST2 extension anywhere and is
    // deliberately absent from `FORMAT_BY_EXTENSION`. Real macOS VST2s are
    // `.vst` bundles. So copy the image to a `.vst` name: that is the shape a
    // host actually meets, and it is what makes this test exercise the routing
    // rather than the naming.
    let src = probe_path::probe_path().clone();
    let staged = std::env::temp_dir().join("tutti-probe-open-routing.vst");
    std::fs::copy(&src, &staged)
        .unwrap_or_else(|e| panic!("staging {src:?} -> {staged:?} failed: {e}"));

    // Hold the staged image open for the whole test: the `effCanDo` switch is
    // a `static` in it, and it only survives while the image stays mapped.
    // SAFETY: the staged file is the probe cdylib this crate's tests build.
    let image = unsafe { libloading::Library::new(&staged) }
        .unwrap_or_else(|e| panic!("opening the staged probe {staged:?} failed: {e}"));
    // SAFETY: `tutti_vst2_probe_set_can_do` is the probe's `#[no_mangle]`
    // export, `extern "C" fn(i32, isize)`; `1` is its `Maybe`.
    unsafe {
        let set_can_do: libloading::Symbol<extern "C" fn(i32, isize)> = image
            .get(b"tutti_vst2_probe_set_can_do\0")
            .expect("the probe exports its effCanDo switch");
        set_can_do(1, 0);
    }

    clear_probe_env();
    // SAFETY: `PROBE_LOCK` is held, so no other test thread reads the
    // environment concurrently.
    unsafe { std::env::set_var("TUTTI_VST2_PROBE_EDITOR", "1") };
    let plugin = tutti_plugin::catalog::Plugin::open(&staged, SAMPLE_RATE)
        .expect("Plugin::open should load a .vst in-process");
    clear_probe_env();
    let handle = plugin.handle().clone();
    drop(plugin);

    assert!(!handle.is_crashed());
    assert!(
        handle.has_editor(),
        "in-process backend should report has_editor"
    );

    drop(handle);
    let _ = std::fs::remove_file(&staged);
}

/// VST2 reports no speaker placement, and the handle says so plainly.
///
/// The end-to-end half of `LayoutSupport`: a real plugin, loaded through the
/// real path, reporting `None` because the format has no way to be asked —
/// `effSetSpeakerArrangement` needs a `VstSpeakerArrangement` struct the
/// vendored bindings do not define, and the host never sends it.
///
/// Worth an integration test rather than only a unit one: the unit tests build
/// a `LoadedPlugin` by hand, so they would still pass if the VST2 loader
/// silently populated the field with something. This asserts the loader's own
/// answer.
#[test]
fn a_vst2_plugin_reports_no_channel_topology() {
    use tutti_plugin_types::LayoutSupport;

    let _lock = lock_probe();
    let handle = load_handle(&[]);

    assert_eq!(
        handle.layout_support(),
        LayoutSupport::None,
        "VST2 cannot be asked for speaker placement, so nothing may claim it can"
    );
    assert_eq!(handle.input_bus_topology(0), None);
    assert_eq!(handle.output_bus_topology(0), None);

    // The widths are still reported, unchanged by any of this — the placement
    // half being absent must not disturb the count half.
    assert_eq!(handle.loaded().total_outputs(), 2);
}

/// **The in-process node carries MIDI on its event ports, as it declared.**
/// A probe declaring MIDI in and no MIDI out: the façade says it takes MIDI
/// and sends none, and its node has one event input and no event output.
/// Declaring MIDI out gives the node an event output.
///
/// The probe answers `effCanDo` with `Maybe` here, so each direction resolves
/// from its declared pins: its default `Yes` outranks the pins (see
/// `tutti-vst2-host`'s resolution order) and would make every probe a sender.
///
/// Mutation (run): `sends_midi` asking `MIDI_IN` → the first probe reads as
/// sending → fails. Mutation (run): the node's event output count not gated
/// on `MIDI_OUT` → the first probe's node has one → fails.
#[test]
fn the_node_carries_midi_on_its_event_ports() {
    let _lock = lock_probe();

    // `Plugin::open` routes on the extension and the probe is built as a plain
    // `.dylib`, so stage it under a `.vst` name — same reason as
    // `open_routes_a_vst2_to_the_in_process_backend`.
    let src = probe_path::probe_path().clone();
    let staged = std::env::temp_dir().join("tutti-probe-midi-ports.vst");
    std::fs::copy(&src, &staged)
        .unwrap_or_else(|e| panic!("staging {src:?} -> {staged:?} failed: {e}"));

    // Hold the staged image open for the whole test: the `effCanDo` switch is
    // a `static` in it, and it only survives while the image stays mapped.
    // SAFETY: the staged file is the probe cdylib this crate's tests build.
    let image = unsafe { libloading::Library::new(&staged) }
        .unwrap_or_else(|e| panic!("opening the staged probe {staged:?} failed: {e}"));
    // SAFETY: `tutti_vst2_probe_set_can_do` is the probe's `#[no_mangle]`
    // export, `extern "C" fn(i32, isize)`; `1` is its `Maybe`.
    unsafe {
        let set_can_do: libloading::Symbol<extern "C" fn(i32, isize)> = image
            .get(b"tutti_vst2_probe_set_can_do\0")
            .expect("the probe exports its effCanDo switch");
        set_can_do(1, 0);
    }

    let open = |outputs: &str| {
        clear_probe_env();
        // SAFETY: `PROBE_LOCK` is held, so no other test thread reads the
        // environment concurrently.
        unsafe {
            std::env::set_var("TUTTI_VST2_PROBE_MIDI_INPUTS", "1");
            std::env::set_var("TUTTI_VST2_PROBE_MIDI_OUTPUTS", outputs);
        }
        let plugin = tutti_plugin::catalog::Plugin::open(&staged, SAMPLE_RATE)
            .unwrap_or_else(|e| panic!("in-process VST2 load failed for {staged:?}: {e:?}"));
        clear_probe_env();
        plugin
    };

    let takes_only = open("0");
    assert!(takes_only.takes_midi());
    assert!(!takes_only.sends_midi());
    let shape = tutti_graph::IntoNode::into_parts(takes_only).node.shape();
    assert_eq!((shape.event_in, shape.event_out), (1, 0));

    let both = open("1");
    assert!(both.sends_midi());
    let shape = tutti_graph::IntoNode::into_parts(both).node.shape();
    assert_eq!((shape.event_in, shape.event_out), (1, 1));

    drop(image);
    let _ = std::fs::remove_file(&staged);
}

/// The in-process node reports the tail the host decoded, not a blanket
/// `Unknown`.
///
/// The `AudioUnit` impls this type had (f32 and f64) used to hard-code
/// `Unknown` with a comment claiming VST2 has no tail query. It has one —
/// `effGetTailSize` — and `tutti-vst2-host` now asks it at load. This is the
/// graph-side consumer of that answer (the node's declared tail), so it gets its own coverage: the
/// out-of-process loader's tests cannot see this code path at all.
///
/// The raw `1` case is the one worth pinning. VST2 inverts the convention every
/// other format uses, so a plugin declaring "no tail at all" sends `1`, and a
/// node reporting that as `Unknown` makes PDC treat a genuinely dry plugin as
/// an unmeasured one.
#[test]
fn the_node_reports_the_decoded_tail_rather_than_unknown() {
    use tutti_plugin_types::{PluginTail, Samples};

    for (raw, want) in [
        ("0", PluginTail::Unknown),
        ("1", PluginTail::None),
        ("48000", PluginTail::Finite(Samples(48_000))),
    ] {
        let _lock = lock_probe();
        let path: PathBuf = probe_path::probe_path().clone();
        clear_probe_env();
        // SAFETY: the probe lock is held, so no other test thread is touching
        // the environment.
        unsafe { std::env::set_var("TUTTI_VST2_PROBE_TAIL_SIZE", raw) };
        let (unit, _handle) = tutti_plugin::in_process_vst2_client(&path, SAMPLE_RATE)
            .unwrap_or_else(|e| panic!("in-process VST2 load failed: {e:?}"));
        clear_probe_env();

        // The tail the graph's compiler folds: the node's declared shape.
        assert_eq!(
            tutti_graph::Node::shape(&unit).tail,
            want,
            "raw tail {raw} must decode to {want:?}"
        );
    }
}
