//! [`Lifecycle`]: one plugin in whichever state it is in, and the interpreter
//! of [`Effect`]s against it.
//!
//! The pure core ([`step`]) decides; `Lifecycle` performs. It is written once
//! for every format that implements [`Loaded`]: [`drive`](Lifecycle::drive)
//! steps an event, applies each effect it emits, and feeds each reply back
//! until none remain.
//!
//! Everything here runs on the main thread. The Processor reaches the realtime
//! thread through a [`Hand`]; a [`Hand::local`] has no realtime thread, so the
//! Processor stays in the [`Seat`] and a single-thread user renders through
//! [`Lifecycle::processor`].

use std::collections::VecDeque;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::{
    Control, Controller, Effect, Event, Layout, Loaded, LoadedPlugin, Normalized, Notice, ParamId,
    ParameterInfo, PluginDescriptor, PluginError, PluginResult, Prepare, Processor, Refused, Reply,
    SampleRate, State, Stuck, step,
};

/// The main thread's end of the Processor's trip to the realtime thread.
///
/// [`local`](Self::local) is the end for a user with no realtime thread:
/// [`Effect::Lend`] does nothing and [`Effect::Recall`] answers
/// [`Reply::Returned`] at once.
#[derive(Debug)]
pub struct Hand<P> {
    _processor: PhantomData<fn() -> P>,
}

impl<P> Hand<P> {
    /// The end for a user whose Processor never leaves the main thread.
    #[must_use]
    pub fn local() -> Self {
        Self {
            _processor: PhantomData,
        }
    }
}

/// Where the Processor of an active plugin is.
#[derive(Debug)]
pub enum Seat<P> {
    /// On the main thread: single-thread use, between a rejoin and a lend, or
    /// after a recall.
    Held(P),
    /// On the realtime thread, which returns it only between blocks.
    Lent,
}

/// A plugin in whichever state it is in.
///
/// The typestate of [`Loaded`] stays in the enum: a transition takes the
/// plugin out, consumes it, and puts back what the result names.
#[derive(Debug)]
pub enum Lifecycle<L: Loaded> {
    /// Configured, not rendering.
    Loaded(L),
    /// Split. The Controller is here; the Processor is held or lent.
    ///
    /// `seat` is declared before `controller` so a dropped `Lifecycle` drops a
    /// held Processor first: the Controller destroys the plugin, and a
    /// Processor must not outlive it. A Processor that is `Lent` is not here to
    /// drop; the main thread recalls it before dropping the `Lifecycle` (the
    /// threaded `Hand` enforces that, and a local hand never lends).
    Active {
        /// The realtime half, or where it is.
        seat: Seat<L::Processor>,
        /// The main-thread half.
        controller: L::Controller,
    },
    /// The plugin refused to run; it is loaded and renders nothing.
    Refused {
        /// The plugin.
        plugin: L,
        /// The rate refused, or `None` when it was a rebuild that was refused.
        rate: Option<SampleRate>,
    },
    /// A transition left no plugin, or the process died. [`Lifecycle::drive`]
    /// sends [`Event::Died`] for it, and nothing leaves this state.
    Poisoned,
}

fn panicked(what: &str) -> PluginError {
    PluginError::Other(format!("{what} panicked"))
}

impl<L: Loaded> Lifecycle<L> {
    /// A freshly loaded plugin.
    #[must_use]
    pub fn new(plugin: L) -> Self {
        Self::Loaded(plugin)
    }

    /// The core's state for a freshly loaded plugin of this format.
    #[must_use]
    pub fn initial_state() -> State {
        State::new(L::RESTARTS)
    }

