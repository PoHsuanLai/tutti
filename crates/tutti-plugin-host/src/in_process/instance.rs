//! The in-process plugin's control side: its [`Lifecycle`], with the
//! Processor lent to the node's realtime end through a threaded [`Hand`]
//! (doc 016 §2.6; doc 021 step 4a-3).
//!
//! The audio thread never reaches this: it renders through the Processor the
//! node's [`Rt`] holds, and takes no lock. The control side (the handle, the
//! node's `shape` and `prepare`, a fork) reaches it under one mutex that only
//! control threads take. A rate change is a recall, a rejoin and an
//! activation ([`HostOp::Reconfigure`]): the node's `prepare` runs it to the
//! end with its own realtime end, which is not in an executor then; a rate
//! parked for the main thread is run by the pump, and the Processor comes
//! back at the audio thread's next poll.

use std::path::Path;
use tutti_plugin_types::MainThread;

use tutti_plugin_types::{
    Event, Hand, HostOp, InProcess, Lifecycle, Load, MaxBlock, Notice, Origin, PluginError,
    Prepare, Processor, RenderMode, SampleRate, Samples, State, standing,
};
use tutti_rt::Rt;

use tutti_plugin_types::LoadStage;
use tutti_plugin_types::PluginTail;
use tutti_plugin_types::{Normalized, ParamId};

/// What the plugin changed about itself since the last
/// [`Instance::pump`]: each field is a re-read the core did after the
/// plugin's own request.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Changes {
    /// The new latency, when it moved.
    pub(super) latency: Option<Samples>,
    /// The new tail, when it moved.
    pub(super) tail: Option<PluginTail>,
    /// The plugin raised [`Request::IoChanged`](tutti_plugin_types::Request::IoChanged): its pin counts or latency
    /// may differ.
    pub(super) io: bool,
}

/// A plugin of format `F` and where it is in its life.
pub(super) struct Instance<F: InProcess> {
    lifecycle: Lifecycle<F>,
    state: State,
    /// The Processor's way to the node's realtime end.
    hand: Hand<F::Processor>,
    /// Where the plugin came from: what a fork loads again.
    origin: Origin,
    /// What the plugin runs at, or was last asked to. Its `mode` follows the
    /// live render mode ([`set_render_mode`](Self::set_render_mode)), so
    /// a rate change activates in the mode the plugin was told.
    prepare: Prepare,
    /// What the pump has not yet reported.
    changes: Changes,
    /// What the control side has not yet been told about the plugin's rate:
    /// `Some(Some(..))` a refusal, `Some(None)` the rate taken again after one.
    /// Read by [`take_rate_report`](Self::take_rate_report).
    rate_report: Option<Option<(SampleRate, String)>>,
    /// The last rate change was refused (the report above says so once).
    rate_refused: bool,
}

impl<F: InProcess> Instance<F> {
    /// Loads and activates the plugin at `sample_rate` and blocks of up to
    /// `block_size`, 32-bit, in realtime mode, and lends its Processor to the
    /// realtime end this returns. Main thread.
    ///
    /// # Errors
    ///
    /// The stage that failed and the reason.
    ///
    /// `restore` runs on the plugin **before it is activated**: a plugin may take
    /// its mode from the state at activation, and the activation then is the
    /// state's. Its outcome is returned beside the instance; a refusal does not
    /// fail the load (a plugin at its defaults beats none).
    pub(super) fn load(
        main: &MainThread,
        path: &Path,
        load: &Load,
        sample_rate: f64,
        block_size: usize,
        restore: Option<&mut dyn FnMut(&mut F) -> Result<(), String>>,
    ) -> Result<(Self, Rt<F::Processor>, Option<Result<(), String>>), (LoadStage, String)> {
        let mut loaded = F::load_in_process(main, path, load).map_err(|e| match e {
            PluginError::Load { stage, reason } => (stage, reason),
            other => (LoadStage::Opening, other.to_string()),
        })?;
        let origin = loaded.origin().clone();
        let restored = restore.map(|restore| restore(&mut loaded));
        let prepare = MaxBlock::new(Samples(block_size))
            .and_then(|block| Prepare::new(SampleRate(sample_rate), block))
            .ok_or_else(|| {
                (
                    LoadStage::Activation,
                    format!(
                        "the plugin cannot run at {sample_rate} Hz in blocks of {block_size} frames"
                    ),
                )
            })?;
        let (hand, rt) = Hand::threaded();
        let mut this = Self {
            lifecycle: Lifecycle::new(loaded),
            state: Lifecycle::<F>::initial_state(),
            hand,
            origin,
            prepare,
            changes: Changes::default(),
            rate_report: None,
            rate_refused: false,
        };
        let refused = this.drive(main, Event::Op(HostOp::Activate(prepare)));
        if let Some(reason) = refused {
            return Err((
                LoadStage::Activation,
                format!("activation failed: {reason}"),
            ));
        }
        // What the activation read is the baseline, not a change.
        this.changes = Changes::default();
        Ok((this, rt, restored))
    }

