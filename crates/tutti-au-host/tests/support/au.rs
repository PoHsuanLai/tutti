//! One Audio Unit through the halves an activation splits it into, as one
//! value a test can hold.
//!
//! The product has no such type: a unit is an [`AuLoaded`] until
//! [`Loaded::activate`] splits it into an [`AuProcessor`] and an
//! [`AuController`], and [`Loaded::deactivate`] rejoins them. A test often wants
//! to say "this unit, initialized, now render" and move on, and many of them
//! then move it back and forth (a new rate, a new block ceiling, a render
//! mode). [`Au`] holds whichever state the unit is in, and derefs to the
//! [`AuLoaded`] (through the controller while active), so every inherent call of
//! the host is reachable on it. It adds only the transitions and the raw-slice
//! render calls, each of which is the host's own call on the half that owns it:
//! nothing here decides anything the host does not.

#![cfg(target_os = "macos")]

use std::cell::RefCell;
use std::ops::{Deref, DerefMut};

use tutti_au_ffi::{Element, Scope};
use tutti_au_host::offline::PushScratch;
use tutti_au_host::types::{AudioUnitRenderActionFlags, K_AUDIO_UNIT_ERR_UNINITIALIZED};
use tutti_au_host::{
    AuComponentInfo, AuController, AuError, AuLoaded, AuProcessor, StreamConfig,
};
use tutti_fixtures::main_thread;
use tutti_plugin_types::{
    Control, Loaded, MaxBlock, PluginError, PluginResult, Prepare, RenderMode, SampleRate,
};
use tutti_types::Samples;

enum State {
    Loaded(AuLoaded),
    Active {
        processor: RefCell<AuProcessor>,
        controller: AuController,
    },
    /// Only seen after an activation that could not hand the unit back.
    Gone,
}

/// A unit in whichever state it is in; see the module docs.
pub struct Au {
    state: State,
    /// What the next activation carries: the rate and block ceiling of the
    /// configuration the unit was built at, then whatever the test changed.
    prepare: Prepare,
}

fn prepare_of(rate: f64, block: u32) -> Prepare {
    Prepare::new(
        SampleRate(rate),
        MaxBlock::of(Samples(block as usize)),
    )
    .expect("a usable rate")
}

impl Au {
    /// A unit at `rate`/`block`, not initialized. See [`AuLoaded::new`].
    pub fn new(
        component: tutti_au_host::component::AuComponentRef,
        rate: f64,
        block: u32,
    ) -> tutti_au_host::Result<Self> {
        Ok(Self::of(AuLoaded::new(component, rate, block)?))
    }

    /// See [`AuLoaded::new_with_config`].
    pub fn new_with_config(
        component: tutti_au_host::component::AuComponentRef,
        config: StreamConfig,
    ) -> tutti_au_host::Result<Self> {
        Ok(Self::of(AuLoaded::new_with_config(component, config)?))
    }

    /// A unit from a scan result, not initialized.
    pub fn from_info(info: &AuComponentInfo, rate: f64, block: u32) -> tutti_au_host::Result<Self> {
        Self::new(info.component, rate, block)
    }

    fn of(loaded: AuLoaded) -> Self {
        let prepare = prepare_of(loaded.config().sample_rate, loaded.config().block_size);
        Self {
            state: State::Loaded(loaded),
            prepare,
        }
    }

    /// Whether the unit is initialized: split into its halves.
    pub fn is_initialized(&self) -> bool {
        matches!(self.state, State::Active { .. })
    }

    /// Activates at the current [`Prepare`]. No-op if already active. On a
    /// refusal the unit is as it was (loaded), unless it could not be handed
    /// back, which leaves it gone.
    pub fn initialize(&mut self) -> PluginResult<()> {
        match std::mem::replace(&mut self.state, State::Gone) {
            State::Loaded(loaded) => match loaded.activate(&main_thread(), self.prepare) {
                Ok((processor, controller)) => {
                    self.state = State::Active {
                        processor: RefCell::new(processor),
                        controller,
                    };
                    Ok(())
                }
                Err(refused) => {
                    if let Some(loaded) = refused.plugin {
                        self.state = State::Loaded(loaded);
                    }
                    Err(refused.reason)
                }
            },
            active @ State::Active { .. } => {
                self.state = active;
                Ok(())
            }
            State::Gone => panic!("the unit is gone"),
        }
    }

