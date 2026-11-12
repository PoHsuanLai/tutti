//! What the plugin does, written out. The figures a case expects are here and
//! nowhere else, and they are not imported from the plugin: a case that asked
//! the plugin what to expect could not fail when the plugin is wrong. The
//! plugin's own copy is the spec in `tutti-shared-plugin`'s `core` module.

use tutti_plugin_types::{ChannelLayout, Normalized, ParamId, Samples};

/// A parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Param {
    /// Persistent, default 0.5; the level the Level mode renders.
    Level,
    /// Persistent, default 0.25.
    Aux,
    /// The mode, read at activation.
    Mode,
    /// The one parameter with a plain domain of its own: a decreasing
    /// exponential, `20000 * 1000^(-n)` for the normalized `n`. It changes
    /// nothing a block renders.
    Tone,
}

impl Param {
    /// Every parameter the plugin lists, with its default.
    pub const ALL: [(Param, f64); 4] = [
        (Param::Level, 0.5),
        (Param::Aux, 0.25),
        (Param::Mode, 0.0),
        (Param::Tone, 0.5),
    ];

    /// The id every format lists it under.
    #[must_use]
    pub const fn id(self) -> ParamId {
        ParamId::new(self as u32)
    }
}

impl Param {
    /// The name every format lists it under.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Param::Level => "level",
            Param::Aux => "aux",
            Param::Mode => "mode",
            Param::Tone => "tone",
        }
    }
}

/// The normalized values the conversion cases ask about.
pub const TONE_AT: [f64; 4] = [0.0, 0.25, 0.5, 1.0];

/// What a format's own conversion answers for [`Param::Tone`]. Each format's
/// figures follow from its spec, not from the plugin: VST3 asks the
/// controller's `normalizedParamToPlain` (`ivsteditcontroller.h:508`), whose map
/// is the plugin's, here `20000 * 1000^(-n)`; CLAP has no normalized value
/// (`ext/params.h` speaks plain `double`s), so its plain value is the host's
/// position over the range the plugin declares, 20 to 20000; VST2 declares no
/// integer range, so it has no plain value at all.
#[derive(Clone, Copy, Debug)]
pub struct Tone {
    /// What the parameter list reports as the default: the plain value where
    /// the format has one, the normalized value where it does not.
    pub default: f64,
    /// The declared bounds, `None` where the format declares none.
    pub bounds: Option<(f64, f64)>,
    /// The plain value at each of [`TONE_AT`], `None` where the format has no
    /// plain value for the parameter.
    pub plain: Option<[f64; 4]>,
}

/// VST3: the plugin's own curve, exact at its default, 20000 / sqrt(1000).
pub const TONE_CURVED: Tone = Tone {
    default: 632.455_532_033_675_8,
    bounds: Some((20.0, 20_000.0)),
    plain: Some([
        20_000.0,
        3_556.558_820_077_845_7,
        632.455_532_033_675_8,
        20.0,
    ]),
};

/// CLAP: the declared range 20 to 20000 as the host's line, the plugin's own
/// plain default.
pub const TONE_LINEAR: Tone = Tone {
    default: 632.455_532_033_675_8,
    bounds: Some((20.0, 20_000.0)),
    plain: Some([20.0, 5_015.0, 10_010.0, 20_000.0]),
};

/// VST2: normalized only.
pub const TONE_NORMALIZED: Tone = Tone {
    default: 0.5,
    bounds: None,
    plain: None,
};

/// Blocks the plugin's transcript keeps; one more overflows it.
pub const LOG_BLOCKS: usize = 16;

/// An id the plugin does not list.
pub const UNLISTED: ParamId = ParamId::new(77);

/// What a block renders. `tag` is `bus * 1000 + channel + 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `in + tag`.
    Tag,
    /// `in` delayed by [`LATENCY`], plus `tag`; reports [`LATENCY`].
    Delay,
    /// The level, as it stands at each sample.
    Level,
    /// `key + 1000 * channel` while a note is held, else 0.
    Notes,
    /// Writes nothing.
    Silent,
}

impl Mode {
    /// The number a state blob stores.
    #[must_use]
    pub const fn index(self) -> u32 {
        self as u32
    }

    /// The normalized value of the mode parameter that selects it.
    #[must_use]
    pub fn value(self) -> Normalized {
        Normalized::clamped(f64::from(self.index()) / 4.0)
    }
}

