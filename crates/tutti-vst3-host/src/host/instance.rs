//! The activation sequence: [`Staged<T>`], a [`Vst3Loaded`] with the scratch
//! its Processor will render through, taken from `setBusArrangements` to
//! `setActive(1)`.
//!
//! [`Vst3Loaded::activate_typed`] builds one, and on success splits it: the
//! plugin goes to the shell its two halves share, the scratch to the
//! [`Vst3Processor`](super::Vst3Processor). On failure the plugin comes back
//! out of it, inactive, so a refusal costs the configuration and not the
//! instance.
//!
//! What a staged plugin has over a loaded one is exactly the state that exists
//! between `setActive(1)` and `setActive(0)`: the scratch buffers sized to the
//! negotiated arrangements, the input and output staging, and the sample width
//! `T`.

use tutti_plugin_types::MainThread;
use vst3::Steinberg::Vst::{SpeakerArr, SpeakerArrangement};

use tutti_plugin_types::{PointPool, RtMidiEvents};
use tutti_types::{ChannelLayout, ChannelTopology};

use crate::error::{LoadStage, Result, Vst3Error};
use crate::events::Stager;
use crate::types::{ParameterChanges, ProcessMode, Vst3Sample};
use tutti_vst3_ffi::{
    BusBuffers, BusDirection, EventList, MIN_POINT_ROOM, MediaType, ParameterChangesImpl,
};

use super::loaded::{Activation, Vst3Loaded};
use super::midi_mapping::CcRoute;
use super::processor::{AudioIO, InputStaging, OutputStaging, ProcessConfig};

/// Floor of the pre-reserved capacity for param-change queues. One slot per
/// distinct param_id in a single block; the reservation is the plugin's
/// parameter count when that is larger. Growing past it allocates once and
/// then sticks.
const OUTPUT_PARAM_QUEUE_RESERVE: usize = 32;

/// The arrangement to propose for a bus of `layout` channels, when the host has
/// no topology for it.
///
/// Named layouts, not a synthesized mask. `(1u64 << n) - 1` — the low `n`
/// bits — has the right *popcount* and the wrong *speakers*: at width 4 it asks
/// for `L R C Lfe` where a four-channel bus means a surround pair, and at width
/// 8 it asks for a front-of-centre pair rather than the extra surround pair.
/// Only 5.1 comes out right, and only because bits 0–5 happen to be contiguous
/// and in the engine's own order.
///
/// `None` for a width with no canonical arrangement, which is a real answer:
/// `setBusArrangements` is a *proposal*, so proposing nothing and reading back
/// what the plugin chose beats proposing a layout that names the wrong
/// speakers.
///
/// [`to_arrangement`](super::speakers::to_arrangement) is the conversion for a caller with a real
/// [`ChannelTopology`]; this is the width-only fallback for the path that
/// enumerates counts.
fn default_arrangement_for(layout: ChannelLayout) -> Option<SpeakerArrangement> {
    Some(match layout.count() {
        0 => SpeakerArr::kEmpty,
        1 => SpeakerArr::kMono,
        2 => SpeakerArr::kStereo,
        // `k40Music` (`L R Ls Rs`), not `k40Cine` (`L R C Cs`): a four-channel
        // bus in this engine is a front pair plus a surround pair.
        4 => SpeakerArr::k40Music,
        6 => SpeakerArr::k51,
        // `k71Music`, whose extra pair sits beyond the 5.1 core — the same
        // shape as the engine's own 7.1 order. `k71CineFullRear` is a different
        // eight-channel layout.
        8 => SpeakerArr::k71Music,
        _ => return None,
    })
}

/// A read-back topology, or `None` when there is nothing usable to say about
/// this bus.
///
/// Split out of `bus_topologies` so the decision is testable: that method needs
/// a live COM plugin, so the rule inside it could otherwise only be exercised
/// by loading one — and a rule no test can reach is a rule that quietly stops
/// being true.
///
/// Three ways to have nothing to say, all of them `None`:
///
/// - **Not fully named** — the arrangement uses a speaker this vocabulary does
///   not have, so the bus cannot be routed by speaker even though its width is
///   known.
/// - **Width disagrees** with the count reported beside it. The count sizes the
///   buffers, so a topology describing a different number of channels describes
///   a different bus.
/// - **Zero width.** A failed `getBusArrangement` yields an empty topology,
///   which is vacuously "fully named" and would otherwise be reported as a real
///   bus that happens to have no channels.
pub(super) fn usable_topology(topology: ChannelTopology, width: usize) -> Option<ChannelTopology> {
    let usable =
        topology.is_fully_named() && topology.layout().count() as usize == width && width > 0;
    usable.then_some(topology)
}