    /// Whether a transition has left no plugin.
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        matches!(self, Self::Poisoned)
    }

    /// The control surface, in any state that has a plugin, lent or not.
    pub fn control(&mut self) -> Option<&mut dyn Control> {
        match self {
            Self::Loaded(plugin) | Self::Refused { plugin, .. } => Some(plugin),
            Self::Active { controller, .. } => Some(controller),
            Self::Poisoned => None,
        }
    }

    /// The Processor, while it is held. A single-thread user renders through
    /// this.
    pub fn processor(&mut self) -> Option<&mut L::Processor> {
        match self {
            Self::Active {
                seat: Seat::Held(processor),
                ..
            } => Some(processor),
            _ => None,
        }
    }

    fn control_ref(&self) -> Option<&dyn Control> {
        match self {
            Self::Loaded(plugin) | Self::Refused { plugin, .. } => Some(plugin),
            Self::Active { controller, .. } => Some(controller),
            Self::Poisoned => None,
        }
    }

    /// Interprets one effect against the plugin. Control thread.
    ///
    /// [`Effect::Notify`] is the driver's: it is delivered by
    /// [`drive`](Self::drive), and here answers [`Reply::Done`].
    ///
    /// A panic out of the plugin leaves [`Lifecycle::Poisoned`], not a
    /// half-moved plugin, whichever call it came from. `hand` is unused until a
    /// threaded hand exists: a local one needs nothing from the effects that
    /// move the Processor.
    pub fn apply(&mut self, effect: Effect, hand: &mut Hand<L::Processor>) -> Reply {
        let _ = hand;
        match effect {
            Effect::CallActivate(prepare) => self.activate(prepare),
            Effect::CallDeactivate => self.deactivate(),
            Effect::Rebuild => self.rebuild(),
            other => self
                .guarded(|this| this.apply_in_place(other))
                .unwrap_or(Reply::NotActive),
        }
    }

    /// Runs `f`; a panic out of it leaves `Poisoned` and `None`.
    fn guarded<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> Option<R> {
        match catch_unwind(AssertUnwindSafe(|| f(self))) {
            Ok(result) => Some(result),
            Err(_) => {
                let lost = std::mem::replace(self, Self::Poisoned);
                // Destroying a plugin that just panicked may panic too.
                let _ = catch_unwind(AssertUnwindSafe(|| drop(lost)));
                None
            }
        }
    }

    /// The effects that act on a plugin where it is.
    fn apply_in_place(&mut self, effect: Effect) -> Reply {
        match effect {
            Effect::CallActivate(_) | Effect::CallDeactivate | Effect::Rebuild => {
                unreachable!("transitions are applied by `apply`")
            }
            Effect::Recall => match self {
                Self::Active {
                    seat: Seat::Held(_),
                    ..
                } => Reply::Returned,
                Self::Active {
                    seat: Seat::Lent, ..
                } => Reply::Lent,
                _ => Reply::NotActive,
            },
            Effect::Lend => match self {
                // A local hand has no realtime thread: the seat keeps it.
                Self::Active {
                    seat: Seat::Held(_),
                    ..
                } => Reply::Done,
                _ => Reply::NotActive,
            },
            Effect::ReadLatency => match self {
                Self::Active { controller, .. } => Reply::Latency(controller.latency()),
                _ => Reply::NotActive,
            },
            Effect::ReadTail => match self {
                Self::Active { controller, .. } => Reply::Tail(controller.tail()),
                _ => Reply::NotActive,
            },
            Effect::ReadLayout => match self {
                Self::Active { controller, .. } => {
                    Reply::Layout(Layout::from(&controller.wiring()))
                }
                _ => Reply::NotActive,
            },
            Effect::ReadParams(scope) => self.with_control(|c| c.rescan_parameters(scope)),
            Effect::RebuildMidiMapping => self.with_control(|c| c.rebuild_midi_mapping()),
            Effect::RunMainThread => self.with_control(|c| c.run_main_thread()),
            Effect::RunTimer(id) => self.with_control(|c| c.run_timer(id)),
            Effect::ServiceFd(id) => self.with_control(|c| c.service_fd(id)),
            Effect::Notify(_) => Reply::Done,
        }
    }

    fn with_control(&mut self, f: impl FnOnce(&mut dyn Control)) -> Reply {
        match self.control() {
            Some(control) => {
                f(control);
                Reply::Done
            }
            None => Reply::NotActive,
        }
    }

    fn activate(&mut self, prepare: Prepare) -> Reply {
        let plugin = match std::mem::replace(self, Self::Poisoned) {
            Self::Loaded(plugin) | Self::Refused { plugin, .. } => plugin,
            other => {
                *self = other;
                return Reply::NotActive;
            }
        };
        match catch_unwind(AssertUnwindSafe(|| plugin.activate(prepare))) {
            Ok(Ok((processor, controller))) => {
                *self = Self::Active {
                    seat: Seat::Held(processor),
                    controller,
                };
                Reply::Activated
            }
            Ok(Err(Refused { plugin, reason })) => {
                if let Some(plugin) = plugin {
                    *self = Self::Refused {
                        plugin,
                        rate: Some(prepare.rate),
                    };
                }
                Reply::Refused(reason)
            }
            Err(_) => Reply::Refused(panicked("activate")),
        }
    }

    fn deactivate(&mut self) -> Reply {
        let (controller, processor) = match std::mem::replace(self, Self::Poisoned) {
            Self::Active {
                seat: Seat::Held(processor),
                controller,
            } => (controller, processor),
            other => {
                *self = other;
                return Reply::NotActive;
            }
        };
        match catch_unwind(AssertUnwindSafe(|| L::deactivate(processor, controller))) {
            Ok(Ok(plugin)) => {
                *self = Self::Loaded(plugin);
                Reply::Deactivated
            }
            Ok(Err(Stuck {
                processor,
                controller,
                reason,
            })) => {
                *self = Self::Active {
                    seat: Seat::Held(processor),
                    controller,
                };
                Reply::Stuck(reason)
            }
            Err(_) => Reply::Refused(panicked("deactivate")),
        }
    }

    fn rebuild(&mut self) -> Reply {
        let (plugin, rate) = match std::mem::replace(self, Self::Poisoned) {
            Self::Loaded(plugin) => (plugin, None),
            Self::Refused { plugin, rate } => (plugin, rate),
            other => {
                *self = other;
                return Reply::NotActive;
            }
        };
        match catch_unwind(AssertUnwindSafe(|| plugin.rebuild())) {
            Ok(Ok(plugin)) => {
                *self = Self::Loaded(plugin);
                Reply::Rebuilt
            }
            Ok(Err(Refused { plugin, reason })) => {
                if let Some(plugin) = plugin {
                    *self = Self::Refused { plugin, rate };
                }
                Reply::Refused(reason)
            }
            Err(_) => Reply::Refused(panicked("rebuild")),
        }
    }

    /// Steps `event`, applies every effect it emits, and feeds each reply back
    /// until none remain. `notify` receives the [`Notice`]s in the order they
    /// happened. Returns the new state.
    ///
    /// A transition that leaves no plugin ([`Refused::plugin`] of `None`, a
    /// panic) poisons, and [`Event::Died`] is stepped for it, so the core's
    /// `Poisoned` is reached the one way the design allows. A panic out of any
    /// other call (a read, the idle flush, main-thread work) is told with
    /// [`Notice::Lost`]; a transition's loss is told by its [`Notice::Refused`].
    ///
    /// Call it on every idle pass of the host's loop, with [`Event::Tick`]: the
    /// Controller's retry of refused parameter edits runs there only.
    ///
    /// While the Processor is lent, a plan that needs both halves is parked in
    /// the state by the core; this returns with it parked, and a later `drive`
    /// runs it once [`Event::Returned`] arrives.
    pub fn drive(
        &mut self,
        state: State,
        event: Event,
        hand: &mut Hand<L::Processor>,
        mut notify: impl FnMut(Notice),
    ) -> State {
        if matches!(event, Event::Tick) {
            // The idle pass is the Controller's tick to retry what a full edit
            // ring refused.
            self.guarded(|this| {
                if let Some(control) = this.control() {
                    control.flush_edits();
                }
            });
            if self.is_poisoned() {
                notify(Notice::Lost {
                    reason: panicked("a call into the plugin"),
                });
                return step(state, Event::Died).0;
            }
        }
        let (mut state, effects) = step(state, event);
        let mut queue: VecDeque<Effect> = effects.into_iter().collect();
        while let Some(effect) = queue.pop_front() {
            if let Effect::Notify(notice) = effect {
                notify(notice);
                continue;
            }
            // A transition that loses the plugin answers `Refused`, which the
            // core turns into its own notice; any other call tells the host here.
            let transition = matches!(
                effect,
                Effect::CallActivate(_) | Effect::CallDeactivate | Effect::Rebuild
            );
            let reply = self.apply(effect, hand);
            let (next, effects) = step(state, Event::Replied(reply));
            state = next;
            for effect in effects.into_iter().rev() {
                queue.push_front(effect);
            }
            if self.is_poisoned() && state.stage() != crate::Stage::Poisoned {
                let (next, _dropped) = step(state, Event::Died);
                state = next;
                if !transition {
                    notify(Notice::Lost {
                        reason: panicked("a call into the plugin"),
                    });
                }
                // What already happened is still told; the rest of the plan
                // has no plugin to run on.
                for effect in queue.drain(..) {
                    if let Effect::Notify(notice) = effect {
                        notify(notice);
                    }
                }
            }
        }
        state
    }
}