    /// Steps `event` and keeps what it told; the reason of a refused
    /// activation, if it was one.
    fn drive(&mut self, main: &MainThread, event: Event) -> Option<String> {
        let mut refused = None;
        let changes = &mut self.changes;
        let state = self.state.clone();
        self.state =
            self.lifecycle
                .drive(state, event, &mut self.hand, main, |notice| match notice {
                    Notice::LatencyChanged(latency) => changes.latency = Some(latency),
                    Notice::TailChanged(tail) => changes.tail = Some(tail),
                    Notice::IoChanged => changes.io = true,
                    Notice::Refused { reason, .. }
                    | Notice::Stuck { reason }
                    | Notice::Lost { reason } => refused = Some(reason.to_string()),
                    Notice::ParamValuesChanged
                    | Notice::NoteExpressionChanged
                    | Notice::ParamTitlesChanged
                    | Notice::Reloaded => {}
                });
        refused
    }

    /// The plugin's `Control` surface (parameters, state, presets), wherever
    /// it is in its life. `None` once it is lost.
    pub(super) fn control(&mut self) -> Option<&mut dyn tutti_plugin_types::Control> {
        self.lifecycle.control()
    }

    /// Whether the plugin is gone (a call into it panicked): it renders
    /// silence and the pump skips it.
    #[cfg(test)]
    #[must_use]
    pub(super) fn is_lost(&self) -> bool {
        self.lifecycle.is_poisoned()
    }

    /// The latency and tail as the control side last read them. Dispatches
    /// nothing, so the graph compile may call it.
    pub(super) fn figures(&self) -> (Samples, PluginTail) {
        (
            self.state.latency().unwrap_or_default(),
            self.state.tail().unwrap_or_default(),
        )
    }

    /// The render mode the plugin runs in, or was last told.
    pub(super) fn render_mode(&self) -> RenderMode {
        self.prepare.mode()
    }

    /// Tells the plugin its render mode, and keeps the mode a rate change
    /// activates it in. Main thread.
    ///
    /// A format that takes it live (VST2: a callback answer; CLAP's
    /// `render.set`) is told through [`Control::set_render_mode`](tutti_plugin_types::Control::set_render_mode). One that
    /// cannot (`false`) is reconfigured through the lifecycle: the activation
    /// carries the mode, and the Processor changes at the pump, as a rate
    /// change's does. Returns whether the plugin now runs in `mode` (or is
    /// on its way to it).
    pub(super) fn set_render_mode(&mut self, main: &MainThread, mode: RenderMode) -> bool {
        self.prepare = self.prepare.with_mode(mode);
        self.state.set_mode(mode);
        let live = self
            .lifecycle
            .control()
            .is_some_and(|control| control.set_render_mode(main, mode));
        if live {
            return true;
        }
        if self.lifecycle.is_poisoned() {
            return false;
        }
        match self.drive(main, Event::Op(HostOp::Reconfigure(self.prepare))) {
            None => true,
            Some(why) => {
                self.note_refusal(why);
                false
            }
        }
    }