/// A bus width the plugin reported (already bounded by the ffi layer's
/// `audio_bus_channels` to what a layout holds) as a layout.
pub(super) fn bus_layout(width: usize) -> ChannelLayout {
    ChannelLayout::try_from(width).expect("the ffi layer bounds a bus width to a layout")
}

/// A plugin and the scratch its Processor renders through, between
/// `setBusArrangements` and the split.
pub(super) struct Staged<T: Vst3Sample> {
    /// The sample width this activation was staged for.
    _format: std::marker::PhantomData<fn() -> T>,
    pub(super) loaded: Vst3Loaded,
    pub(super) audio: AudioIO,
}

impl<T: Vst3Sample> Staged<T> {
    /// Runs `setBusArrangements`, `setupProcessing`, `activateBus` and
    /// `setActive(1)` (which also tells the plugin processing is about to
    /// start), and sizes the scratch to what the plugin settled on.
    ///
    /// # Errors
    ///
    /// The plugin comes back with the reason, inactive: the f64 width the
    /// plugin does not process, a refused `setupProcessing` or a refused
    /// `setActive(1)`.
    // The `Err` variant hands the whole plugin back, which is the point of it;
    // boxing it would add an allocation to a path that must return the plugin.
    #[allow(clippy::result_large_err)]
    pub(super) fn stage(
        main: &MainThread,
        loaded: Vst3Loaded,
        sample_rate: f64,
        block_size: usize,
        mode: ProcessMode,
    ) -> std::result::Result<Self, (Vst3Loaded, Vst3Error)> {
        if T::VST3_SYMBOLIC_SIZE == crate::types::K_SAMPLE_64_INT && !loaded.info.supports_f64 {
            return Err((
                loaded,
                Vst3Error::NotSupported("Plugin does not support 64-bit processing".to_string()),
            ));
        }

        let num_input_channels = bus_layout(loaded.info.num_inputs);
        let num_output_channels = bus_layout(loaded.info.num_outputs);
        // Resolve the per-direction scratch from the initial bus layout in the
        // PluginInfo snapshot. `activate_buses` re-resolves the same way from
        // the live component afterward, since some plugins only finalise their
        // arrangement once active.
        let in_scratch = BusBuffers::new(
            T::WIDTH,
            &loaded.info.input_bus_channels,
            num_input_channels,
            block_size,
        );
        let out_scratch = BusBuffers::new(
            T::WIDTH,
            &loaded.info.output_bus_channels,
            num_output_channels,
            block_size,
        );

        // One queue per parameter on every param-change list, built here off
        // the audio thread: the host sends changes only, so one block can
        // carry a change for every parameter at once, and the count varies
        // from block to block. Nothing grows past them on the audio thread:
        // a queue past a list's is dropped, counted.
        // Capped so a controller reporting a negative or absurd count (read
        // back through `as u32`) cannot reserve gigabytes.
        const MAX_PRESIZED_QUEUES: u32 = 1 << 14;
        let param_queues = (loaded.parameter_count().min(MAX_PRESIZED_QUEUES) as usize)
            .max(OUTPUT_PARAM_QUEUE_RESERVE);
        // The host's chunk adds its spare queues to the parameter count.
        let chunk_queues = param_queues + ParameterChanges::SPARE_QUEUES;
        // The plugin's emitted changes are drained into reserved queues, each
        // with the room the output COM queues have, so a block whose
        // emitted points fit it never allocates a queue or its points.
        let emitted_param_changes = ParameterChanges::reserved(param_queues, MIN_POINT_ROOM);
        // Each input queue has room for the points one parameter can carry
        // in a block of `block_size` (what the host's plugin node reserves),
        // and one more a skipped block's carried value can prepend, stored
        // as the host's chunk stores them (`ParameterChanges::point_layout`):
        // a floor of its own and a block from the list's shared pool when it
        // carries more, so the refill never grows a queue on the audio
        // thread and the list's memory follows the points a chunk carries,
        // not the parameter count. The `IMidiMapping` route copies the same
        // chunk into a set laid out the same way (`ParameterChanges::reserved`).
        let chunk_room = tutti_plugin_types::ParameterQueue::chunk_room(block_size);
        let (floor, blocks) = ParameterChanges::point_layout(chunk_queues, chunk_room);
        let input_pool = PointPool::new(blocks, chunk_room + 1, chunk_queues);

        let audio = AudioIO {
            config: ProcessConfig {
                sample_rate,
                block_size,
                num_input_channels,
                num_output_channels,
                // Both start at the requested mode; `apply_process_setup` is
                // what actually delivers `setup_mode` to the plugin, and
                // `set_prefetch` is the only thing that may move `block_mode`
                // away from it.
                setup_mode: mode,
                block_mode: mode,
            },
            input: InputStaging {
                buses: in_scratch,
                events: EventList::input(),
                stager: Stager::default(),
                param_changes: ParameterChangesImpl::with_queues(chunk_queues, floor, input_pool),
            },
            output: OutputStaging {
                buses: out_scratch,
                events: EventList::output(),
                // The plugin fills this one through `addPoint`, which cannot
                // borrow a block: every queue has `MIN_POINT_ROOM`, no pool.
                param_changes: ParameterChangesImpl::with_queues(
                    param_queues,
                    MIN_POINT_ROOM,
                    PointPool::default(),
                ),
                emitted_midi: RtMidiEvents::new(),
                emitted_param_changes,
            },
            cc: CcRoute::following(
                loaded.midi_cc.reader(),
                // Only a plugin with an `IMidiMapping` can publish a mapping
                // later, so only it reserves the routing scratch.
                loaded.plugin.has_midi_mapping(),
                chunk_queues,
                chunk_room,
            ),
            midi_learn: loaded.midi_learn.producer(),
        };

        let mut instance = Self {
            _format: std::marker::PhantomData,
            loaded,
            audio,
        };
        // VST3 activation order: setBusArrangements → setupProcessing →
        // activateBus → setActive. Arrangements must be negotiated first so the
        // plugin has decided its channel layout before the scratch is sized and
        // processing is set up.
        let activated = instance
            .negotiate_bus_arrangements()
            .and_then(|()| instance.apply_process_setup())
            .and_then(|()| instance.activate_buses())
            .and_then(|()| instance.set_active(true));
        if let Err(error) = activated {
            return Err((instance.loaded, error));
        }
        instance.loaded.active = Activation {
            format: Some(T::FORMAT),
        };
        // The spec requires reading latency after each `setActive(true)`: the
        // value is only valid once the plugin is active, and it is what a host
        // feeds into delay compensation. Skipping it means a latency-reporting
        // plugin runs uncompensated — audibly out of time against every other
        // track. The read is also what tells a plugin the host implements PDC
        // at all (HostChecker flags the omission as "Missing Call:
        // getLatencySamples ()").
        //
        // The figure is not stored here: the core reads it from the Controller
        // after every activation. What matters at this layer is that the call
        // happens, in the right place, every activation.
        let _ = instance.loaded.latency_samples(main);
        Ok(instance)
    }

