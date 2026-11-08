//! The three instruments, as subjects.

use tutti_polysynth::{FilterType, OscillatorType, PolySynth, SynthConfig};
use tutti_session::PluginLoadRequest;
use tutti_soundfont::{Settings, SoundFont, SoundFontUnit};
use tutti_types::{Adsr, Amplitude, Seconds};

use super::cases::{Gap, Subject};
use super::rig::{RATE, Rig, offline_session};

/// `PolySynth`: a saw, a 1 ms attack and a 50 ms release.
pub struct Poly;

impl Subject for Poly {
    const NAME: &'static str = "PolySynth";
    const GAPS: &'static [Gap] = &[];
    const PITCHED: bool = true;
    const SETTLE: usize = 4_800;
    const LEAD: (usize, usize) = (0, 4);
    const CUT: usize = 1;

    fn rig() -> Rig {
        let synth = PolySynth::new(SynthConfig {
            sample_rate: RATE,
            max_voices: 8,
            oscillator: OscillatorType::Saw,
            filter: FilterType::None,
            envelope: Adsr {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.05),
            },
            ..SynthConfig::default()
        })
        .expect("builds");
        let mut session = offline_session();
        let (handle, _controls) = session
            .add_instrument(synth)
            .expect("a synth is an instrument");
        Rig::wired(session, handle)
    }
}

/// The SoundFont unit over the repository's test font, the first preset.
pub struct Soundfont;

impl Subject for Soundfont {
    const NAME: &'static str = "SoundFontUnit";
    const GAPS: &'static [Gap] = &[];
    const PITCHED: bool = true;
    /// The release of the font's first preset, measured: silent within 6 s.
    const SETTLE: usize = 6 * 48_000;
    /// The lead of the font's attack ramp, and the unit's 8-frame event
    /// resolution either side.
    const LEAD: (usize, usize) = (0, 64);
    const CUT: usize = 8;

    fn rig() -> Rig {
        let mut settings = Settings::new(RATE);
        settings.reverb_and_chorus = false;
        let path = tutti_fixtures::asset("soundfonts/TimGM6mb.sf2");
        let mut file = std::fs::File::open(&path).expect("the test font is in the checkout");
        let font = std::sync::Arc::new(SoundFont::new(&mut file).expect("the test font parses"));
        let unit = SoundFontUnit::new(font, &settings).expect("builds");
        let mut session = offline_session();
        let (handle, ()) = session
            .add_instrument(unit)
            .expect("a SoundFont unit is an instrument");
        Rig::wired(session, handle)
    }
}

/// The shared test plugin (`tutti-shared-plugin`) in `Notes` mode: a note gate
/// that outputs `key + 1000 * channel` while a note is held. The mode is a word
/// in the state it is loaded with, as the plugin contract requires: no
/// environment variable, symbol or process global. In a real `plugin-server`.
pub struct Plugin;

impl Plugin {
    /// A session and the plugin loaded for it, not yet in the graph.
    pub fn load(
        path: std::path::PathBuf,
        state: Option<Vec<u8>>,
    ) -> (tutti_session::Session, tutti_session::PluginLoaded) {
        // SAFETY: nextest runs every test in its own process and this runs
        // before the first server is spawned, on the test's own thread.
        unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server()) };
        let session = offline_session();
        let loaded = session
            .plugin_load(PluginLoadRequest {
                id: tutti_plugin::catalog::PluginId::from_path(path),
                state,
            })
            .run();
        (session, loaded)
    }

    /// The state that puts the shared plugin in `Notes` mode: its mode
    /// parameter set, saved (`core.rs`, "State").
    pub fn notes_state() -> Vec<u8> {
        let core = tutti_shared_plugin::core::Core::new();
        core.set_param(tutti_shared_plugin::core::MODE, 3.0 / 4.0);
        core.save()
    }

    /// The shared plugin, state in `Notes` mode.
    pub fn notes() -> (tutti_session::Session, tutti_session::PluginLoaded) {
        let real = tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin");
        Self::load(
            tutti_fixtures::publish_with_extension(&real, "clap"),
            Some(Self::notes_state()),
        )
    }

    /// The reference probe, which declares itself an effect.
    pub fn effect() -> (tutti_session::Session, tutti_session::PluginLoaded) {
        let real = tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin");
        Self::load(tutti_fixtures::publish_with_extension(&real, "clap"), None)
    }
}

impl Subject for Plugin {
    const NAME: &'static str = "SharedPluginNotes";
    const GAPS: &'static [Gap] = &[];
    const PITCHED: bool = false;
    const SETTLE: usize = 1_024;
    const LEAD: (usize, usize) = (0, 0);
    const CUT: usize = 1;

    /// The plugin's own record of what the host sent it, read from its state.
    fn transcript(rig: &Rig) -> Option<tutti_shared_plugin::transcript::Transcript> {
        let state = rig
            .session
            .plugin(rig.node)?
            .handle()
            .state()
            .save_state()
            .expect("the plugin saves its state");
        // Not `complete()`: its overflow flag is also set by more blocks than the
        // log keeps, and a case this long renders more. The events are few.
        Some(tutti_shared_plugin::transcript::Transcript::parse(
            &tutti_shared_plugin::transcript::find_blob(&state),
        ))
    }

    fn rig() -> Rig {
        let (mut session, loaded) = Self::notes();
        let handle = session
            .instrument_plugin(loaded)
            .expect("the plugin declares itself an instrument");
        let mut rig = Rig::wired(session, handle);
        rig.pace = std::time::Duration::from_millis(10);
        rig
    }
}