    /// Tells the plugin its rate: a recall, a rejoin and an activation at the
    /// new [`Prepare`], which is how VST2 takes one (`effSetSampleRate`
    /// between `effMainsChanged` brackets). Main thread.
    ///
    /// With `rt`, the node's own realtime end out of any executor (its
    /// `prepare`), the whole exchange runs here and the new Processor is in
    /// `rt` on return. Without, the recall waits for the audio thread's next
    /// poll and a later [`pump`](Self::pump) finishes it.
    ///
    /// A plugin that refuses the rate renders nothing until a rate it takes
    /// (doc 016's rule for every format); the figures it recomputes are read
    /// after the activation.
    ///
    /// # Errors
    ///
    /// Why the plugin did not take the rate, worded as the other formats word it
    /// (`standing`: where the plugin stands). The refusal is also kept for
    /// [`take_rate_report`](Self::take_rate_report), which is how the control side
    /// hears of one the audio thread's parked rate caused.
    pub(super) fn set_sample_rate(
        &mut self,
        main: &MainThread,
        rate: f64,
        rt: Option<&mut Rt<F::Processor>>,
    ) -> Result<(), String> {
        let refused = |this: &Self, why: &str| {
            format!(
                "plugin refused sample rate {rate} ({}): {why}",
                standing(this.state.renders(), this.prepare.rate())
            )
        };
        let result = match self.prepare.with_rate(SampleRate(rate)) {
            // Before the plugin is touched: nothing is recalled for a rate
            // that is no rate.
            None => Err(refused(self, "not a positive finite rate")),
            Some(wanted) => {
                let mut outcome = self.drive(main, Event::Op(HostOp::Reconfigure(wanted)));
                if let Some(rt) = rt {
                    // The recall raised above: the realtime end hands the
                    // Processor back, the tick runs the parked plan, and the
                    // new Processor is lent and taken.
                    let _ = rt.poll(Processor::stop);
                    let ticked = self.drive(main, Event::Tick);
                    outcome = outcome.or(ticked);
                    let _ = rt.poll(Processor::stop);
                }
                match outcome {
                    Some(why) => Err(refused(self, &why)),
                    None => {
                        self.prepare = wanted;
                        Ok(())
                    }
                }
            }
        };
        match &result {
            Err(reason) => {
                self.rate_refused = true;
                self.rate_report = Some(Some((SampleRate(rate), reason.clone())));
            }
            Ok(()) if self.rate_refused => {
                self.rate_refused = false;
                self.rate_report = Some(None);
            }
            Ok(()) => {}
        }
        result
    }

    /// The refusal (or the recovery from one) not yet reported, once: what the
    /// control side turns into `PluginInvalidation::RateRefused` or `RateRestored`.
    pub(super) fn take_rate_report(&mut self) -> Option<Option<(SampleRate, String)>> {
        self.rate_report.take()
    }

    /// The main thread's tick: delivers what the plugin asked for, collects a
    /// Processor the audio thread returned (running a parked rate change), and
    /// reports what the plugin changed about itself since the last call.
    ///
    /// Re-reads on `audioMasterIOChanged` (the plugin's own signal: its pin
    /// counts and latency may both have moved), and reports what an activation
    /// read. Main thread: a re-read dispatches `effGetTailSize`.
    pub(super) fn pump(&mut self, main: &MainThread) -> Changes {
        let mut asked = Vec::new();
        if let Some(control) = self.lifecycle.control() {
            control.take_requests(main, &mut asked);
        }
        // The plugin's own signal, reported as it was raised: its pin counts or
        // latency may have moved, and the handle's `Io` says so even when the
        // re-read finds the same layout (the core reports a layout change
        // only).
        if asked.contains(&tutti_plugin_types::Request::IoChanged) {
            self.changes.io = true;
        }
        for request in asked {
            if let Some(why) = self.drive(main, Event::Request(request)) {
                self.note_refusal(why);
            }
        }
        if let Some(why) = self.drive(main, Event::Tick) {
            self.note_refusal(why);
        }
        std::mem::take(&mut self.changes)
    }

    /// A refusal the pump's plan met (a parked rate change), kept for the
    /// control side.
    fn note_refusal(&mut self, why: String) {
        self.rate_refused = true;
        self.rate_report = Some(Some((self.prepare.rate(), why)));
    }

    /// The plugin's parameter edits since the last call.
    pub(super) fn drain_param_changes(&mut self, main: &MainThread) -> Vec<(ParamId, Normalized)> {
        let mut edits = Vec::new();
        if let Some(control) = self.lifecycle.control() {
            control.take_reported_edits(main, &mut edits);
        }
        edits
    }

    /// Whether the plugin is split and rendering.
    #[cfg(test)]
    pub(super) fn renders(&self) -> bool {
        self.state.renders()
    }

    /// The rate the plugin runs at, or was last asked to.
    #[cfg(test)]
    pub(super) fn rate(&self) -> SampleRate {
        self.prepare.rate()
    }

    /// Where the plugin came from: what a fork loads again.
    pub(super) fn origin(&self) -> &Origin {
        &self.origin
    }
}

#[cfg(test)]
mod tests {
    use tutti_fixtures::main_thread;
    use tutti_vst2_host::Vst2Loaded;

    use super::*;

    /// **A load at no rate or no block is refused** at the activation stage:
    /// zero, negative and non-finite rates, and a block of no frames.
    ///
    /// Mutation (run): `valid_rate` in `tutti-plugin-types` accepting every
    /// rate, and `MaxBlock::new` accepting zero, each -> the matching case loads
    /// -> fails.
    #[test]
    fn a_load_at_no_rate_or_no_block_is_refused() {
        for (rate, block) in [
            (0.0, 64),
            (-48_000.0, 64),
            (f64::NAN, 64),
            (f64::INFINITY, 64),
            (f64::NEG_INFINITY, 64),
            (48_000.0, 0),
        ] {
            match Instance::<Vst2Loaded>::load(
                &main_thread(),
                tutti_fixtures::vst2_probe(),
                &Load::new(),
                rate,
                block,
                None,
            ) {
                Err((LoadStage::Activation, reason)) => {
                    assert!(reason.contains("cannot run at"), "{reason}");
                }
                Err((stage, reason)) => panic!("{rate} Hz, {block}: {stage:?} {reason}"),
                Ok(_) => panic!("{rate} Hz in {block} frames must be refused"),
            }
        }
    }