/// The plugin's one bus a side: a stereo main, in every format.
pub const MAIN_BUS: ChannelLayout = ChannelLayout::STEREO;

/// The Delay mode's latency.
pub const LATENCY: Samples = Samples(137);
/// The tail, in every mode.
pub const TAIL: Samples = Samples(8191);

/// The tag on channel `channel` of the main bus.
#[must_use]
pub fn tag(channel: usize) -> f32 {
    (channel + 1) as f32
}

/// The editor behaviour that offers a view (`EDITOR_TIDY` in the plugin's
/// state word, second byte).
pub const EDITOR_TIDY: u32 = 1;
/// The notice behaviour that raises one notice per state load (`NOTICE_ON_LOAD`,
/// third byte).
pub const NOTICE_ON_LOAD: u32 = 1;

/// What a format's control surface answers for the shared plugin, the same in
/// a loaded plugin and an active one. One row per format ([`Surface`]).
#[derive(Clone, Copy, Debug)]
pub struct Surface {
    /// Whether the plugin's shell reads the state's editor byte: only the Rust
    /// VST3 shell does, offering a view once it says [`EDITOR_TIDY`]. Where
    /// `false` the case does not send the byte (the C++ plugin refuses it).
    pub selects_editor: bool,
    /// What `has_editor` answers once the case has sent what the shell reads.
    /// The Rust VST3 shell's view embeds only an X11 window, so a host on
    /// Windows or macOS is right to see none.
    pub has_editor: bool,
    /// What `set_render_mode(Offline)` answers: `true` where the format takes
    /// the mode live (VST2: the host's own flag), `false` where it is read at
    /// activation (VST3) or the plugin has no render extension (the shared
    /// CLAP shell exports none).
    pub render_mode_live: bool,
    /// Whether the shell reads the notice byte ([`NOTICE_ON_LOAD`]): CLAP, VST2
    /// and VST3, which tell the host something on each state load.
    pub notices: bool,
    /// The request a notice raises, as `take_requests` reports it: CLAP's
    /// `request_callback`. Nothing for every other format.
    pub request: Option<tutti_plugin_types::Request>,
    /// The edit a notice reports outside a block, as `take_reported_edits`
    /// reads it: VST2's `audioMasterAutomate(0, 0.5)`, VST3's `performEdit` of
    /// `aux` to 0.75 through the host's component handler. Nothing for CLAP,
    /// which reports edits in a block's output, and for AU.
    pub edit: Option<(Param, f64)>,
    /// What the format's conversion answers for [`Param::Tone`].
    pub tone: Tone,
}

/// CLAP: no editor in the plugin, no `clap.render`, a callback request.
pub const CLAP: Surface = Surface {
    selects_editor: false,
    has_editor: false,
    render_mode_live: false,
    notices: true,
    request: Some(tutti_plugin_types::Request::Callback),
    edit: None,
    tone: TONE_LINEAR,
};

/// VST2: no editor in the plugin, a live render mode, an automate report.
pub const VST2: Surface = Surface {
    selects_editor: false,
    has_editor: false,
    render_mode_live: true,
    notices: true,
    request: None,
    edit: Some((Param::Level, 0.5)),
    tone: TONE_NORMALIZED,
};

/// VST3 from the Rust plugin: an editor once selected (where its X11 view can
/// embed), the mode read at activation, an edit of `aux` to 0.75 on each state
/// load.
pub const VST3: Surface = Surface {
    selects_editor: true,
    has_editor: cfg!(all(unix, not(target_os = "macos"))),
    render_mode_live: false,
    notices: true,
    request: None,
    edit: Some((Param::Aux, 0.75)),
    tone: TONE_CURVED,
};

/// VST3 from the C++ plugin: no editor, an edit of `aux` to 0.75 on each state
/// load.
pub const VST3_CPP: Surface = Surface {
    selects_editor: false,
    has_editor: false,
    render_mode_live: false,
    notices: true,
    request: None,
    edit: Some((Param::Aux, 0.75)),
    tone: TONE_CURVED,
};

/// AU, from an Apple built-in unit (AUDelay): it has a Cocoa view; no plugin of
/// ours to select an editor or raise a notice, and no live render mode (the
/// property is read at initialization, so an active unit refuses it).
pub const AU: Surface = Surface {
    selects_editor: false,
    has_editor: true,
    render_mode_live: false,
    notices: false,
    request: None,
    edit: None,
    tone: TONE_NORMALIZED,
};
