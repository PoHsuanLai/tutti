//! A hosted plugin's presets through the main-thread view (design 029 step 4):
//! what the format can do with them, a list that loads by id, a file that
//! loads by path, and the three typed answers of a load.
//!
//! CLAP over the reference plugin in a real `plugin-server` (with `TUTTI_CLAP_PROBE_PRESET_LOAD=1` it takes a preset
//! file with the suffix `.tutti-preset` and sets its Cutoff, observed through
//! its saved state); VST2 over the reference plugin in process, whose program
//! count the environment sets.

use tutti_plugin::catalog::PluginRef;
use tutti_plugin_types::{MainThread, PresetError};
use tutti_session::{PluginLoadRequest, PresetKind, Session};
use tutti_types::{ChannelLayout, SampleRate};

fn main_thread() -> MainThread {
    MainThread::claim().expect("one test, one process")
}

#[cfg(feature = "clap")]
mod clap {
    use super::*;
    use crate::support::{clap_probe, plugin_server};
    use tutti_graph::NodeKey;
    use tutti_types::{MaxBlock, Samples};

    fn loaded() -> (Session, NodeKey) {
        // SAFETY: nextest runs each test in its own process, and this is set
        // before the first server is spawned, from the test's own thread.
        unsafe {
            std::env::set_var("TUTTI_PLUGIN_SERVER", plugin_server());
            // The reference plugin answers `clap.preset-load` only when asked.
            std::env::set_var("TUTTI_CLAP_PROBE_PRESET_LOAD", "1");
        }
        let mut s = Session::builder()
            .build_offline(
                SampleRate::SR_48K,
                ChannelLayout::STEREO,
                MaxBlock::of(Samples(512)),
            )
            .expect("an offline session builds");
        let node = s
            .add_plugin_blocking(
                &main_thread(),
                PluginLoadRequest::new(PluginRef::file(clap_probe()).expect("a plugin file")),
            )
            .expect("the reference CLAP plugin loads")
            .erase();
        (s, node)
    }

    fn saved(s: &Session, node: NodeKey) -> Vec<u8> {
        s.plugin(node)
            .unwrap()
            .state()
            .save()
            .expect("the plugin saves")
            .bytes()
            .to_vec()
    }

    /// **CLAP loads a preset from a file and lists none.** Its kind is `File`
    /// with an empty list; a file it accepts changes its state, and one it
    /// declines is `Refused` and leaves the state alone, so the load reached the
    /// plugin and the plugin's answer reached the caller.
    #[test]
    fn clap_loads_a_preset_file_and_lists_nothing() {
        let (mut s, node) = loaded();
        let mut ui = s.main(main_thread());
        let plugin = ui.plugin(node).unwrap();
        let presets = plugin.presets().expect("CLAP has a preset-load extension");
        assert_eq!(presets.kind(), PresetKind::File);
        assert!(presets.is_empty(), "CLAP enumerates nothing");
        assert_eq!(presets.current(), None, "and has no current-preset query");

        let before = saved(ui.session(), node);
        let presets_again = ui.plugin(node).unwrap();
        let presets = presets_again.presets().unwrap();
        assert_eq!(
            presets.load_file("/nowhere/not-a-preset.txt"),
            Err(PresetError::Refused),
            "the plugin declined a file of the wrong kind"
        );
        assert_eq!(
            saved(ui.session(), node),
            before,
            "a refused load changes nothing"
        );

        let plugin = ui.plugin(node).unwrap();
        let presets = plugin.presets().unwrap();
        assert_eq!(presets.load_file("/nowhere/bright.tutti-preset"), Ok(()));
        assert_ne!(
            saved(ui.session(), node),
            before,
            "an accepted preset file moved the plugin's own state"
        );
    }
}

#[cfg(feature = "vst2")]
mod vst2 {
    use super::*;
    use tutti_fixtures::ScopedEnv;
    use tutti_plugin_types::PresetId;

    fn probe_with(programs: &str) -> (Session, tutti_graph::NodeKey, ScopedEnv) {
        let mut env = ScopedEnv::new();
        env.set("TUTTI_VST2_PROBE_PROGRAMS", programs);
        let mut s = Session::builder()
            .outputs_floor(ChannelLayout::STEREO)
            .build_detached(SampleRate::DEFAULT)
            .expect("a detached session builds");
        let node = s
            .add_plugin_blocking(
                &main_thread(),
                PluginLoadRequest::new(
                    PluginRef::file(tutti_fixtures::vst2_probe().to_path_buf())
                        .expect("a plugin file"),
                ),
            )
            .expect("the in-process VST2 loads")
            .erase();
        (s, node, env)
    }

    /// **A listed preset loads by the id it was listed with.** Browse kind, a
    /// list of the probe's three programs, the third loaded from its entry and
    /// confirmed by the plugin's own current-program query; an id the plugin
    /// never listed is not an entry, and a file load is `Unsupported`.
    #[test]
    fn a_listed_preset_loads_by_its_id() {
        let (mut s, node, _env) = probe_with("3");
        let mut ui = s.main(main_thread());
        let plugin = ui.plugin(node).unwrap();
        let presets = plugin.presets().expect("VST2 programs are presets");
        assert_eq!(presets.kind(), PresetKind::Browse);
        assert_eq!(presets.len(), 3, "the probe declared three programs");

        let third = presets.iter().nth(2).expect("three entries");
        let id = third.id().clone();
        assert_eq!(third.load(), Ok(()));
        assert_eq!(presets.current(), Some(id.clone()), "the plugin took it");

        assert!(presets.get(&id).is_some());
        assert!(presets.get(&PresetId::Number(77)).is_none());
        assert_eq!(
            presets.load_file("/x.tutti-preset"),
            Err(PresetError::Unsupported),
            "a list-and-load format takes no file"
        );
    }

    /// **A plugin that declares no programs answers `Unsupported`**, not an
    /// empty browser: the format was asked and declined.
    #[test]
    fn a_plugin_with_no_programs_has_no_presets() {
        let (mut s, node, _env) = probe_with("0");
        let mut ui = s.main(main_thread());
        let plugin = ui.plugin(node).unwrap();
        assert!(matches!(plugin.presets(), Err(PresetError::Unsupported)));
    }
}