    /// Negotiates per-bus speaker arrangements with the plugin via
    /// `IAudioProcessor::setBusArrangements`, before `setupProcessing`.
    ///
    /// One arrangement per bus is proposed, derived from the channel counts the
    /// component already enumerated (1 → mono, 2 → stereo, N → an N-bit low
    /// mask). Multichannel / surround / sidechain plugins need this: without it
    /// they fall back to a default layout that may not match the buses the host
    /// wired.
    ///
    /// A `kResultFalse` return means the plugin **kept its own layout** rather
    /// than accepting the host's — not an error. In that case the host reads
    /// back the plugin's chosen arrangement per bus with `getBusArrangement`,
    /// re-derives the channel counts, and re-resolves the audio scratch so
    /// `process` stages the right number of channels. (`activate_buses` re-resolves again from
    /// the live component after activation, covering plugins that only finalise
    /// their layout once active.)
    ///
    /// **Either way this ends by reconciling `PluginInfo`.** The scratch and
    /// the reported layout are two separate copies of the same fact, and only
    /// the scratch was being updated: `resolve_scratch_from_counts` fixes what
    /// this instance renders through, while `PluginInfo` is what every caller
    /// above reads — `PluginClient::new` sizes its graph node from it. A
    /// plugin that refused therefore had its *proposed* width reported while it
    /// ran another. Reconciling on the accepting branch too is not belt-and-
    /// braces: a plugin may accept the arrangement and still restructure its
    /// buses in the same call, and one exit path is one thing to keep true.
    fn negotiate_bus_arrangements(&mut self) -> Result<()> {
        let plugin = &self.loaded.plugin;

        // A bus whose width has no canonical arrangement makes the whole
        // proposal unsendable: `setBusArrangements` takes one arrangement per
        // bus, so there is no way to say "no opinion" about a single entry.
        // Skipping the call entirely is the honest move — the plugin keeps the
        // layout it already has, and the reconcile below reports it. Proposing
        // a made-up mask for that bus would name the wrong speakers.
        let proposals = plugin
            .audio_bus_channels(BusDirection::Input)
            .into_iter()
            .map(|c| default_arrangement_for(bus_layout(c)))
            .collect::<Option<Vec<_>>>()
            .zip(
                plugin
                    .audio_bus_channels(BusDirection::Output)
                    .into_iter()
                    .map(|c| default_arrangement_for(bus_layout(c)))
                    .collect::<Option<Vec<_>>>(),
            );

        let Some((mut inputs, mut outputs)) = proposals else {
            self.loaded.reconcile_bus_counts();
            return Ok(());
        };

        let result = plugin.set_bus_arrangements(&mut inputs, &mut outputs);

        // Anything but `kResultTrue`/`kResultOk` (typically `kResultFalse`):
        // the plugin kept its own layout. Read it back and re-resolve scratch
        // to match. Not an error.
        if result.is_err() {
            let in_layouts = self
                .loaded
                .read_back_arrangements(BusDirection::Input, inputs.len());
            let out_layouts = self
                .loaded
                .read_back_arrangements(BusDirection::Output, outputs.len());
            let widths = |ls: &[ChannelTopology]| -> Vec<usize> {
                ls.iter().map(|t| t.layout().count() as usize).collect()
            };
            self.resolve_scratch_from_counts(&widths(&in_layouts), &widths(&out_layouts));
        }

        // Re-enumerated from the component rather than from `in_counts` above,
        // so there is one answer to "what is the layout" and it is the same
        // enumeration `activate_buses` and the restart path use.
        // `getBusArrangement` reports only the buses that existed at proposal
        // time, and a refusal is exactly when a plugin may have restructured
        // them.
        self.loaded.reconcile_bus_counts();
        Ok(())
    }