    /// **A rate that is no rate leaves the instance as it was, and says so**: the plugin is
    /// not taken apart (its halves stay, it is not lost), the configuration is the one it had,
    /// and the refusal is an error worded as the other formats word it (the rate, where the
    /// plugin stands, why) and a report for the control side
    /// (`PluginInvalidation::RateRefused`). A rate the plugin then takes reports the
    /// recovery once (`RateRestored`).
    ///
    /// Mutation (run): `valid_rate` in `tutti-plugin-types` accepting every
    /// rate -> the zero rate is activated and kept as the configuration ->
    /// `prepare` changes -> fails. Mutation (run): the `None` arm of
    /// `set_sample_rate`'s match returning `Ok(())` -> the error assertion fails.
    /// Mutation (run): `set_sample_rate` not storing `rate_report` -> the report assertion
    /// fails. Mutation (run): the `Ok(()) if self.rate_refused` arm removed -> the
    /// restored assertion fails.
    #[test]
    fn a_rate_that_is_no_rate_leaves_the_instance_alone_and_is_reported() {
        let (mut instance, _rt, _) = Instance::<Vst2Loaded>::load(
            &main_thread(),
            tutti_fixtures::vst2_probe(),
            &Load::new(),
            48_000.0,
            64,
            None,
        )
        .expect("loads");
        let before = instance.prepare;
        assert_eq!(instance.take_rate_report(), None, "nothing to report yet");
        for rate in [0.0, -48_000.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let reason = instance
                .set_sample_rate(&main_thread(), rate, None)
                .expect_err("a rate that is no rate is refused");
            assert!(
                reason.contains("running at 48000") && reason.contains("not a positive finite"),
                "{rate}: {reason}"
            );
            let report = instance.take_rate_report().expect("and reported");
            let (reported, why) = report.expect("a refusal, not a recovery");
            assert!(
                reported.0 == rate || (reported.0.is_nan() && rate.is_nan()),
                "{rate}"
            );
            assert_eq!(why, reason);
            assert_eq!(instance.take_rate_report(), None, "once");
            assert_eq!(instance.prepare, before, "{rate}");
            assert!(instance.renders(), "{rate}");
            assert!(!instance.is_lost(), "{rate}");
        }
        instance
            .set_sample_rate(&main_thread(), 96_000.0, None)
            .expect("a real rate is taken");
        assert_eq!(instance.prepare.rate().0, 96_000.0);
        assert_eq!(
            instance.take_rate_report(),
            Some(None),
            "the recovery is reported"
        );
        instance
            .set_sample_rate(&main_thread(), 48_000.0, None)
            .expect("and again");
        assert_eq!(
            instance.take_rate_report(),
            None,
            "a plain change reports nothing"
        );
    }

    /// **A rate change made off the realtime thread completes at the pump,
    /// once the audio thread hands the Processor back.** The change recalls
    /// it; the realtime end, at its next poll, stops it and returns it; the
    /// pump runs the parked plan and lends the new Processor, which the next
    /// poll takes.
    ///
    /// Mutation (run): `pump` not driving `Event::Tick` -> the returned
    /// Processor is never collected, the plan stays parked and the last poll
    /// finds nothing -> fails.
    #[test]
    fn a_parked_rate_change_completes_at_the_pump() {
        let main = main_thread();
        let (mut instance, mut rt, _) = Instance::<Vst2Loaded>::load(
            &main,
            tutti_fixtures::vst2_probe(),
            &Load::new(),
            48_000.0,
            64,
            None,
        )
        .expect("loads");
        assert!(rt.poll(Processor::stop).is_some(), "lent at the load");
        instance
            .set_sample_rate(&main, 96_000.0, None)
            .expect("the change starts");
        assert!(rt.poll(Processor::stop).is_none(), "handed back");
        assert!(instance.state.is_parked(), "waits for the Processor");
        let _ = instance.pump(&main);
        assert!(instance.renders());
        assert_eq!(instance.rate(), SampleRate(96_000.0));
        assert!(rt.poll(Processor::stop).is_some(), "the new Processor");
    }
}