    /// Rejoins the halves. No-op if not initialized. A unit that will not
    /// uninitialize stays active.
    pub fn uninitialize(&mut self) -> PluginResult<()> {
        match std::mem::replace(&mut self.state, State::Gone) {
            State::Active {
                processor,
                controller,
            } => match AuLoaded::deactivate(&main_thread(), processor.into_inner(), controller) {
                Ok(loaded) => {
                    self.state = State::Loaded(loaded);
                    Ok(())
                }
                Err(stuck) => {
                    self.state = State::Active {
                        processor: RefCell::new(stuck.processor),
                        controller: stuck.controller,
                    };
                    Err(stuck.reason)
                }
            },
            loaded @ State::Loaded(_) => {
                self.state = loaded;
                Ok(())
            }
            State::Gone => panic!("the unit is gone"),
        }
    }

    /// Runs `change` with the unit deactivated, then activates it again if it
    /// was active (the unit comes back at whatever `prepare` is by then).
    fn while_loaded(&mut self, change: impl FnOnce(&mut Self) -> PluginResult<()>) -> PluginResult<()> {
        let was_active = self.is_initialized();
        if was_active {
            self.uninitialize()?;
        }
        let result = change(self);
        if was_active {
            self.initialize()?;
        }
        result
    }

    /// A new sample rate: a rejected one leaves the unit on the rate it was on.
    pub fn set_sample_rate(&mut self, rate: f64) -> PluginResult<()> {
        let wanted = self
            .prepare
            .with_rate(SampleRate(rate))
            .ok_or_else(|| PluginError::Other(format!("{rate} is not a rate")))?;
        let before = self.prepare;
        self.prepare = wanted;
        let result = self.while_loaded(|_| Ok(()));
        if result.is_err() {
            // The activation refused: the unit is back on the old
            // configuration, and the next one carries it.
            self.prepare = before;
            let _ = self.initialize();
        }
        result
    }

    /// A new block ceiling. Zero is refused.
    pub fn set_block_size(&mut self, frames: u32) -> PluginResult<()> {
        let Some(block) = MaxBlock::new(Samples(frames as usize)) else {
            return Err(PluginError::Other("block_size must be non-zero".into()));
        };
        if frames == self.block_size() {
            return Ok(());
        }
        let before = self.prepare;
        self.prepare = Prepare::new(self.prepare.rate(), block)
            .expect("the rate was usable")
            .with_mode(self.prepare.mode());
        let result = self.while_loaded(|_| Ok(()));
        if result.is_err() {
            self.prepare = before;
            let _ = self.initialize();
        }
        result
    }

    /// Writes the render mode the way the host does: the next activation carries
    /// it. Returns whether *this unit* accepted the property.
    pub fn set_offline_render_bracketed(&mut self, offline: bool) -> PluginResult<bool> {
        self.prepare = self.prepare.with_mode(if offline {
            RenderMode::Offline
        } else {
            RenderMode::Realtime
        });
        self.while_loaded(|_| Ok(()))?;
        Ok(self.render_mode_accepted())
    }

    fn uninitialized(function: &'static str) -> AuError {
        AuError::OsStatus {
            function,
            code: K_AUDIO_UNIT_ERR_UNINITIALIZED,
        }
    }