    /// Re-resolve the per-direction audio scratch and pointer tables from
    /// explicit per-bus channel counts (the plugin's chosen layout after a
    /// `setBusArrangements` refusal). Mirrors the resolve in `activate_buses`,
    /// but from counts rather than a fresh component enumeration.
    fn resolve_scratch_from_counts(&mut self, in_counts: &[usize], out_counts: &[usize]) {
        let block_size = self.audio.config.block_size;
        // Bus 0's count, or 0 when the plugin named no bus in that direction.
        // The scratch below is sized from the full per-bus slices, not from
        // these, so an empty direction stays empty rather than being rounded up
        // to a channel the plugin never declared.
        let num_in = bus_layout(in_counts.first().copied().unwrap_or(0));
        let num_out = bus_layout(out_counts.first().copied().unwrap_or(0));
        let in_scratch = BusBuffers::new(T::WIDTH, in_counts, num_in, block_size);
        let out_scratch = BusBuffers::new(T::WIDTH, out_counts, num_out, block_size);

        self.audio.config.num_input_channels = num_in;
        self.audio.config.num_output_channels = num_out;
        self.audio.input.buses = in_scratch;
        self.audio.output.buses = out_scratch;
    }

    /// The `ProcessSetup` describing this instance's negotiated configuration.
    ///
    /// The single source of the setup values, so what `setupProcessing`
    /// receives and what the conformance observer is told cannot drift — a
    /// second, hand-built copy for the observer would agree with `ProcessData`
    /// by construction and hide exactly the mismatch the check exists to find.
    fn apply_process_setup(&mut self) -> Result<()> {
        let setup = self.audio.config.process_setup(T::VST3_SYMBOLIC_SIZE);
        // `Ok(false)` is `kResultFalse`, which the SDK lets a plugin answer.
        self.loaded
            .plugin
            .setup_processing(setup)
            .map(drop)
            .map_err(|e| Vst3Error::PluginError {
                stage: LoadStage::Setup,
                code: e.code(),
            })
    }

