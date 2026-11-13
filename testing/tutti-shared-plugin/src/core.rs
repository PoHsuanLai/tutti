//! The oracle: what the plugin does, independent of any plugin format.
//!
//! Every format shell decodes its host's calls into the calls below and
//! encodes the answers back. Nothing in this file names a format SDK.
//!
//! # Spec
//!
//! Parameters, ids the same in every format: `LEVEL` 0 (default 0.5), `AUX` 1
//! (default 0.25), `MODE` 2 (five steps, value `k / 4`, default 0), `TONE` 3
//! (default 0.5). `TONE` is the one parameter with a plain domain of its own, a
//! non-linear and **decreasing** map: `plain = 20000 * 1000^(-n)` for the
//! normalized `n` (20000 at 0, 20 at 1, `20000 / sqrt(1000)` = 632.455... at the
//! default). It changes nothing a block renders and is not in the state. A
//! format that speaks normalized (VST3) answers that map from its plain
//! conversion calls; one that speaks plain (CLAP) declares the range 20 to 20000
//! with the default 632.455... and takes and reports the plain value; VST2 has
//! no plain domain and speaks `n`.
//!
//! Mode, read when the plugin is prepared, and what a block renders. `tag` is
//! `bus * 1000 + channel + 1`.
//!
//! | mode | name | output |
//! |---|---|---|
//! | 0 | Tag | `in[c][i] + tag` |
//! | 1 | Delay | `in[c][i - 137] + tag`; reports latency 137 |
//! | 2 | Level | `level` as it stands at sample `i` |
//! | 3 | Notes | `key + 1000 * c` while a note is held, else 0 |
//! | 4 | Silent | writes nothing |
//!
//! Tail 8191 in every mode, unless the state's tail nibble (the low nibble of the
//! fourth byte of the mode word, `mode | editor << 8 | notice << 16 | (tail |
//! rings << 4) << 24`) selects the infinite marker (`TAIL_INFINITE`) or a finite
//! 61 s (`TAIL_PAST_CAP`). That is what the plugin **reports**. A parameter event
//! moves `level` or `aux` at its sample offset.
//!
//! # Faults
//!
//! The high nibble of the mode word's low byte (`mode | fault << 4`) makes
//! `process` fail, for a probe that measures a plugin which dies rendering
//! (doc 023 section 6): `FAULT_CRASH` aborts the process in the first block,
//! `FAULT_HANG` never returns from it. Loading and describing are untouched.
//!
//! # Rings
//!
//!
//! What the plugin **does** with its tail, for the probe that measures one
//! (doc 023 section 6): the high nibble of the fourth byte, `RINGS_*`. With it
//! non-zero the plugin ignores its mode and the `tag` (a probe subject has no DC
//! offset to settle past) and renders a ring: any frame with a non-zero input
//! on any channel starts it, at age 0; at an age from `onset` to `onset + length`
//! inclusive every output channel is `RING_LEVEL` (0.1), at any other age 0.
//! A new trigger restarts the age. The ring's `length` is `RING_FRAMES` unless
//! the behaviour says otherwise, and has nothing to do with the tail it reports:
//!
//! | rings | name | `length` | `onset` | also |
//! |---|---|---|---|---|
//! | 0 | None | | | the modes above |
//! | 1 | Exact | `RING_FRAMES` (8191) | 0 | |
//! | 2 | Longer | `RING_FRAMES + RING_EXTRA` (12287) | 0 | |
//! | 3 | Hiss | `RING_FRAMES` | 0 | `HISS_LEVEL` (0.001, -60 dB) of noise on every frame |
//! | 4 | Late | `RING_FRAMES` | `LATE_FRAMES` (64) | reports the latency of the mode (0 in Tag) |
//!
//! So a probe of an `Exact` plugin finds its last frame above the floor at 8191
//! and, against the reported tail of 8191, calls it honest; against a tail byte
//! of `TAIL_INFINITE`, unchecked.
//!
//! # Editor
//!
//! Only the VST3 Rust shell has an editor, and its behaviour is the second
//! byte of the state's mode word (`mode | editor << 8`; 0 when a blob leaves it
//! out): `EDITOR_NONE` offers no view; `EDITOR_TIDY` registers one run-loop
//! timer and one file descriptor when the view is attached, as VSTGUI does on
//! Linux, and unregisters both when it is removed; `EDITOR_LEAKY` registers the
//! same and never unregisters them. The host's run-loop bookkeeping is the
//! witness, so the behaviour needs no transcript. `EDITOR_FAILING` registers and
//! then fails `attached`; `EDITOR_LATE` registers (and leaks) from `onSize`. The view also aborts the
//! process if `removed` runs on a thread other than the one `attached` did.
//!
//! # Notice
//!
//! The third byte of the mode word (`mode | editor << 8 | notice << 16`) is the
//! notice behaviour: `NOTICE_NONE`, or `NOTICE_ON_LOAD`, which makes every
//! shell but the AU tell the host something on every state load, so a host's
//! read of what a plugin asked for has a witness in either state: CLAP a
//! `request_callback`, VST2 `audioMasterAutomate(0, 0.5)`, VST3 an edit of
//! `aux` to 0.75 (`beginEdit`, `performEdit`, `endEdit` through the host's
//! `IComponentHandler`, from the controller's `setComponentState`, or from the
//! component's `setState` in the combined Rust shell). The C++ VST3 plugin takes
//! the notice byte and refuses the editor byte.
//!
//! `NOTICE_GESTURE_ON_LOAD` makes every shell but the AU report one **gesture**
//! on `aux` on every state load, which a host reads as four edits in this order:
//! the gesture's begin, a change to 0.375, a change to 0.625, the gesture's end.
//! The begin and the end carry no value in any format. VST2 reports
//! `audioMasterBeginEdit(1)`, `audioMasterAutomate(1, 0.375)`,
//! `audioMasterAutomate(1, 0.625)`, `audioMasterEndEdit(1)` from `setChunk`;
//! VST3 `beginEdit`, `performEdit` twice and `endEdit` through the handler, as
//! `NOTICE_ON_LOAD` does; CLAP the output events `PARAM_GESTURE_BEGIN`,
//! `PARAM_VALUE` twice and `PARAM_GESTURE_END`, at frame 0 of the first `process`
//! or `params.flush` after the load, since CLAP has no other way for a plugin
//! to say it. The plugin's own `aux` does not move: the report is the witness.
//!
//! # VST3 controllers and flushes
//!
//! Only the VST3 shells do these, and both cores must (one spec):
//!
//! - **`IMidiMapping`**, event bus 0, channel 0: controller 74 drives `level`;
//!   every other (bus, channel, controller) is unmapped (`kResultFalse`). The
//!   mapped controller reaches the plugin as a parameter point, the unmapped
//!   ones are not heard (VST3 has no event for them).
//! - **`outputParameterChanges`**: a note on of key `k` on channel 15 writes the
//!   point `aux = k / 127` at the note's offset into the block's
//!   `outputParameterChanges`, as a processor-driven parameter (a meter, a preset
//!   change) does. The processor's own `aux` does not move: the point is the
//!   report, and the host applies it to the controller.
//! - **A data event** (`kMidiSysEx`) in the input list is recorded as kind 5.
//! - **A null `inputParameterChanges`** is recorded as kind 6, at offset 0, once
//!   per process call that carried one.
//! - **A process call with `numSamples == 0`** is a parameter flush: kind 7 at
//!   offset 0, the call's parameter points are recorded (kind 1) and applied at
//!   once, and no block is counted (`calls` stays), nothing is rendered.
//!
//! # State
//!
//! State is a little-endian blob. The head is what a load restores; the rest is
//! the transcript, what the host did to this activation, recorded as it
//! arrived and before the plugin orders or repairs anything.
//!
//! | at | field |
//! |---|---|
//! | 0 | magic `TSP1` (u32) |
//! | 4 | mode (u32): the low byte is the mode; the next byte is the editor behaviour (below) |
//! | 8, 16 | `level`, `aux` (f64) |
//! | 24 | process calls (u64), then total frames (u64) |
//! | 40 | prepare calls (u32), rate (f64), max block (u32) |
//! | 56 | overflow (u32): 1 when the host sent more blocks or events than the log keeps or the plugin queues, else 0 |
//! | 60 | `n` (u32), then the last `n` block sizes (u32 each), oldest first |
//! | after | `m` (u32), then `m` events of 24 bytes in arrival order |
//! | after | `t` (u32), the transports received since the activation, then the first `min(t, 16)` of 32 bytes each, in arrival order |
//!
//! A transport is what the host told the plugin of its transport, as it was
//! sent, before the plugin reads anything from it: the process call it came
//! with (u32, counted from 0 since the activation) and its sample offset in
//! that block (u32), the flags (u32: `T_PLAYING`, `T_RECORDING`, `T_CYCLE`,
//! and `T_TEMPO`, `T_BEAT` when the host marked that value valid), a zero
//! (u32), the tempo in BPM (f64) and the musical position in quarter notes
//! (f64), both as the host wrote them whatever their flag. A block's own
//! transport is at offset 0; CLAP also sends one at the frame of each change
//! inside a block (`CLAP_EVENT_TRANSPORT`), and VST3 and VST2 cannot. VST3
//! sends a context with every block; a VST2 plugin asks for one
//! (`audioMasterGetTime`) once per block. The log is separate from the
//! events, so a host that sends one every block never overflows them; `t`
//! counts what it did not keep.
//!
//! An event is `kind << 32 | offset` (u64; kind 1 parameter, 2 note on, 3 note
//! off, 4 control change, 5 data event, 6 null parameter list, 7 flush), the
//! parameter id, key or controller number (u64; a data event's first byte, 0
//! for 6 and 7), the value (f64; a control change's value, 0 to 127; a data
//! event's size in bytes, 0 for 6 and 7). A control change is
//! recorded and has no effect on the output: the plugin is a black box to the
//! host, and the transcript is how a test sees that the host delivered one, and
//! where (a format delivers it as an event: CLAP and VST2 do; VST3 carries a
//! controller as a parameter and records none). A host that wraps the
//! blob in a container of its own leaves the bytes intact; a reader finds the
//! magic.