    /// See [`AuProcessor::render_f32`]; an uninitialized unit refuses.
    pub fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        num_frames: u32,
    ) -> tutti_au_host::Result<()> {
        match &self.state {
            State::Active { processor, .. } => {
                processor.borrow_mut().render_f32(input, output, num_frames)
            }
            _ => Err(Self::uninitialized("Au::process")),
        }
    }

    /// See [`AuProcessor::render_f64`]; an uninitialized unit refuses.
    pub fn process_f64(
        &mut self,
        input: &[&[f64]],
        output: &mut [&mut [f64]],
        num_frames: u32,
    ) -> tutti_au_host::Result<()> {
        match &self.state {
            State::Active { processor, .. } => {
                processor.borrow_mut().render_f64(input, output, num_frames)
            }
            _ => Err(Self::uninitialized("Au::process_f64")),
        }
    }

    /// Clears the unit's audio state. Legal before initialization too, where it
    /// is a no-op in practice (see `au_notification.rs`); while active the
    /// Processor's flush also restarts its render clock.
    pub fn reset(&mut self) -> tutti_au_host::Result<()> {
        match &self.state {
            State::Active { processor, .. } => processor.borrow_mut().flush(),
            _ => Ok(self.unit().reset(Scope::GLOBAL, Element::ZERO)?),
        }
    }

    /// See [`AuProcessor::send_midi`]. Only an active unit has the SysEx room
    /// and the render path; a loaded one takes nothing.
    pub fn send_midi(&self, events: &[tutti_graph::UmpAt]) {
        if let State::Active { processor, .. } = &self.state {
            processor.borrow_mut().send_midi(events);
        }
    }

    /// See [`AuProcessor::sysex_dropped`]; zero before activation.
    pub fn sysex_dropped(&self) -> u64 {
        match &self.state {
            State::Active { processor, .. } => processor.borrow().sysex_dropped(),
            _ => 0,
        }
    }

    /// See [`AuController::process_push`]; an uninitialized unit refuses.
    pub fn process_push(
        &mut self,
        scratch: &mut PushScratch,
        frames: u32,
    ) -> tutti_au_host::Result<AudioUnitRenderActionFlags> {
        match &mut self.state {
            State::Active { controller, .. } => controller.process_push(scratch, frames),
            _ => Err(Self::uninitialized("Au::process_push")),
        }
    }

    /// See [`AuController::process_push_multiple`]; an uninitialized unit refuses.
    pub fn process_push_multiple(
        &mut self,
        scratch: &mut PushScratch,
        frames: u32,
    ) -> tutti_au_host::Result<AudioUnitRenderActionFlags> {
        match &mut self.state {
            State::Active { controller, .. } => controller.process_push_multiple(scratch, frames),
            _ => Err(Self::uninitialized("Au::process_push_multiple")),
        }
    }

    /// The state as bytes ([`Control::save_state`]).
    pub fn get_state(&mut self, main: &tutti_plugin_types::MainThread) -> PluginResult<Vec<u8>> {
        Control::save_state(&mut **self, main)
    }

    /// Restores state ([`Control::load_state`]).
    pub fn set_state(
        &mut self,
        main: &tutti_plugin_types::MainThread,
        data: &[u8],
    ) -> PluginResult<()> {
        Control::load_state(&mut **self, main, data)
    }

    /// How many times the input render callback was installed on the current
    /// activation (0 when not active).
    pub fn callback_install_count(&self) -> u32 {
        match &self.state {
            State::Active { processor, .. } => processor.borrow().input_callback_installs(),
            _ => 0,
        }
    }

    /// The halves, for a test that drives the traits itself.
    pub fn halves(&mut self) -> Option<(&mut AuProcessor, &mut AuController)> {
        match &mut self.state {
            State::Active {
                processor,
                controller,
            } => Some((processor.get_mut(), controller)),
            _ => None,
        }
    }
}

impl Deref for Au {
    type Target = AuLoaded;

    fn deref(&self) -> &AuLoaded {
        match &self.state {
            State::Loaded(loaded) => loaded,
            State::Active { controller, .. } => controller,
            State::Gone => panic!("the unit is gone"),
        }
    }
}

impl DerefMut for Au {
    fn deref_mut(&mut self) -> &mut AuLoaded {
        match &mut self.state {
            State::Loaded(loaded) => loaded,
            State::Active { controller, .. } => controller,
            State::Gone => panic!("the unit is gone"),
        }
    }
}

impl std::fmt::Debug for Au {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Au")
            .field("initialized", &self.is_initialized())
            .finish_non_exhaustive()
    }
}
