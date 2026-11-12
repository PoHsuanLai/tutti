//! `Params::typed` over a real format's own value map (design 029 step 3): the
//! reference VST2 plugin, in process. Its parameter 1 is labelled `Hz` and
//! declares the integer range `0..=127`, which VST2 maps linearly
//! (`tutti-vst2-host`'s `IntegerRange`), so `Hz(31.75)` is position `0.25`
//! (the figures are the range's, written here, not read from the host).

use tutti_graph::NodeKey;
use tutti_plugin::catalog::PluginRef;
use tutti_plugin_types::{Normalized, ParamId, ParamUnit};
use tutti_session::{PluginLoadRequest, PluginParamError, Root, Session};
use tutti_types::{ChannelLayout, Db, Hz, SampleRate};

/// The probe's parameter that is labelled `Hz` and has an integer range.
const HERTZ: ParamId = ParamId::new(1);

fn main_thread() -> tutti_plugin_types::MainThread {
    tutti_plugin_types::MainThread::claim().expect("one test, one process")
}

/// The probe's image, opened here and held, with its parameter-properties
/// switch on: the host's own load of the same file shares the image, so its
/// first read of the parameters sees the integer range (a default VST2 plugin,
/// like every real one measured, declines the opcode).
fn probe_answering_properties() -> (libloading::Library, std::path::PathBuf) {
    let path = tutti_fixtures::cdylib("tutti_vst2_test_plugin", "tutti-vst2-test-plugin");
    // SAFETY: the probe is our own test plugin, whose load runs no constructor.
    let lib = unsafe { libloading::Library::new(&path) }.expect("the probe image opens");
    // SAFETY: the probe exports this as `extern "C" fn(bool)`.
    unsafe {
        let switch: libloading::Symbol<extern "C" fn(bool)> = lib
            .get(b"tutti_vst2_probe_set_answer_param_properties\0")
            .expect("the probe exports its switch");
        switch(true);
    }
    (lib, path)
}

fn session_with_probe() -> (Session, NodeKey, libloading::Library) {
    let (image, probe) = probe_answering_properties();
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    let node = s
        .add_plugin_blocking(
            &main_thread(),
            PluginLoadRequest::new(PluginRef::file(probe).expect("a plugin file")),
        )
        .expect("the in-process VST2 loads");
    s.graph().connect(node, Root).unwrap();
    let _ = s.update();
    (s, node.erase(), image)
}

/// **`typed` round-trips plain values through VST2's own map**, and refuses the
/// wrong unit on the same parameter: `Db` on a parameter that measures hertz.
#[test]
fn a_typed_hertz_parameter_round_trips_through_the_formats_own_range() {
    let (s, node, _image) = session_with_probe();
    let params = s.plugin(node).unwrap().params();
    assert_eq!(
        params.get(HERTZ).unwrap().info().measures(),
        Some(ParamUnit::Hertz),
        "the label and the declared range type it"
    );
    assert!(matches!(
        params.typed::<Db>(HERTZ),
        Err(PluginParamError::UnitMismatch {
            declared: ParamUnit::Hertz,
            ..
        })
    ));

    let hertz = params.typed::<Hz>(HERTZ).unwrap();
    for (plain, position) in [(0.0, 0.0), (31.75, 0.25), (63.5, 0.5), (127.0, 1.0)] {
        hertz.set(Hz::new(plain)).unwrap();
        assert_eq!(
            hertz.param().normalized(),
            Normalized::of(position),
            "{plain} Hz is position {position}"
        );
        assert_eq!(hertz.get(), Hz::new(plain), "and reads back as {plain} Hz");
    }
    assert_eq!(
        hertz.set(Hz::new(128.0)),
        Err(PluginParamError::OutOfRange(HERTZ)),
        "past the declared range"
    );
}
