//! Grouped configuration and scratch state for the CLAP instance types.
//!
//! [`AudioConfig`] / [`PortLayout`] / [`LifecycleFlags`] live on `ClapLoaded`;
//! [`AudioScratch`] holds all per-block RT scratch and lives only on the
//! `ClapProcessor<T>` (it exists solely for `process`).

use crate::events::{InputEventList, OutputEventList};
use crate::types::{ClapNoteExpression, ParameterChanges, RtNoteExpressions};
use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::process::CLAP_PROCESS_CONTINUE;
use std::sync::atomic::AtomicI32;
use tutti_plugin_types::BusChannels;
use tutti_plugin_types::ChannelLayout;
use tutti_plugin_types::RtMidiEvents;

/// Extra caller-channel-pointer slots reserved beyond the plugin's port layout,
/// so a caller supplying more channels than the plugin consumes does not grow
/// the pool on the audio thread. See [`ProcessScratch::caller_input_ptrs`].
///
/// 8 covers the realistic over-supply (a 7.1 host feeding a stereo plugin) for
/// 64 bytes a side; past it, one heap grow on the first such block and none
/// after.
const CALLER_PTR_HEADROOM: usize = 8;

/// Note expressions one block can carry in to the plugin.
pub(crate) const INPUT_NOTE_EXPRESSIONS: usize = 256;

/// Audio format the host presents to the plugin.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AudioConfig {
    pub sample_rate: f64,
    pub max_frames: u32,
    pub supports_f64: bool,
    /// Whether the current activation hands the plugin 64-bit buffers.
    pub f64_active: bool,
}

/// Pre-allocated scratch for one sample type (f32 or f64) used by the RT
/// process path. Sized once in `activate()` from the plugin's port layout;
/// [`ClapProcessor::process`](super::ClapProcessor::process) reuses these
/// vectors in place — it never grows or reallocates them on the audio
/// thread.
///
/// Fields are `pub(crate)` because this struct is an internal buffer pool;
/// users interact with it only through `ClapProcessor::process`.
pub struct ProcessScratch<T> {
    pub(crate) channels: Vec<Vec<T>>,
    pub(crate) input_ptrs: Vec<*mut T>,
    pub(crate) output_ptrs: Vec<*mut T>,
    pub(crate) input_bufs: Vec<clap_audio_buffer>,
    pub(crate) output_bufs: Vec<clap_audio_buffer>,
    /// The *caller's* channel pointers for this block, one entry per channel
    /// the caller supplied — distinct from `input_ptrs`/`output_ptrs`, which
    /// are the plugin-facing arrays after zero-padding to the port layout.
    ///
    /// These were small-vector locals in `process` (16 pointers inline, then
    /// the heap), which spilled to
    /// the heap every block past 16 channels a side — reachable through
    /// supported layouts, since two 8-channel ports or a third-order ambisonic
    /// bus already sit at the inline bound. Pooled here instead, sized from the
    /// port layout in `resize_for`.
    pub(crate) caller_input_ptrs: Vec<*mut T>,
    pub(crate) caller_output_ptrs: Vec<*mut T>,
}

// The raw pointers live inside our own `channels` vec; the whole struct is
// fine to move across threads together with `ClapProcessor`. Pointers are re-derived on each `process` call.
unsafe impl<T: Send> Send for ProcessScratch<T> {}

impl<T: Copy + Default> ProcessScratch<T> {
    pub fn new() -> Self {
        Self {
            channels: Vec::new(),
            input_ptrs: Vec::new(),
            output_ptrs: Vec::new(),
            input_bufs: Vec::new(),
            output_bufs: Vec::new(),
            caller_input_ptrs: Vec::new(),
            caller_output_ptrs: Vec::new(),
        }
    }