use std::sync::Mutex;
use std::sync::atomic::{
    AtomicU32, AtomicU64, Ordering::Acquire, Ordering::Relaxed, Ordering::Release,
};

/// Parameter ids.
pub const LEVEL: u32 = 0;
/// Second persistent parameter.
pub const AUX: u32 = 1;
/// Mode selector.
pub const MODE: u32 = 2;
/// The parameter with a plain domain: a decreasing exponential (see the spec).
pub const TONE: u32 = 3;
/// Number of parameters.
pub const PARAMS: u32 = 4;
/// Parameter names, by id.
pub const NAMES: [&str; PARAMS as usize] = ["level", "aux", "mode", "tone"];
/// The plain value of `TONE` at its normalized minimum, its largest.
pub const TONE_PLAIN_MAX: f64 = 20_000.0;
/// The plain value of `TONE` at its normalized maximum, its smallest.
pub const TONE_PLAIN_MIN: f64 = 20.0;

/// The plain value of `TONE` at normalized `n`: `20000 * 1000^(-n)`.
pub fn tone_plain(n: f64) -> f64 {
    TONE_PLAIN_MAX * (TONE_PLAIN_MIN / TONE_PLAIN_MAX).powf(n.clamp(0.0, 1.0))
}

/// The normalized value of a plain `TONE`, the inverse of [`tone_plain`].
pub fn tone_normalized(plain: f64) -> f64 {
    ((TONE_PLAIN_MAX / plain.clamp(TONE_PLAIN_MIN, TONE_PLAIN_MAX)).ln()
        / (TONE_PLAIN_MAX / TONE_PLAIN_MIN).ln())
    .clamp(0.0, 1.0)
}
/// Number of modes.
pub const MODES: u32 = 5;
/// The shell offers no editor view.
pub const EDITOR_NONE: u32 = 0;
/// The view registers a timer and a file descriptor with the host's run loop
/// on `attached` and unregisters them on `removed`.
pub const EDITOR_TIDY: u32 = 1;
/// The view registers as [`EDITOR_TIDY`]'s does and never unregisters.
pub const EDITOR_LEAKY: u32 = 2;
/// The view registers as [`EDITOR_LEAKY`]'s does during `attached`, then fails it.
pub const EDITOR_FAILING: u32 = 3;
/// The view registers nothing in `attached` and a timer and a file descriptor in
/// `onSize`, and never unregisters them.
pub const EDITOR_LATE: u32 = 4;
/// Number of editor behaviours.
pub const EDITORS: u32 = 5;
/// The plugin raises nothing of its own accord.
pub const NOTICE_NONE: u32 = 0;
/// Every state load raises one notice to the host, from the control thread:
/// CLAP `request_callback`, VST2 `audioMasterAutomate(0, 0.5)`. The VST3 shells
/// raise none.
pub const NOTICE_ON_LOAD: u32 = 1;
/// Every state load reports one gesture on `aux` (core, "Notice"): its begin,
/// a change to [`GESTURE_VALUES`]`[0]`, a change to `[1]`, its end.
pub const NOTICE_GESTURE_ON_LOAD: u32 = 2;
/// The values the gesture notice's two changes report, in order (both exact in
/// an `f32`, which VST2 carries).
pub const GESTURE_VALUES: [f64; 2] = [0.375, 0.625];
/// Number of notice behaviours.
pub const NOTICES: u32 = 3;
/// Latency of the Delay mode, in frames.
pub const LATENCY: u32 = 137;
/// Reported tail, in frames, by default ([`TAIL_DEFAULT`]).
pub const TAIL: u32 = 8191;
/// The tail behaviour (fourth byte of the mode word) that reports [`TAIL`].
pub const TAIL_DEFAULT: u32 = 0;
/// The tail behaviour that reports the formats' infinite marker, `u32::MAX`
/// (VST3's `kInfiniteTail`; CLAP reads every count from `INT32_MAX` up as
/// infinite). VST2 and AU cannot say it.
pub const TAIL_INFINITE: u32 = 1;
/// The tail behaviour that reports a finite tail of [`TAIL_PAST_CAP_FRAMES`].
pub const TAIL_PAST_CAP: u32 = 2;
/// Number of tail behaviours.
pub const TAILS: u32 = 3;
/// `process` renders (the default).
pub const FAULT_NONE: u32 = 0;
/// The first `process` aborts the process: a plugin that dies while rendering.
pub const FAULT_CRASH: u32 = 1;
/// The first `process` never returns: a plugin that hangs while rendering.
pub const FAULT_HANG: u32 = 2;
/// Number of fault behaviours.
pub const FAULTS: u32 = 3;
/// The plugin renders its modes (the tag and all), the default.
pub const RINGS_NONE: u32 = 0;
/// The plugin rings [`RING_FRAMES`] after its trigger.
pub const RINGS_EXACT: u32 = 1;
/// The plugin rings [`RING_FRAMES`]` + `[`RING_EXTRA`] after its trigger.
pub const RINGS_LONGER: u32 = 2;
/// As [`RINGS_EXACT`], and adds noise of [`HISS_LEVEL`] to every frame.
pub const RINGS_HISS: u32 = 3;
/// As [`RINGS_EXACT`], its onset [`LATE_FRAMES`] after its trigger.
pub const RINGS_LATE: u32 = 4;
/// Number of ring behaviours.
pub const RINGS: u32 = 5;
/// Frames a ring lasts after its onset, in every ring behaviour but
/// [`RINGS_LONGER`]: what [`TAIL`] says, the plugin being honest.
pub const RING_FRAMES: u32 = 8191;
/// Frames [`RINGS_LONGER`] rings beyond [`RING_FRAMES`].
pub const RING_EXTRA: u32 = 4096;
/// What a ring is made of: a held level.
pub const RING_LEVEL: f32 = 0.1;
/// Peak of [`RINGS_HISS`]'s noise: 0.001, -60 dB, above the probe's floor.
pub const HISS_LEVEL: f32 = 0.001;
/// Frames [`RINGS_LATE`] puts between its trigger and its onset.
pub const LATE_FRAMES: u32 = 64;
/// 61 s at 48 kHz: a finite tail past what the graph's `MAX_NODE_TAIL` (60 s)
/// bounds.
pub const TAIL_PAST_CAP_FRAMES: u32 = 48_000 * 61;
/// Most channels on the main bus.
pub const MAX_CHANNELS: usize = 8;
/// Events kept per block.
pub const MAX_EVENTS: usize = 128;
/// Transcript entries kept per activation.
pub const LOG_EVENTS: usize = 32;
/// Block sizes kept in the transcript.
pub const LOG_BLOCKS: usize = 16;
/// Transports kept in the transcript per activation (the first ones).
pub const LOG_TRANSPORTS: usize = 16;
/// A transport's flag: rolling.
pub const T_PLAYING: u32 = 1;
/// A transport's flag: capturing.
pub const T_RECORDING: u32 = 2;
/// A transport's flag: the loop is on.
pub const T_CYCLE: u32 = 4;
/// A transport's flag: the host marked its tempo valid.
pub const T_TEMPO: u32 = 8;
/// A transport's flag: the host marked its musical position valid.
pub const T_BEAT: u32 = 16;
/// First word of a state blob.
pub const MAGIC: u32 = 0x3150_5354;
/// Bytes of a state blob that [`Core::load`] reads.
pub const STATE_HEAD: usize = 24;