impl From<&LoadedPlugin> for Layout {
    fn from(wiring: &LoadedPlugin) -> Self {
        Self {
            inputs: wiring.inputs.clone(),
            outputs: wiring.outputs.clone(),
            input_topology: wiring.input_topology.clone(),
            output_topology: wiring.output_topology.clone(),
        }
    }
}

/// A plugin in some state, driven by effects: the main-thread side of a hosted
/// plugin. The realtime side is [`Processor`].
///
/// [`Lifecycle<L>`] implements it for every format; a server generic over
/// `Hosted` runs a fake and a real plugin through one code path.
pub trait Hosted {
    /// The realtime half.
    type Processor: Processor;

    /// The core's state for a freshly loaded plugin.
    fn initial_state(&self) -> State;

    /// [`Lifecycle::drive`].
    fn drive(
        &mut self,
        state: State,
        event: Event,
        hand: &mut Hand<Self::Processor>,
        notify: impl FnMut(Notice),
    ) -> State;

    /// The plugin's identity, while there is a plugin.
    fn info(&self) -> Option<&PluginDescriptor>;
    /// The plugin's wiring, read now.
    fn wiring(&self) -> Option<LoadedPlugin>;
    /// Every parameter. Empty when there is no plugin.
    fn parameters(&self) -> Vec<ParameterInfo>;
    /// A parameter's value.
    fn parameter(&self, id: ParamId) -> Option<Normalized>;
    /// Sets a parameter; nothing when there is no plugin.
    fn set_parameter(&mut self, id: ParamId, value: Normalized);
    /// The plugin's state.
    ///
    /// # Errors
    ///
    /// [`PluginError::Other`] when there is no plugin; otherwise the plugin's.
    fn save_state(&mut self) -> PluginResult<Vec<u8>>;
    /// Restores state.
    ///
    /// # Errors
    ///
    /// [`PluginError::Other`] when there is no plugin; otherwise the plugin's.
    fn load_state(&mut self, bytes: &[u8]) -> PluginResult<()>;
}