    /// Pre-allocate every vector. Called from `activate()` after the port
    /// layout and max-frames are known, so the RT path never grows these.
    ///
    /// Channel pool layout: the first `input_channels_total` entries serve
    /// the input side; the next `output_channels_total` serve the output
    /// side. This split keeps input and output pads independent.
    pub fn resize_for(
        &mut self,
        input_channels_total: usize,
        output_channels_total: usize,
        max_frames: usize,
        num_input_ports: usize,
        num_output_ports: usize,
    ) {
        let total_channel_bufs = input_channels_total + output_channels_total;
        self.channels.clear();
        self.channels.reserve_exact(total_channel_bufs);
        for _ in 0..total_channel_bufs {
            self.channels.push(vec![T::default(); max_frames]);
        }

        self.input_ptrs.clear();
        self.input_ptrs.reserve_exact(input_channels_total);
        self.output_ptrs.clear();
        self.output_ptrs.reserve_exact(output_channels_total);

        self.input_bufs.clear();
        self.input_bufs.reserve_exact(num_input_ports);
        self.output_bufs.clear();
        self.output_bufs.reserve_exact(num_output_ports);

        // Headroom above the port layout: `refill_port_buffers` clamps with
        // `.min(wanted)`, but the caller's extra pointers are collected before
        // the clamp discards them, so an over-supplying caller would otherwise
        // be exactly the case that still grows the pool on the audio thread.
        // Additive, not a multiplier — over-supply is a handful of channels,
        // not a proportion of the layout.
        self.caller_input_ptrs.clear();
        self.caller_input_ptrs
            .reserve_exact(input_channels_total + CALLER_PTR_HEADROOM);
        self.caller_output_ptrs.clear();
        self.caller_output_ptrs
            .reserve_exact(output_channels_total + CALLER_PTR_HEADROOM);
    }
}

/// All per-block RT scratch for the committed sample type `T`, grouped so it
/// can live on the Processor and drop before the plugin handle.
///
/// Sized once in `ClapLoaded::activate_typed`; every `process` call reuses these
/// in place and never allocates. Because `ClapProcessor<T>` is monomorphised by `T`,
/// there is exactly one `ProcessScratch<T>` here — the pre-split single type
/// had to carry both an f32 and an f64 pool.
pub(crate) struct AudioScratch<T: super::ClapSample> {
    /// Channel pool + per-port `clap_audio_buffer` descriptors.
    pub process: ProcessScratch<T>,
    /// Refilled per call (UMP → CLAP). Cleared in place; capacity persists.
    pub input_events: InputEventList,
    /// Filled by the plugin's `try_push` callback during `process`.
    pub output_events: OutputEventList,
    /// Return pools: `process` drains the plugin's emitted events into these so
    /// the call can hand back borrowed slices.
    ///
    /// Capped rather than growable. The plugin drives how many events land
    /// here — `output_events_try_push` is its callback — so an unbounded pool
    /// would let a plugin provoke a `malloc` inside the audio callback. Events
    /// past the cap are dropped; `overflowed()` reports it.
    pub out_midi: RtMidiEvents,
    /// Reserved at activation, one queue per parameter with a block's point
    /// room, and filled within that room (`fill_param_changes`); points past
    /// it are counted in `out_param_points_dropped`.
    pub out_param_changes: ParameterChanges,
    /// Output parameter points `fill_param_changes` could not keep whole
    /// since activation: see
    /// [`ClapProcessor::output_param_points_dropped`](super::ClapProcessor::output_param_points_dropped).
    pub out_param_points_dropped: u64,
    pub out_note_expressions: RtNoteExpressions,
    /// The block's note expressions in CLAP's voice-addressed form, converted
    /// from the shared vocabulary in place. Reserved at activation to
    /// [`INPUT_NOTE_EXPRESSIONS`]; past it they are dropped and counted in
    /// `in_note_expressions_dropped`, so the audio thread never grows it.
    pub in_note_expressions: Vec<ClapNoteExpression>,
    /// Note expressions dropped for want of room in `in_note_expressions`.
    pub in_note_expressions_dropped: u64,
    /// Monotonic sample counter fed to CLAP's `steady_time`. Init 0 at
    /// activate; advances by `frames_count` each processed block; reset to 0
    /// on stop_processing/reactivate. Never derived from transport seconds.
    pub steady_time: i64,
    /// The `clap_process_status` the plugin returned on the most recent block.
    ///
    /// Atomic so the audio thread's `Relaxed` store is readable off-thread
    /// through
    /// [`ClapProcessor::last_process_status`](super::ClapProcessor::last_process_status);
    /// as a plain field, TAIL and SLEEP reached callers only via an `eprintln!`
    /// on the audio thread. A plain `AtomicI32` rather than `ArcLend` because
    /// this is a scalar — nothing here can be freed on the audio thread.
    pub last_process_status: AtomicI32,
}

