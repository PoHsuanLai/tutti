//! A served plugin's two control surfaces, against the real CLAP probe in a
//! real `plugin-server`.
//!
//! [`IpcController`] answers a call twice over: through [`Control`] (with the
//! main-thread token the trait takes) and through [`AnyThread`] (with none).
//! The two exist because a handle's off-thread calls have no token to give
//! (design 021 §10.4, "PR 4 as built"), and each call has **one** body, in
//! `AnyThread`, which `Control` forwards to. This suite is what holds the
//! forwards to it: a `Control` call that answered a default instead of
//! forwarding would pass every test that reads only one surface.

use tutti_fixtures::{machine_lock, main_thread};
use tutti_plugin_ipc::{BridgeConfig, Ipc, Launched};
use tutti_plugin_types::{AnyThread, Control, Health, ParamId, RenderMode, SampleRate};

use super::real_stall_tests::plugin_server_path;

/// The probe's applied gain (`GAIN_PARAM_ID` in the plugin).
const GAIN: ParamId = ParamId::new(77);

/// **Every call both surfaces carry answers alike**, each against the same
/// real plugin: the parameter catalog, a value, its text, the value parsed from
/// that text, the state, and the
/// render mode. (A parameter write is queued for the audio path and lands
/// only when blocks run, so it is not read back here; its forward is one line
/// onto the same bridge call.) The text and the parse are `Some` for this plugin, so a forward
/// that answered the default (`None`) differs from `AnyThread`'s answer.
///
/// `Control::set_automation_state` returns nothing, so the advisory is pinned
/// on the `AnyThread` side alone (it is delivered).
#[test]
fn the_served_control_and_any_thread_answer_alike() {
    let _lock = machine_lock();
    let mut env = tutti_fixtures::ScopedEnv::new();
    env.set("TUTTI_PLUGIN_SERVER", plugin_server_path());
    let Launched { ipc, .. } = Ipc::launch(
        BridgeConfig::default(),
        tutti_fixtures::clap_probe().to_path_buf(),
        tutti_plugin_types::Load::new(),
        SampleRate(48_000.0),
        None,
        crate::format::gui::load_gui_instance,
    )
    .expect("the probe launches in a real plugin-server");
    let main = main_thread();
    let mut control = ipc.controller();
    let any = ipc.controller();

    let catalog = AnyThread::parameter_descriptors(&any).expect("the probe lists its parameters");
    assert!(!catalog.is_empty());
    assert_eq!(
        format!("{:?}", Control::parameters(&control, &main)),
        format!("{catalog:?}"),
        "the catalog"
    );

    let value = AnyThread::parameter_value(&any, GAIN).expect("the probe answers a value");
    assert_eq!(
        Control::parameter(&control, &main, GAIN),
        Some(value),
        "the value"
    );

    let text = AnyThread::parameter_text(&any, GAIN, value).expect("the probe gives a text");
    assert_eq!(
        Control::parameter_text(&control, &main, GAIN, value),
        Some(text.clone()),
        "the text"
    );

    let parsed = AnyThread::parameter_value_from_text(&any, GAIN, &text)
        .expect("the probe parses its own text");
    assert_eq!(
        Control::parameter_value_from_text(&control, &main, GAIN, &text),
        Some(parsed),
        "the parse"
    );

    let state = AnyThread::save_state(&any).expect("the probe saves");
    assert_eq!(
        Control::save_state(&mut control, &main).expect("the probe saves"),
        state,
        "the state"
    );

    assert!(AnyThread::set_render_mode(&any, RenderMode::Offline));
    assert!(Control::set_render_mode(
        &mut control,
        &main,
        RenderMode::Realtime
    ));
    assert!(
        AnyThread::set_automation_mode(&any, tutti_plugin_types::AutomationMode::Off).is_ok(),
        "the advisory is delivered"
    );
    assert!(!Health::is_crashed(&any));
}