    /// Activates the buses this host wants live, per
    /// [`wants_activation`](super::wants_activation).
    ///
    /// Event buses are activated on the same terms as audio ones: the spec
    /// starts every bus inactive regardless of media type, and `activateBus`
    /// takes the type as a parameter precisely because it is not audio-only.
    /// Reading only the counts — to decide whether the plugin speaks MIDI at
    /// all — leaves event buses inactive: a plugin that honours the inactive
    /// default then receives no events while one that ignores it works, which
    /// presents as a plugin quirk rather than a host bug.
    fn activate_buses(&mut self) -> Result<()> {
        let plugin = &self.loaded.plugin;
        for (media, direction) in [
            (MediaType::Audio, BusDirection::Input),
            (MediaType::Audio, BusDirection::Output),
            (MediaType::Event, BusDirection::Input),
            (MediaType::Event, BusDirection::Output),
        ] {
            for i in 0..plugin.bus_count(media, direction) {
                let info = plugin.bus_info(media, direction, i);
                // A bus whose info cannot be read is activated anyway. That
                // restores the pre-policy behaviour for exactly the plugins
                // that cannot answer the question the policy asks, rather than
                // silently dropping a bus over a failed query.
                let wants = match &info {
                    Ok(bus) => super::wants_activation(bus.bus_type(), bus.flags()),
                    Err(_) => true,
                };
                if wants {
                    let _ = plugin.activate_bus(media, direction, i);
                }
            }
        }

        // Re-resolve the per-bus scratch from the live component — some plugins
        // only finalise their bus arrangement once activated, so the layout can
        // differ from the PluginInfo snapshot used in `from_loaded`. Setup-time
        // only; never on the audio thread.
        //
        // Both the flat count and the scratch come from this one enumeration.
        // Querying them separately lets the flat one fall back to the
        // pre-activation value when its query fails — reinstating the stale
        // layout this re-resolve exists to replace, at exactly the moment the
        // live read said it could not be trusted.
        let in_counts = plugin.audio_bus_channels(BusDirection::Input);
        let out_counts = plugin.audio_bus_channels(BusDirection::Output);
        let num_in = bus_layout(in_counts.first().copied().unwrap_or(0));
        let num_out = bus_layout(out_counts.first().copied().unwrap_or(0));
        let block_size = self.audio.config.block_size;
        let in_scratch = BusBuffers::new(T::WIDTH, &in_counts, num_in, block_size);
        let out_scratch = BusBuffers::new(T::WIDTH, &out_counts, num_out, block_size);

        self.audio.config.num_input_channels = num_in;
        self.audio.config.num_output_channels = num_out;
        self.audio.input.buses = in_scratch;
        self.audio.output.buses = out_scratch;
        Ok(())
    }

    fn set_active(&mut self, active: bool) -> Result<()> {
        let result = self.loaded.plugin.set_active(active);
        // `kResultFalse` is a *refusal*, not a "didn't implement it". A plugin
        // whose licence check, dongle, or device claim fails reports it here,
        // and it is the only way it can. Accepting it as success would leave the
        // host believing an inactive plugin was live and calling `process` on it —
        // which is undefined, and which the plugin has no way to prevent.
        //
        // Steinberg's own suite agrees: `validstatetransition.cpp` fails the
        // plugin unless `setActive` returns exactly `kResultTrue`.
        //
        // This is the opposite of the `setProcessing` call below, where a
        // non-OK result genuinely does mean "not implemented" — see there.
        if let Err(error) = result {
            return Err(Vst3Error::PluginError {
                stage: LoadStage::Activation,
                code: error.code(),
            });
        }
        if active {
            // `setProcessing` only *informs* the plugin that processing is about
            // to start; the SDK's own `AudioEffect` base returns
            // `kNotImplemented` for it, so every plugin that doesn't need the
            // notification reports failure here. Treating that as fatal rejects
            // valid plugins — 7 of Steinberg's 11 reference plugins among them.
            // Nothing downstream depends on the result, so ignore it.
            let _ = self.loaded.plugin.set_processing(true);
        }
        Ok(())
    }
}

#[cfg(test)]
mod default_arrangement_tests {
    use super::super::speakers;
    use super::*;

    /// Each width proposes the arrangement whose speakers the engine means.
    ///
    /// The table is asserted against `SpeakerArr`'s named constants rather than
    /// against literals: a literal would agree with a wrong mask that happened
    /// to have the right popcount, such as `(1 << n) - 1`. Widths 4 and 8 are
    /// the two such a mask gets wrong.
    #[test]
    fn each_width_proposes_its_named_arrangement() {
        for (width, expected, name) in [
            (0u16, SpeakerArr::kEmpty, "kEmpty"),
            (1, SpeakerArr::kMono, "kMono"),
            (2, SpeakerArr::kStereo, "kStereo"),
            (4, SpeakerArr::k40Music, "k40Music"),
            (6, SpeakerArr::k51, "k51"),
            (8, SpeakerArr::k71Music, "k71Music"),
        ] {
            assert_eq!(
                default_arrangement_for(ChannelLayout::from(width)),
                Some(expected),
                "width {width} must propose {name}"
            );
        }
    }

