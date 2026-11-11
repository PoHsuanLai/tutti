//! **A removed in-process VST2 node is dropped on the control side, never on the audio thread.**
//!
//! The node holds the plugin's `Processor` and so its `RenderHandle`. Dropping the last
//! `RenderHandle` (the instance having closed first) hands the core to the main thread with
//! `defer_to_main`, which locks and allocates: it must never run on the audio thread. What
//! keeps it off is the graph: the executor moves a retired unit into the commit box it sends
//! back (`tutti-graph`'s `exec.rs`, the `retired` pushes in `apply_pending`), the editor's
//! `collect` drops it on the control side, and a unit dropped inside the audio scope panics in
//! a debug build (`NodeBox::drop` -> `RtScope::check_outside`).
//!
//! The test removes the node while the engine renders, applies the removal and renders the next
//! blocks inside `assert_no_alloc`, reads the plugin's close count (the plugin's own view: 0,
//! nothing closed on the audio side), then collects on the control thread and reads it again
//! (1).

use std::path::PathBuf;
use tutti_plugin_types::Load;
use tutti_types::MaxBlock;
use tutti_vst2_host::Vst2Loaded;

use assert_no_alloc::AllocDisabler;
use tutti_graph::NodeKey;
use tutti_graph::{Editor, Prepare, Transport, Unforkable};
use tutti_graph::{OutPort, Source};
use tutti_plugin_host::load_client;
use tutti_types::{SampleRate, Samples};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

const RATE: f64 = 48_000.0;
const BLOCK: usize = 64;

/// The probe's count of `effClose`s, read from the image the host loaded (the linked rlib is a
/// separate image with separate statics); the image is kept mapped.
fn close_count(path: &PathBuf) -> u32 {
    // SAFETY: the path is the cdylib this crate's dev-dependency built.
    let lib = unsafe { libloading::Library::new(path) }.expect("re-open the probe");
    // SAFETY: the symbol is the probe's `#[no_mangle]` export.
    let sym: libloading::Symbol<*mut std::ffi::c_void> =
        unsafe { lib.get(b"tutti_vst2_probe_close_count\0") }.expect("probe export");
    // SAFETY: the export has this signature.
    let f: extern "C" fn() -> u32 = unsafe { std::mem::transmute(*sym) };
    let n = f();
    std::mem::forget(lib);
    n
}

fn block(exec: &mut tutti_graph::Executor, left: &mut [f32], right: &mut [f32]) {
    exec.process(BLOCK, &Transport::default(), &[], &mut [left, right]);
}

#[test]
fn a_removed_vst2_node_is_dropped_on_the_control_side() {
    let main = tutti_plugin_types::MainThread::claim().expect("one test, one process");
    let path = tutti_fixtures::vst2_probe().to_path_buf();
    let (unit, handle) =
        load_client::<Vst2Loaded>(&main, &path, &Load::new(), SampleRate::new(RATE))
            .expect("the probe loads in process");

    let key = NodeKey(1);
    let (mut editor, mut exec) =
        Editor::new(Prepare::new(SampleRate(RATE), MaxBlock::of(Samples(BLOCK))));
    editor.insert(key, "vst2", Unforkable(unit.bind(tempo())));
    editor
        .spec_mut()
        .topology
        .set_outputs((0..2).map(|port| Source::Node(OutPort { node: key, port })));
    editor.commit().expect("commits");
    let mut left = vec![0.0f32; BLOCK];
    let mut right = vec![0.0f32; BLOCK];
    for _ in 0..4 {
        block(&mut exec, &mut left, &mut right);
    }
    assert_ne!(left[0], 0.0, "the plugin renders before the removal");

    // The node is the last holder of the instance once the handle goes.
    drop(handle);
    let before = close_count(&path);

    editor.remove(key);
    editor
        .spec_mut()
        .topology
        .set_outputs(vec![Source::Zero, Source::Zero]);
    editor.commit().expect("the removal commits");
    // The executor applies the removal: the unit moves into the commit box (a drop here would
    // panic in a debug build, inside the audio scope). Applying may allocate, so it is outside
    // the gate, as in the other no-alloc suites; the blocks that follow are inside it.
    exec.apply_pending();
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..4 {
            block(&mut exec, &mut left, &mut right);
        }
    });
    assert_eq!(
        close_count(&path),
        before,
        "the audio side closed the plugin: the unit was dropped there"
    );

    // The control side collects: the unit drops here, on the thread that made the instance.
    let retired = editor.collect();
    assert_eq!(retired, vec![key], "the commit box brought the unit back");
    assert_eq!(
        close_count(&path),
        before + 1,
        "the control-side drop closed the plugin, once"
    );
}

/// The project's tempo map the node under test is bound to.
fn tempo() -> tutti_transport::TempoReader {
    tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo")
}