impl<T: super::ClapSample> AudioScratch<T> {
    pub fn new() -> Self {
        Self {
            process: ProcessScratch::new(),
            input_events: InputEventList::new(),
            output_events: OutputEventList::new(),
            out_midi: RtMidiEvents::new(),
            out_param_changes: ParameterChanges::new(),
            out_param_points_dropped: 0,
            out_note_expressions: RtNoteExpressions::new(),
            in_note_expressions: Vec::new(),
            in_note_expressions_dropped: 0,
            steady_time: 0,
            last_process_status: AtomicI32::new(CLAP_PROCESS_CONTINUE),
        }
    }
}

/// Each parameter's native range, which the Processor denormalizes automation
/// and edits against.
///
/// CLAP parameter events carry the plugin's **plain** value (CLAP has no
/// normalization), but host-side automation authors values normalized `0..1`,
/// so incoming points are denormalized `min + v·(max-min)` against this table.
/// The Controller publishes it through an [`ArcLend`](tutti_types::ArcLend) at
/// activation and again after a rescan that changed the parameters; the
/// Processor lends it once per block.
#[derive(Debug, Default)]
pub(crate) struct RangeTable {
    /// `(param_id, min, max)`, sorted by id.
    pub ranges: Vec<(u32, f32, f32)>,
    /// Whether the plugin reported a nonzero `params.count()`.
    ///
    /// An empty `ranges` is ambiguous on its own and the two cases need
    /// opposite handling: no `params` extension means automation must pass
    /// through unchanged, while a failing `get_info(0)` empties the table by
    /// truncation, where pass-through would hand raw `0..1` to a param
    /// expecting a real range. Recording the claim separately lets
    /// [`add_param_changes`](crate::events::InputEventList::add_param_changes)
    /// tell those apart.
    pub claims: bool,
}

/// Per-port channel layouts for audio IO.
/// E.g. `inputs = [Stereo]` for one stereo port, `[Stereo, Stereo]` for two.
///
/// One [`ChannelLayout`] per bus, not a raw count: the layout survives from
/// `layout_from_clap_port` (which reads CLAP's `port_type` tag, so a mono or
/// stereo port is *named* rather than inferred from its width) all the way to
/// the process scratch. The raw `u32` reappears only where the CLAP C ABI
/// demands it — `make_port_buffer`'s `channel_count` field.
#[derive(Debug, Clone, Default)]
pub(crate) struct PortLayout {
    pub inputs: BusChannels,
    pub outputs: BusChannels,
}

impl PortLayout {
    /// Channel count summed across every input port.
    ///
    /// Stays `usize`, not a [`ChannelLayout`]: a sum across buses is a size for
    /// the flat channel pool, not the layout of any one bus.
    pub fn input_channel_total(&self) -> usize {
        self.inputs.iter().map(|c| c.count() as usize).sum()
    }

    /// Channel count summed across every output port. `usize` for the same
    /// reason as [`input_channel_total`](Self::input_channel_total).
    pub fn output_channel_total(&self) -> usize {
        self.outputs.iter().map(|c| c.count() as usize).sum()
    }