/// `bus * 1000 + channel + 1`.
pub fn tag(bus: u32, channel: usize) -> f32 {
    (bus * 1000 + channel as u32 + 1) as f32
}

/// The parameter default.
pub fn default_of(id: u32) -> Option<f64> {
    match id {
        LEVEL => Some(0.5),
        AUX => Some(0.25),
        MODE => Some(0.0),
        TONE => Some(0.5),
        _ => None,
    }
}

/// The mode a normalized value selects.
pub fn mode_of(value: f64) -> u32 {
    ((value.clamp(0.0, 1.0) * f64::from(MODES - 1)).round() as u32).min(MODES - 1)
}

/// A state blob the plugin refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadState;

/// What the host sent inside a block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ev {
    /// A parameter value at a sample offset.
    Param { id: u32, offset: u32, value: f64 },
    /// A note on.
    NoteOn { offset: u32, key: u8 },
    /// A note off.
    NoteOff { offset: u32, key: u8 },
    /// A control change: recorded, never acted on.
    Control {
        offset: u32,
        controller: u8,
        value: u8,
    },
    /// A VST3 data event (`kMidiSysEx`): recorded, never acted on.
    Data { offset: u32, first: u8, size: u32 },
    /// A VST3 process call whose `inputParameterChanges` was null.
    NullParams,
    /// A VST3 process call with `numSamples == 0`.
    Flush,
}

impl Ev {
    fn offset(&self) -> u32 {
        match *self {
            Ev::Param { offset, .. }
            | Ev::NoteOn { offset, .. }
            | Ev::NoteOff { offset, .. }
            | Ev::Control { offset, .. }
            | Ev::Data { offset, .. } => offset,
            Ev::NullParams | Ev::Flush => 0,
        }
    }
}

/// A transport as the host sent it (see the module's spec, "State").
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transport {
    /// Its sample offset in the block.
    pub offset: u32,
    /// `T_*` bits.
    pub flags: u32,
    /// Beats per minute, as written.
    pub tempo: f64,
    /// Quarter notes from the timeline's start, as written.
    pub beat: f64,
}

