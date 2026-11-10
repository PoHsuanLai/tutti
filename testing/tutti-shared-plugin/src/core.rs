//! The oracle: what the plugin does, independent of any plugin format.
//!
//! Every format shell decodes its host's calls into the calls below and
//! encodes the answers back. Nothing in this file names a format SDK.
//!
//! # Spec
//!
//! Parameters, ids the same in every format: `LEVEL` 0 (default 0.5), `AUX` 1
//! (default 0.25), `MODE` 2 (five steps, value `k / 4`, default 0).
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
//! Tail 8191 in every mode. A parameter event moves `level` or `aux` at its
//! sample offset.
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
//! off, 4 control change), the parameter id, key or controller number (u64), the
//! value (f64; a control change's value, 0 to 127). A control change is
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
/// Number of parameters.
pub const PARAMS: u32 = 3;
/// Parameter names, by id.
pub const NAMES: [&str; PARAMS as usize] = ["level", "aux", "mode"];
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
/// Latency of the Delay mode, in frames.
pub const LATENCY: u32 = 137;
/// Reported tail, in frames.
pub const TAIL: u32 = 8191;
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
}

impl Ev {
    fn offset(&self) -> u32 {
        match *self {
            Ev::Param { offset, .. }
            | Ev::NoteOn { offset, .. }
            | Ev::NoteOff { offset, .. }
            | Ev::Control { offset, .. } => offset,
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
    mode: AtomicU32,
    editor: AtomicU32,
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
            mode: z(),
            editor: z(),
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

    /// A parameter's normalized value, `None` for an id the plugin lacks.
    pub fn param(&self, id: u32) -> Option<f64> {
        match id {
            LEVEL => Some(f64::from_bits(self.level.load(Relaxed))),
            AUX => Some(f64::from_bits(self.aux.load(Relaxed))),
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
        }
        if dsp.events.len() < MAX_EVENTS {
            dsp.events.push(ev);
        } else {
            self.overflow.store(1, Relaxed);
        }
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
                Ev::Control { .. } => {}
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

    /// Silences the audio side's memory: delay line and held note.
    pub fn reset(&self, dsp: &mut Dsp) {
        dsp.held = None;
        for l in &mut dsp.delay {
            l.iter_mut().for_each(|v| *v = 0.0);
        }
    }

    /// Serializes the state: the head [`Core::load`] reads, then the
    /// transcript.
    pub fn save(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(512);
        b.extend_from_slice(&MAGIC.to_le_bytes());
        let word = self.mode.load(Relaxed) | self.editor.load(Relaxed) << 8;
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
        let (mode, editor) = (word(4) & 0xFF, word(4) >> 8);
        let (level, aux) = (float(8), float(16));
        if mode >= MODES || editor >= EDITORS || !level.is_finite() || !aux.is_finite() {
            return Err(BadState);
        }
        self.mode.store(mode, Relaxed);
        self.editor.store(editor, Relaxed);
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

    // Mutation (run): `save` not writing the editor byte, or `load` not
    // reading it, fails the round trip; `load` masking nothing refuses a blob
    // with an editor byte as a bad mode.
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

    // Mutation (run): not setting the flag when the log is full fails
    // `a_full_event_log_sets_overflow`; not setting it past the block ring
    // fails `more_blocks_than_the_ring_keeps_sets_overflow`; `prepare` not
    // clearing it fails `preparing_clears_overflow`.
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

    // Mutation (run): `log_transport` counting only what it keeps fails the
    // first assertion; `prepare` not clearing the count fails the last.
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
}