    /// Give each empty direction one stereo bus, so "unknown layout" is stated as
    /// a bus rather than left for later code to guess at.
    ///
    /// A plugin with no `audio-ports` extension reports no buses at all. Stereo
    /// because it is what the overwhelming majority of plugins present — the same
    /// rule, and the same reason, as the subprocess host's `default_if_empty`.
    ///
    /// For the layout the host **reports** (`PluginInfo`'s channel totals,
    /// which the plugin server turns into the node's graph-facing buses) only,
    /// never for the one handed to the plugin: `process` must carry exactly
    /// `clap_plugin_audio_ports::count()` buffers on each side, so
    /// `ClapLoaded::ports` keeps the declared layout, empty sides included,
    /// and the load applies this to a copy.
    ///
    /// Call this **before** reading the reported totals. Putting a `.max(2)`
    /// floor on the totals instead makes a genuine mono plugin report
    /// 2-in/2-out while `self` still holds the true `[Mono]` — two sources of
    /// truth about one width, disagreeing.
    pub fn default_empty_buses(&mut self) {
        if self.inputs.is_empty() {
            self.inputs.push(ChannelLayout::STEREO);
        }
        if self.outputs.is_empty() {
            self.outputs.push(ChannelLayout::STEREO);
        }
    }
}

#[cfg(test)]
mod port_layout_tests {
    use super::*;

    /// A plugin that reports a real mono bus keeps width 1 all the way to the
    /// totals. A `.max(2)` floor on the totals would report 2 here, sizing a
    /// stereo slab for a mono plugin.
    #[test]
    fn a_mono_bus_is_not_widened_to_stereo() {
        let mut ports = PortLayout {
            inputs: vec![ChannelLayout::MONO],
            outputs: vec![ChannelLayout::MONO],
        };
        ports.default_empty_buses();

        assert_eq!(
            ports.input_channel_total(),
            1,
            "mono input must stay 1-wide"
        );
        assert_eq!(
            ports.output_channel_total(),
            1,
            "mono output must stay 1-wide"
        );
    }

    /// The no-`audio-ports` case: the default is applied to the bus list, so the
    /// total *derives* from it and the two agree by construction.
    #[test]
    fn an_absent_bus_list_defaults_to_one_stereo_bus() {
        let mut ports = PortLayout::default();
        ports.default_empty_buses();

        assert_eq!(ports.inputs.as_slice(), &[ChannelLayout::STEREO]);
        assert_eq!(ports.outputs.as_slice(), &[ChannelLayout::STEREO]);
        assert_eq!(ports.input_channel_total(), 2);
        assert_eq!(ports.output_channel_total(), 2);
    }

    /// Multi-bus layouts sum rather than being floored or truncated, and an
    /// effect with no input bus but real outputs keeps 0 inputs — the asymmetry
    /// a blanket `.max(2)` on both directions erased.
    #[test]
    fn totals_sum_across_buses_and_survive_defaulting() {
        let mut ports = PortLayout {
            inputs: BusChannels::new(),
            outputs: vec![ChannelLayout::STEREO, ChannelLayout::from(6u16)],
        };
        ports.default_empty_buses();

        // Only the empty direction was defaulted.
        assert_eq!(ports.output_channel_total(), 8, "2 + 6, not floored");
        assert_eq!(
            ports.inputs.as_slice(),
            &[ChannelLayout::STEREO],
            "an absent input bus is stated as stereo, not left empty"
        );
    }
}

/// Lifecycle sub-state a `ClapLoaded` keeps for the methods that cannot see
/// which half of an activation they were reached through.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LifecycleFlags {
    /// The plugin's `activate()` has run and `deactivate()` has not.
    ///
    /// Distinct from processing (`Shell::processing`), and the distinction is
    /// load-bearing: CLAP tags methods like `params.flush` as
    /// `[active ? audio-thread : main-thread]`, keyed on *activation* and not on
    /// whether audio has started flowing. Gating those on processing treats the
    /// window between `activate()` and the first `process()` as inactive, and a
    /// plugin that knows it is active reports the host for calling on the wrong
    /// thread.
    pub active: bool,
    pub gui_created: bool,
}