/// A sample width.
pub trait Sample: Copy + Default {
    /// From an `f32`.
    fn from_f32(v: f32) -> Self;
    /// To an `f64`.
    fn to_f64(self) -> f64;
    /// From an `f64`.
    fn from_f64(v: f64) -> Self;
    /// Takes the input scratch of this width out of `dsp` (no allocation).
    fn take(dsp: &mut Dsp) -> Vec<Vec<Self>>;
    /// Puts it back.
    fn put(dsp: &mut Dsp, v: Vec<Vec<Self>>);
}
impl Sample for f32 {
    fn from_f32(v: f32) -> Self {
        v
    }
    fn to_f64(self) -> f64 {
        f64::from(self)
    }
    fn from_f64(v: f64) -> Self {
        v as f32
    }
    fn take(dsp: &mut Dsp) -> Vec<Vec<Self>> {
        std::mem::take(&mut dsp.scratch32)
    }
    fn put(dsp: &mut Dsp, v: Vec<Vec<Self>>) {
        dsp.scratch32 = v;
    }
}
impl Sample for f64 {
    fn from_f32(v: f32) -> Self {
        f64::from(v)
    }
    fn to_f64(self) -> f64 {
        self
    }
    fn from_f64(v: f64) -> Self {
        v
    }
    fn take(dsp: &mut Dsp) -> Vec<Vec<Self>> {
        std::mem::take(&mut dsp.scratch64)
    }
    fn put(dsp: &mut Dsp, v: Vec<Vec<Self>>) {
        dsp.scratch64 = v;
    }
}

/// State shared by the control and the audio side: atomics only.
#[derive(Debug)]
pub struct Core {
    level: AtomicU64,
    aux: AtomicU64,
    tone: AtomicU64,
    mode: AtomicU32,
    fault: AtomicU32,
    editor: AtomicU32,
    notice: AtomicU32,
    tail: AtomicU32,
    rings: AtomicU32,
    gesture_due: AtomicU32,
    calls: AtomicU64,
    frames: AtomicU64,
    prepares: AtomicU32,
    rate: AtomicU64,
    max_block: AtomicU32,
    blocks_n: AtomicU32,
    overflow: AtomicU32,
    blocks: [AtomicU32; LOG_BLOCKS],
    log_n: AtomicU32,
    log: [[AtomicU64; 3]; LOG_EVENTS],
    told_n: AtomicU32,
    told: [[AtomicU64; 4]; LOG_TRANSPORTS],
}

impl Default for Core {
    fn default() -> Self {
        Self::new()
    }
}

/// The audio side's own storage: allocated at [`Core::prepare`], used without
/// allocating.
#[derive(Debug, Default)]
pub struct Dsp {
    mode: u32,
    level: f64,
    aux: f64,
    held: Option<u8>,
    rings: u32,
    age: Option<u64>,
    noise: u32,
    delay: Vec<Vec<f64>>,
    pos: usize,
    scratch32: Vec<Vec<f32>>,
    scratch64: Vec<Vec<f64>>,
    events: Vec<Ev>,
}

/// The audio side, behind the lock the control side never takes.
pub type SharedDsp = Mutex<Dsp>;

impl Core {
    /// A plugin at its defaults.
    pub fn new() -> Self {
        let z = || AtomicU32::new(0);
        Self {
            level: AtomicU64::new(0.5f64.to_bits()),
            aux: AtomicU64::new(0.25f64.to_bits()),
            tone: AtomicU64::new(0.5f64.to_bits()),
            mode: z(),
            fault: z(),
            editor: z(),
            notice: z(),
            tail: z(),
            rings: z(),
            gesture_due: z(),
            calls: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            prepares: z(),
            rate: AtomicU64::new(0),
            max_block: z(),
            blocks_n: z(),
            overflow: z(),
            blocks: std::array::from_fn(|_| z()),
            log_n: z(),
            log: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
            told_n: z(),
            told: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
        }
    }

    /// The editor behaviour a state load selected (`EDITOR_*`).
    pub fn editor(&self) -> u32 {
        self.editor.load(Relaxed)
    }

    /// The tail the plugin reports, in the formats' raw frames: [`TAIL`], the
    /// infinite marker or [`TAIL_PAST_CAP_FRAMES`], as the state's tail byte
    /// selected (`TAIL_*`).
    pub fn tail_samples(&self) -> u32 {
        match self.tail.load(Relaxed) {
            TAIL_INFINITE => u32::MAX,
            TAIL_PAST_CAP => TAIL_PAST_CAP_FRAMES,
            _ => TAIL,
        }
    }

    /// The ring behaviour a state load selected (`RINGS_*`).
    pub fn rings(&self) -> u32 {
        self.rings.load(Relaxed)
    }

    /// The notice behaviour a state load selected (`NOTICE_*`).
    pub fn notice(&self) -> u32 {
        self.notice.load(Relaxed)
    }

    /// Whether a state load selected [`NOTICE_GESTURE_ON_LOAD`] and its gesture
    /// has not been reported yet; takes the report, so one load is one gesture.
    /// For a shell (CLAP) that can say it only from `process` or a flush.
    pub fn take_gesture(&self) -> bool {
        self.gesture_due.swap(0, Relaxed) != 0
    }

    /// A parameter's normalized value, `None` for an id the plugin lacks.
    pub fn param(&self, id: u32) -> Option<f64> {
        match id {
            LEVEL => Some(f64::from_bits(self.level.load(Relaxed))),
            AUX => Some(f64::from_bits(self.aux.load(Relaxed))),
            TONE => Some(f64::from_bits(self.tone.load(Relaxed))),
            MODE => Some(f64::from(self.mode.load(Relaxed)) / f64::from(MODES - 1)),
            _ => None,
        }
    }

    /// Sets a parameter; an id the plugin lacks is ignored.
    pub fn set_param(&self, id: u32, value: f64) {
        let v = value.clamp(0.0, 1.0);
        match id {
            LEVEL => self.level.store(v.to_bits(), Relaxed),
            AUX => self.aux.store(v.to_bits(), Relaxed),
            TONE => self.tone.store(v.to_bits(), Relaxed),
            MODE => self.mode.store(mode_of(v), Relaxed),
            _ => {}
        }
    }

    /// The latency reported now.
    pub fn latency(&self) -> u32 {
        if self.mode.load(Relaxed) == 1 {
            LATENCY
        } else {
            0
        }
    }