    /// The proposal a width makes decodes back to that many channels.
    ///
    /// Guards the pairing rather than the mask: an entry naming a real
    /// arrangement of the *wrong width* — `k51` for width 4, say — would pass
    /// the table test above if the expectation were edited to match, but a
    /// plugin would then be handed a bus of a different size than the host
    /// allocated.
    #[test]
    fn a_proposed_arrangement_has_the_width_it_was_asked_for() {
        for width in [0u16, 1, 2, 4, 6, 8] {
            let arr = default_arrangement_for(ChannelLayout::from(width)).expect("width is named");
            assert_eq!(
                speakers::from_arrangement(arr).layout().count(),
                width,
                "the arrangement proposed for width {width} is not {width} channels"
            );
        }
    }

    /// The 4- and 8-channel proposals are not the low-bit masks.
    ///
    /// Named explicitly because those two are where a low-bit mask has the
    /// right channel count and the wrong speakers, so every width-based check
    /// passes while a quad bus is negotiated as a centre-plus-LFE 3.1.
    #[test]
    fn the_four_and_eight_channel_proposals_are_not_the_low_bit_masks() {
        for width in [4u16, 8] {
            let proposed = default_arrangement_for(ChannelLayout::from(width)).expect("named");
            let old_mask: SpeakerArrangement = (1u64 << width) - 1;
            assert_ne!(
                proposed, old_mask,
                "width {width} is still proposing the low-{width}-bits mask"
            );
        }
    }

    /// A fully-named topology whose width matches its bus is reported.
    #[test]
    fn a_named_topology_matching_its_bus_width_is_usable() {
        let stereo = speakers::from_arrangement(SpeakerArr::kStereo);
        assert_eq!(usable_topology(stereo.clone(), 2), Some(stereo));
    }

    /// A topology naming a speaker this vocabulary lacks is not usable.
    ///
    /// The width is right and the bus is real — but nothing can route it by
    /// speaker, so reporting it would promise more than is known.
    #[test]
    fn a_topology_with_an_unnamed_speaker_is_not_usable() {
        // `kSpeakerLfe2` is a real VST3 speaker with no name here.
        let arr = SpeakerArr::kStereo | vst3::Steinberg::Vst::kSpeakerLfe2;
        let topology = speakers::from_arrangement(arr);
        assert_eq!(topology.layout().count(), 3, "the width is still known");
        assert_eq!(usable_topology(topology, 3), None);
    }

    /// A topology whose width disagrees with its bus is not usable.
    ///
    /// The count sizes the buffers, so a topology describing a different number
    /// of channels describes a different bus — there is no way to adjudicate
    /// which is right, and acting on the wrong one misroutes audio.
    #[test]
    fn a_topology_that_disagrees_with_its_bus_width_is_not_usable() {
        let stereo = speakers::from_arrangement(SpeakerArr::kStereo);
        assert_eq!(usable_topology(stereo, 6), None);
    }

    /// An empty topology on a zero-width bus is not reported as a real answer.
    ///
    /// This is the one a naive check misses: a failed `getBusArrangement`
    /// yields an empty topology, which is *vacuously* fully named and whose
    /// width *does* equal the zero count beside it. Both other guards pass, so
    /// only the explicit `width > 0` stops "the plugin never answered" from
    /// being reported as "this bus has no channels".
    #[test]
    fn an_empty_topology_is_not_a_usable_answer() {
        let empty = speakers::from_arrangement(SpeakerArr::kEmpty);
        assert!(empty.is_fully_named(), "vacuously true, which is the trap");
        assert_eq!(empty.layout().count(), 0);
        assert_eq!(usable_topology(empty, 0), None);
    }

    /// A width with no canonical arrangement declines rather than inventing one.
    ///
    /// `None` is what makes the caller skip `setBusArrangements` entirely, so a
    /// plugin keeps the layout it already had instead of being handed a
    /// fabricated one.
    #[test]
    fn an_unnamed_width_has_no_proposal() {
        for width in [3u16, 5, 7, 9, 12] {
            assert_eq!(
                default_arrangement_for(ChannelLayout::from(width)),
                None,
                "width {width} should have no canonical arrangement"
            );
        }
    }
}