fn lost() -> PluginError {
    PluginError::Other("the plugin is lost".into())
}

impl<L: Loaded> Hosted for Lifecycle<L> {
    type Processor = L::Processor;

    fn initial_state(&self) -> State {
        Self::initial_state()
    }

    fn drive(
        &mut self,
        state: State,
        event: Event,
        hand: &mut Hand<L::Processor>,
        notify: impl FnMut(Notice),
    ) -> State {
        Lifecycle::drive(self, state, event, hand, notify)
    }

    fn info(&self) -> Option<&PluginDescriptor> {
        self.control_ref().map(Control::info)
    }

    fn wiring(&self) -> Option<LoadedPlugin> {
        self.control_ref().map(Control::wiring)
    }

    fn parameters(&self) -> Vec<ParameterInfo> {
        self.control_ref()
            .map(Control::parameters)
            .unwrap_or_default()
    }

    fn parameter(&self, id: ParamId) -> Option<Normalized> {
        self.control_ref().and_then(|c| c.parameter(id))
    }

    fn set_parameter(&mut self, id: ParamId, value: Normalized) {
        if let Some(control) = self.control() {
            control.set_parameter(id, value);
        }
    }

    fn save_state(&mut self) -> PluginResult<Vec<u8>> {
        self.control().ok_or_else(lost)?.save_state()
    }

    fn load_state(&mut self, bytes: &[u8]) -> PluginResult<()> {
        self.control().ok_or_else(lost)?.load_state(bytes)
    }
}

#[cfg(test)]
mod tests;