    /// Readies the audio side for `rate` and `max_block`, reading the mode,
    /// and clears the transcript. Allocates; control thread.
    pub fn prepare(&self, dsp: &mut Dsp, rate: f64, max_block: usize) {
        dsp.mode = self.mode.load(Relaxed);
        dsp.rings = self.rings.load(Relaxed);
        dsp.age = None;
        dsp.noise = 0x1234_5678;
        dsp.held = None;
        dsp.pos = 0;
        dsp.delay = (0..MAX_CHANNELS)
            .map(|_| vec![0.0; LATENCY as usize])
            .collect();
        dsp.scratch32 = (0..MAX_CHANNELS).map(|_| vec![0.0; max_block]).collect();
        dsp.scratch64 = (0..MAX_CHANNELS).map(|_| vec![0.0; max_block]).collect();
        dsp.events = Vec::with_capacity(MAX_EVENTS);
        self.calls.store(0, Relaxed);
        self.frames.store(0, Relaxed);
        self.blocks_n.store(0, Relaxed);
        self.overflow.store(0, Relaxed);
        self.log_n.store(0, Relaxed);
        self.told_n.store(0, Relaxed);
        self.rate.store(rate.to_bits(), Relaxed);
        self.max_block.store(max_block as u32, Relaxed);
        self.prepares.fetch_add(1, Relaxed);
    }

    /// Copies `ins` (raw channel reads) into the width's scratch and returns it
    /// taken out of `dsp`; hand it back with [`Sample::put`].
    pub fn stage<S: Sample>(
        &self,
        dsp: &mut Dsp,
        frames: usize,
        read: impl Fn(usize, usize) -> S,
        channels: usize,
    ) -> Vec<Vec<S>> {
        let mut scratch = S::take(dsp);
        for (c, ch) in scratch.iter_mut().enumerate().take(channels) {
            for (i, v) in ch.iter_mut().enumerate().take(frames) {
                *v = read(c, i);
            }
        }
        scratch
    }

    /// Queues an event for the next [`process`](Self::process); drops it when
    /// the block's room is spent (never allocates).
    pub fn push(&self, dsp: &mut Dsp, ev: Ev) {
        // The transcript records arrival order, before the sort a render does.
        match ev {
            Ev::Param { id, offset, value } => self.log(1, offset, id, value),
            Ev::NoteOn { offset, key } => self.log(2, offset, u32::from(key), 0.0),
            Ev::NoteOff { offset, key } => self.log(3, offset, u32::from(key), 0.0),
            Ev::Control {
                offset,
                controller,
                value,
            } => self.log(4, offset, u32::from(controller), f64::from(value)),
            Ev::Data {
                offset,
                first,
                size,
            } => self.log(5, offset, u32::from(first), f64::from(size)),
            Ev::NullParams => self.log(6, 0, 0, 0.0),
            // Logged by `flush`, which is not a block's event.
            Ev::Flush => {}
        }
        if dsp.events.len() < MAX_EVENTS {
            dsp.events.push(ev);
        } else {
            self.overflow.store(1, Relaxed);
        }
    }

    /// A VST3 parameter flush (a process call with `numSamples == 0`): records
    /// it, applies the parameter points queued since the last block at once and
    /// forgets the points. No block is counted and nothing is rendered. Never
    /// allocates.
    pub fn flush(&self, dsp: &mut Dsp) {
        self.log(7, 0, 0, 0.0);
        dsp.events.retain(|ev| match *ev {
            Ev::Param { id, value, .. } => {
                self.set_param(id, value);
                false
            }
            _ => true,
        });
    }

    /// Records a parameter value the host set outside a block's event list, at
    /// offset 0: VST2 has no parameter events, so the host's `setParameter`
    /// calls are what it sent. The log has one writer; a call from another
    /// thread while blocks run can lose an entry.
    pub fn log_set_param(&self, id: u32, value: f64) {
        self.log(1, 0, id, value);
    }

    /// Records a transport the host sent with the block about to be
    /// processed. Never allocates; past [`LOG_TRANSPORTS`] it only counts.
    pub fn log_transport(&self, t: Transport) {
        let n = self.told_n.load(Relaxed);
        if let Some(slot) = self.told.get(n as usize) {
            let call = self.calls.load(Relaxed).min(u64::from(u32::MAX));
            slot[0].store(call << 32 | u64::from(t.offset), Relaxed);
            slot[1].store(u64::from(t.flags), Relaxed);
            slot[2].store(t.tempo.to_bits(), Relaxed);
            slot[3].store(t.beat.to_bits(), Relaxed);
        }
        self.told_n.store(n.saturating_add(1), Release);
    }

    fn log(&self, kind: u64, offset: u32, id: u32, value: f64) {
        let n = self.log_n.load(Relaxed) as usize;
        if n < LOG_EVENTS {
            self.log[n][0].store(kind << 32 | u64::from(offset), Relaxed);
            self.log[n][1].store(u64::from(id), Relaxed);
            self.log[n][2].store(value.to_bits(), Relaxed);
            self.log_n.store(n as u32 + 1, Release);
        } else {
            self.overflow.store(1, Relaxed);
        }
    }

    /// Renders one block of `frames` on bus `bus`. `ins` and `outs` are the
    /// bus's channels; `ins` must not alias `outs` (see
    /// [`Sample::take`]).
    pub fn process<S: Sample>(
        &self,
        dsp: &mut Dsp,
        frames: usize,
        bus: u32,
        ins: &[&[S]],
        outs: &mut [&mut [S]],
    ) {
        match self.fault.load(Relaxed) {
            FAULT_CRASH => std::process::abort(),
            FAULT_HANG => loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
            },
            _ => {}
        }
        let n = self.calls.fetch_add(1, Relaxed);
        self.frames.fetch_add(frames as u64, Relaxed);
        if n as usize >= LOG_BLOCKS {
            self.overflow.store(1, Relaxed);
        }
        let slot = (n as usize) % LOG_BLOCKS;
        self.blocks[slot].store(frames as u32, Relaxed);
        self.blocks_n
            .store((n + 1).min(u64::from(u32::MAX)) as u32, Release);

