//! Grouped configuration and scratch state for the CLAP instance types.
//!
//! [`AudioConfig`] / [`PortLayout`] / [`LifecycleFlags`] live on `ClapLoaded`;
//! [`AudioScratch`] holds all per-block RT scratch and lives only on the
//! `ClapProcessor` (it exists solely for `process`).

use crate::events::{InputEventList, OutputEventList, ParamBounds};
use crate::types::{ClapNoteExpression, ParameterChanges, RtNoteExpressions};
use clap_sys::process::CLAP_PROCESS_CONTINUE;
use std::sync::atomic::AtomicI32;
use tutti_clap_ffi::PortScratch;
use tutti_plugin_types::BusChannels;
use tutti_plugin_types::RtMidiEvents;
use tutti_plugin_types::SampleFormat;

/// How many converted note expressions one block can carry; reserved at
/// activation so converting never grows the buffer.
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

/// All per-block RT scratch for the committed sample format, grouped so it
/// can live on the Processor and drop before the plugin handle.
///
/// Sized once in `ClapLoaded::activate_now`; every `process` call reuses these
/// in place and never allocates. The channel tables are one `PortScratch`, made for the activation's
/// format; the rest of the scratch has no width.
pub(crate) struct AudioScratch {
    /// Channel pool + per-port `clap_audio_buffer` descriptors.
    pub process: PortScratch,
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

impl AudioScratch {
    pub fn new(format: SampleFormat) -> Self {
        Self {
            process: PortScratch::new(format),
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
/// The Controller publishes it through an [`ArcLend`](tutti_rt::ArcLend) at
/// activation and again after a rescan that changed the parameters; the
/// Processor lends it once per block.
#[derive(Debug, Default)]
pub(crate) struct RangeTable {
    /// Each parameter's bounds and steppedness, sorted by id.
    pub ranges: Vec<ParamBounds>,
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
/// One [`ChannelLayout`](tutti_plugin_types::ChannelLayout) per bus, not a raw count: the layout survives from
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
    /// Stays `usize`, not a [`ChannelLayout`](tutti_plugin_types::ChannelLayout): a sum across buses is a size for
    /// the flat channel pool, not the layout of any one bus.
    pub fn input_channel_total(&self) -> usize {
        self.inputs.iter().map(|c| c.count() as usize).sum()
    }

    /// Channel count summed across every output port. `usize` for the same
    /// reason as [`input_channel_total`](Self::input_channel_total).
    pub fn output_channel_total(&self) -> usize {
        self.outputs.iter().map(|c| c.count() as usize).sum()
    }
}

#[cfg(test)]
mod port_layout_tests {
    use super::*;
    use tutti_plugin_types::ChannelLayout;

    /// A plugin that reports a real mono bus keeps width 1 all the way to the
    /// totals. A `.max(2)` floor on the totals would report 2 here, sizing a
    /// stereo slab for a mono plugin.
    #[test]
    fn a_mono_bus_is_not_widened_to_stereo() {
        let ports = PortLayout {
            inputs: vec![ChannelLayout::MONO],
            outputs: vec![ChannelLayout::MONO],
        };

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

    /// Multi-bus layouts sum rather than being floored or truncated, and an
    /// effect with no input bus but real outputs keeps 0 inputs — the asymmetry
    /// a blanket `.max(2)` on both directions erased.
    #[test]
    fn totals_sum_across_buses_and_an_absent_side_is_zero() {
        let ports = PortLayout {
            inputs: BusChannels::new(),
            outputs: vec![ChannelLayout::STEREO, ChannelLayout::from(6u16)],
        };

        assert_eq!(ports.output_channel_total(), 8, "2 + 6, not floored");
        assert_eq!(ports.input_channel_total(), 0, "no input port, no input");
    }
}

/// Lifecycle sub-state a `ClapLoaded` keeps for the methods that cannot see
/// which half of an activation they were reached through.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LifecycleFlags {
    /// The plugin's `activate()` has run and `deactivate()` has not.
    ///
    /// Distinct from processing (`AudioSide::is_processing`), and the distinction is
    /// load-bearing: CLAP tags methods like `params.flush` as
    /// `[active ? audio-thread : main-thread]`, keyed on *activation* and not on
    /// whether audio has started flowing. Gating those on processing treats the
    /// window between `activate()` and the first `process()` as inactive, and a
    /// plugin that knows it is active reports the host for calling on the wrong
    /// thread.
    pub active: bool,
    pub gui_created: bool,
}