        dsp.level = f64::from_bits(self.level.load(Relaxed));
        dsp.aux = f64::from_bits(self.aux.load(Relaxed));
        // Stable insertion sort by offset (`sort_by_key` allocates). The
        // transcript above was written at arrival, so this repair of the
        // host's order does not hide it.
        for i in 1..dsp.events.len() {
            let mut j = i;
            while j > 0 && dsp.events[j - 1].offset() > dsp.events[j].offset() {
                dsp.events.swap(j - 1, j);
                j -= 1;
            }
        }
        let mut events = std::mem::take(&mut dsp.events);
        let mut pos = 0usize;
        for ev in &events {
            let at = (ev.offset() as usize).min(frames);
            self.render(dsp, pos, at, bus, ins, outs);
            pos = at;
            match *ev {
                Ev::Param { id, value, .. } => {
                    self.set_param(id, value);
                    match id {
                        LEVEL => dsp.level = value.clamp(0.0, 1.0),
                        AUX => dsp.aux = value.clamp(0.0, 1.0),
                        _ => {}
                    }
                }
                Ev::NoteOn { key, .. } => {
                    dsp.held = Some(key);
                }
                Ev::NoteOff { key, .. } => {
                    if dsp.held == Some(key) {
                        dsp.held = None;
                    }
                }
                // Recorded when it arrived; the output ignores it.
                Ev::Control { .. } | Ev::Data { .. } | Ev::NullParams | Ev::Flush => {}
            }
        }
        self.render(dsp, pos, frames, bus, ins, outs);
        dsp.pos = (dsp.pos + frames) % (LATENCY as usize);
        events.clear();
        dsp.events = events;
    }

    fn render<S: Sample>(
        &self,
        dsp: &mut Dsp,
        from: usize,
        to: usize,
        bus: u32,
        ins: &[&[S]],
        outs: &mut [&mut [S]],
    ) {
        if dsp.rings != RINGS_NONE {
            return Self::render_rings(dsp, from, to, ins, outs);
        }
        for (c, out) in outs.iter_mut().enumerate() {
            let t = tag(bus, c);
            for i in from..to.min(out.len()) {
                let x = ins.get(c).map_or(0.0, |s| s[i].to_f64());
                match dsp.mode {
                    0 => out[i] = S::from_f64(x + f64::from(t)),
                    1 => {
                        let line = &mut dsp.delay[c.min(MAX_CHANNELS - 1)];
                        let k = (dsp.pos + i) % line.len();
                        let old = line[k];
                        line[k] = x;
                        out[i] = S::from_f64(old + f64::from(t));
                    }
                    2 => out[i] = S::from_f64(dsp.level),
                    3 => {
                        out[i] =
                            S::from_f64(dsp.held.map_or(0.0, |k| f64::from(k) + 1000.0 * c as f64));
                    }
                    _ => {}
                }
            }
        }
    }

    /// The ring behaviours' render (the module's "Rings"): frame by frame, since
    /// the age is the frame's, not a channel's.
    fn render_rings<S: Sample>(
        dsp: &mut Dsp,
        from: usize,
        to: usize,
        ins: &[&[S]],
        outs: &mut [&mut [S]],
    ) {
        let (length, onset) = match dsp.rings {
            RINGS_LONGER => (RING_FRAMES + RING_EXTRA, 0),
            RINGS_LATE => (RING_FRAMES, LATE_FRAMES),
            _ => (RING_FRAMES, 0),
        };
        let hiss = if dsp.rings == RINGS_HISS {
            HISS_LEVEL
        } else {
            0.0
        };
        let frames = outs.iter().map(|o| o.len()).min().unwrap_or(0);
        for i in from..to.min(frames) {
            if ins.iter().any(|ch| ch[i].to_f64() != 0.0) {
                dsp.age = Some(0);
            }
            let ringing = dsp.age.is_some_and(|a| {
                (u64::from(onset)..=u64::from(onset) + u64::from(length)).contains(&a)
            });
            for out in outs.iter_mut() {
                // A xorshift32: deterministic, and never zero.
                dsp.noise ^= dsp.noise << 13;
                dsp.noise ^= dsp.noise >> 17;
                dsp.noise ^= dsp.noise << 5;
                let noise = (dsp.noise as f32 / u32::MAX as f32 * 2.0 - 1.0) * hiss;
                let ring = if ringing { RING_LEVEL } else { 0.0 };
                out[i] = S::from_f32(ring + noise);
            }
            dsp.age = dsp.age.map(|a| a + 1);
        }
    }

    /// Silences the audio side's memory: delay line and held note.
    pub fn reset(&self, dsp: &mut Dsp) {
        dsp.held = None;
        dsp.age = None;
        for l in &mut dsp.delay {
            l.iter_mut().for_each(|v| *v = 0.0);
        }
    }

    /// Serializes the state: the head [`Core::load`] reads, then the
    /// transcript.
    pub fn save(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(512);
        b.extend_from_slice(&MAGIC.to_le_bytes());
        let word = self.mode.load(Relaxed)
            | self.fault.load(Relaxed) << 4
            | self.editor.load(Relaxed) << 8
            | self.notice.load(Relaxed) << 16
            | (self.tail.load(Relaxed) | self.rings.load(Relaxed) << 4) << 24;
        b.extend_from_slice(&word.to_le_bytes());
        b.extend_from_slice(&self.level.load(Relaxed).to_le_bytes());
        b.extend_from_slice(&self.aux.load(Relaxed).to_le_bytes());
        b.extend_from_slice(&self.calls.load(Relaxed).to_le_bytes());
        b.extend_from_slice(&self.frames.load(Relaxed).to_le_bytes());
        b.extend_from_slice(&self.prepares.load(Relaxed).to_le_bytes());
        b.extend_from_slice(&self.rate.load(Relaxed).to_le_bytes());
        b.extend_from_slice(&self.max_block.load(Relaxed).to_le_bytes());
        b.extend_from_slice(&self.overflow.load(Relaxed).to_le_bytes());
        let nb = self.blocks_n.load(Acquire);
        let kept = (nb as usize).min(LOG_BLOCKS);
        b.extend_from_slice(&(kept as u32).to_le_bytes());
        for k in 0..kept {
            // Oldest first.
            let idx = (nb as usize - kept + k) % LOG_BLOCKS;
            b.extend_from_slice(&self.blocks[idx].load(Relaxed).to_le_bytes());
        }
        let ne = self.log_n.load(Acquire) as usize;
        b.extend_from_slice(&(ne as u32).to_le_bytes());
        for e in &self.log[..ne] {
            for w in e {
                b.extend_from_slice(&w.load(Relaxed).to_le_bytes());
            }
        }
        let nt = self.told_n.load(Acquire);
        b.extend_from_slice(&nt.to_le_bytes());
        for t in &self.told[..(nt as usize).min(LOG_TRANSPORTS)] {
            let head = t[0].load(Relaxed);
            b.extend_from_slice(&((head >> 32) as u32).to_le_bytes());
            b.extend_from_slice(&(head as u32).to_le_bytes());
            b.extend_from_slice(&(t[1].load(Relaxed) as u32).to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes());
            b.extend_from_slice(&t[2].load(Relaxed).to_le_bytes());
            b.extend_from_slice(&t[3].load(Relaxed).to_le_bytes());
        }
        b
    }

    /// Restores the persistent part of a state blob.
    ///
    /// # Errors
    ///
    /// [`BadState`] when the blob is shorter than the head, its magic is
    /// wrong, or a field is out of range. The plugin is left as it was.
    pub fn load(&self, bytes: &[u8]) -> Result<(), BadState> {
        let head: &[u8; STATE_HEAD] = bytes
            .get(..STATE_HEAD)
            .and_then(|h| h.try_into().ok())
            .ok_or(BadState)?;
        let word =
            |at: usize| u32::from_le_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]);
        let float = |at: usize| {
            let mut raw = [0; 8];
            raw.copy_from_slice(&head[at..at + 8]);
            f64::from_bits(u64::from_le_bytes(raw))
        };
        if word(0) != MAGIC {
            return Err(BadState);
        }
        let (mode, editor, notice, tail, rings, fault) = (
            word(4) & 0xF,
            (word(4) >> 8) & 0xFF,
            (word(4) >> 16) & 0xFF,
            (word(4) >> 24) & 0xF,
            word(4) >> 28,
            (word(4) >> 4) & 0xF,
        );
        let (level, aux) = (float(8), float(16));
        if mode >= MODES
            || editor >= EDITORS
            || notice >= NOTICES
            || tail >= TAILS
            || rings >= RINGS
            || fault >= FAULTS
            || !level.is_finite()
            || !aux.is_finite()
        {
            return Err(BadState);
        }
        self.mode.store(mode, Relaxed);
        self.editor.store(editor, Relaxed);
        self.notice.store(notice, Relaxed);
        self.tail.store(tail, Relaxed);
        self.rings.store(rings, Relaxed);
        self.fault.store(fault, Relaxed);
        self.gesture_due
            .store(u32::from(notice == NOTICE_GESTURE_ON_LOAD), Relaxed);
        self.set_param(LEVEL, level);
        self.set_param(AUX, aux);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OVERFLOW_AT: usize = 56;

    fn overflow(core: &Core) -> u32 {
        let blob = core.save();
        u32::from_le_bytes(blob[OVERFLOW_AT..OVERFLOW_AT + 4].try_into().unwrap())
    }

    fn block(core: &Core, dsp: &mut Dsp) {
        let (ins, mut out) = ([0.0f32; 4], [0.0f32; 4]);
        core.process(dsp, 4, 0, &[&ins], &mut [&mut out]);
    }

    fn ready() -> (Core, Dsp) {
        let (core, mut dsp) = (Core::new(), Dsp::default());
        core.prepare(&mut dsp, 48_000.0, 4);
        (core, dsp)
    }

    #[test]
    fn the_editor_byte_round_trips_beside_the_mode() {
        let core = Core::new();
        let mut blob = core.save();
        blob[4..8].copy_from_slice(&(3 | EDITOR_LEAKY << 8).to_le_bytes());
        core.load(&blob).unwrap();
        assert_eq!(core.editor(), EDITOR_LEAKY);
        let saved = core.save();
        assert_eq!(&saved[4..8], &(3 | EDITOR_LEAKY << 8).to_le_bytes());
        blob[4..8].copy_from_slice(&(EDITORS << 8).to_le_bytes());
        assert!(core.load(&blob).is_err());
    }

    #[test]
    fn the_notice_byte_round_trips_beside_the_editor() {
        let core = Core::new();
        let mut blob = core.save();
        blob[4..8].copy_from_slice(&(2 | EDITOR_TIDY << 8 | NOTICE_ON_LOAD << 16).to_le_bytes());
        core.load(&blob).unwrap();
        assert_eq!(core.notice(), NOTICE_ON_LOAD);
        assert_eq!(core.editor(), EDITOR_TIDY);
        assert_eq!(
            &core.save()[4..8],
            &(2 | EDITOR_TIDY << 8 | NOTICE_ON_LOAD << 16).to_le_bytes()
        );
        blob[4..8].copy_from_slice(&(NOTICES << 16).to_le_bytes());
        assert!(core.load(&blob).is_err());
    }

    #[test]
    fn a_log_within_its_caps_does_not_overflow() {
        let (core, mut dsp) = ready();
        for k in 0..LOG_EVENTS as u32 {
            core.push(&mut dsp, Ev::NoteOn { offset: k, key: 60 });
        }
        for _ in 0..LOG_BLOCKS {
            block(&core, &mut dsp);
        }
        assert_eq!(overflow(&core), 0);
    }

    #[test]
    fn a_full_event_log_sets_overflow() {
        let (core, mut dsp) = ready();
        for k in 0..=LOG_EVENTS as u32 {
            core.push(&mut dsp, Ev::NoteOn { offset: k, key: 60 });
        }
        assert_eq!(overflow(&core), 1);
    }

    #[test]
    fn more_blocks_than_the_ring_keeps_sets_overflow() {
        let (core, mut dsp) = ready();
        for _ in 0..=LOG_BLOCKS {
            block(&core, &mut dsp);
        }
        assert_eq!(overflow(&core), 1);
    }

    fn told(core: &Core) -> (u32, usize) {
        let t = crate::transcript::Transcript::parse(&core.save());
        (t.transports_sent, t.transports.len())
    }

    #[test]
    fn the_transport_log_keeps_the_first_and_counts_the_rest() {
        let (core, mut dsp) = ready();
        let t = Transport {
            offset: 0,
            flags: T_PLAYING,
            tempo: 120.0,
            beat: 0.0,
        };
        for _ in 0..=LOG_TRANSPORTS {
            core.log_transport(t);
        }
        assert_eq!(told(&core), (LOG_TRANSPORTS as u32 + 1, LOG_TRANSPORTS));
        assert_eq!(
            overflow(&core),
            0,
            "the transports never overflow the events"
        );
        core.prepare(&mut dsp, 48_000.0, 4);
        assert_eq!(told(&core), (0, 0));
    }

    #[test]
    fn preparing_clears_overflow() {
        let (core, mut dsp) = ready();
        for _ in 0..=LOG_BLOCKS {
            block(&core, &mut dsp);
        }
        core.prepare(&mut dsp, 48_000.0, 4);
        assert_eq!(overflow(&core), 0);
    }

    /// **The tail byte selects what the plugin reports, round-trips through the
    /// state, and is refused out of range**: the default 8191, the infinite
    /// marker, a finite tail past the graph's cap.
    #[test]
    fn the_tail_byte_selects_the_reported_tail() {
        let core = Core::new();
        assert_eq!(core.tail_samples(), TAIL);
        for (byte, want) in [
            (TAIL_INFINITE, u32::MAX),
            (TAIL_PAST_CAP, TAIL_PAST_CAP_FRAMES),
            (TAIL_DEFAULT, TAIL),
        ] {
            let mut blob = core.save();
            blob[4..8].copy_from_slice(&(byte << 24).to_le_bytes());
            core.load(&blob).expect("loads");
            assert_eq!(core.tail_samples(), want, "byte {byte}");
            assert_eq!(&core.save()[4..8], &(byte << 24).to_le_bytes());
        }
        let mut blob = core.save();
        blob[4..8].copy_from_slice(&(TAILS << 24).to_le_bytes());
        assert!(core.load(&blob).is_err(), "out of range");
    }

    /// Selects a ring behaviour (and keeps the tail nibble) through the state,
    /// as the contract does.
    fn with_rings(core: &Core, rings: u32, tail: u32) {
        let mut blob = core.save();
        blob[4..8].copy_from_slice(&((tail | rings << 4) << 24).to_le_bytes());
        core.load(&blob).expect("loads");
    }

    /// **The fault nibble shares the mode byte without disturbing the mode**, and
    /// out of range is refused. (A fault itself ends the process or hangs it: the
    /// probe tests run it in a separate process.)
    #[test]
    fn the_fault_nibble_round_trips_beside_the_mode() {
        let core = Core::new();
        let mut blob = core.save();
        blob[4..8].copy_from_slice(&(3 | FAULT_HANG << 4).to_le_bytes());
        core.load(&blob).expect("loads");
        assert_eq!(core.param(MODE), Some(0.75));
        assert_eq!(&core.save()[4..8], &(3 | FAULT_HANG << 4).to_le_bytes());
        blob[4..8].copy_from_slice(&(FAULTS << 4).to_le_bytes());
        assert!(core.load(&blob).is_err());
    }

    /// An impulse on both channels at frame 0, then silence, rendered in blocks
    /// of 512 for `frames`: channel 0's output, frame by frame.
    fn impulse_response(core: &Core, frames: usize) -> Vec<f32> {
        let mut dsp = Dsp::default();
        core.prepare(&mut dsp, 48_000.0, 512);
        let mut heard = Vec::new();
        for at in (0..frames).step_by(512) {
            let mut ins = [vec![0.0f32; 512], vec![0.0f32; 512]];
            if at == 0 {
                ins[0][0] = 0.1;
                ins[1][0] = 0.1;
            }
            let ins: Vec<&[f32]> = ins.iter().map(Vec::as_slice).collect();
            let mut outs = [vec![9.0f32; 512], vec![9.0f32; 512]];
            let mut refs: Vec<&mut [f32]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
            core.process(&mut dsp, 512, 0, &ins, &mut refs);
            heard.extend_from_slice(&outs[0]);
        }
        heard
    }

    /// **The ring is `RING_LEVEL` from the trigger's frame for exactly its
    /// length and then nothing**: no DC, no tag, whatever the mode says.
    #[test]
    fn an_exact_ring_lasts_its_length_after_the_impulse() {
        let core = Core::new();
        with_rings(&core, RINGS_EXACT, TAIL_DEFAULT);
        let heard = impulse_response(&core, 10_240);
        let last = RING_FRAMES as usize;
        assert!(heard[..=last].iter().all(|&v| v == RING_LEVEL));
        assert!(heard[last + 1..].iter().all(|&v| v == 0.0), "then silence");
    }

    /// **A longer ring and a late one differ from the exact one by exactly their
    /// figure**: the extra frames, the onset's delay.
    #[test]
    fn the_longer_and_the_late_ring_move_their_edge() {
        let core = Core::new();
        with_rings(&core, RINGS_LONGER, TAIL_DEFAULT);
        let heard = impulse_response(&core, 14_336);
        let last = (RING_FRAMES + RING_EXTRA) as usize;
        assert_eq!(heard[last], RING_LEVEL);
        assert_eq!(heard[last + 1], 0.0);
        with_rings(&core, RINGS_LATE, TAIL_DEFAULT);
        let heard = impulse_response(&core, 10_240);
        let onset = LATE_FRAMES as usize;
        assert!(heard[..onset].iter().all(|&v| v == 0.0));
        assert_eq!(heard[onset], RING_LEVEL);
        assert_eq!(heard[onset + RING_FRAMES as usize], RING_LEVEL);
        assert_eq!(heard[onset + RING_FRAMES as usize + 1], 0.0);
    }

    /// **The hiss is on with silence in, and stays at its level**: every frame
    /// carries noise, none above `HISS_LEVEL`, and the peak reaches near it.
    #[test]
    fn the_hiss_is_always_on_and_bounded() {
        let core = Core::new();
        with_rings(&core, RINGS_HISS, TAIL_DEFAULT);
        let heard = impulse_response(&core, 20_480);
        let tail = &heard[RING_FRAMES as usize + 1..];
        assert!(tail.iter().all(|v| v.abs() <= HISS_LEVEL));
        assert!(tail.iter().any(|v| v.abs() > 0.9 * HISS_LEVEL));
        assert!(tail.iter().filter(|&&v| v == 0.0).count() < 4);
    }

    /// **The ring byte shares the tail's byte without disturbing it**: each
    /// round-trips, and either out of range is refused.
    #[test]
    fn the_rings_nibble_round_trips_beside_the_tail() {
        let core = Core::new();
        for (rings, tail) in [(RINGS_LONGER, TAIL_INFINITE), (RINGS_LATE, TAIL_PAST_CAP)] {
            with_rings(&core, rings, tail);
            assert_eq!(core.rings(), rings);
            assert_eq!(
                core.tail_samples(),
                if tail == TAIL_INFINITE {
                    u32::MAX
                } else {
                    TAIL_PAST_CAP_FRAMES
                }
            );
            assert_eq!(
                &core.save()[4..8],
                &((tail | rings << 4) << 24).to_le_bytes()
            );
        }
        let mut blob = core.save();
        blob[4..8].copy_from_slice(&(RINGS << 28).to_le_bytes());
        assert!(core.load(&blob).is_err());
    }
}
