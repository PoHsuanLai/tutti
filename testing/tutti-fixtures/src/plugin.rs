//! What a plugin crate's tests share (feature `plugin`).

use std::fmt;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

use tutti_plugin_types::{Loaded, MainThread, PluginError, Prepare};

/// This process's main thread, claimed by the calling thread. Each test is its
/// own process under nextest, so the claim cannot be refused; a test that calls
/// it from two threads panics, which is the point.
pub fn main_thread() -> MainThread {
    MainThread::claim().expect("one test, one process")
}

/// How one format makes and unmakes an activation's two halves: the part of
/// [`Active`] that differs between the formats' hosts. [`Whole`] is the
/// adapter for any [`Loaded`]; a host whose activation is typed (VST3's
/// `activate_typed`) writes its own.
pub trait Activation {
    /// The plugin, loaded and not rendering.
    type Loaded;
    /// The realtime half.
    type Processor;
    /// The main-thread half.
    type Controller;

    /// Splits `loaded`; a refusal hands the plugin back when the format can.
    ///
    /// # Errors
    ///
    /// The plugin's refusal.
    #[allow(clippy::type_complexity, clippy::result_large_err)]
    fn activate(
        main: &MainThread,
        loaded: Self::Loaded,
        prepare: Prepare,
    ) -> Result<(Self::Processor, Self::Controller), (Option<Self::Loaded>, PluginError)>;

    /// Rejoins the halves.
    ///
    /// # Errors
    ///
    /// Why the halves would not rejoin.
    fn deactivate(
        main: &MainThread,
        processor: Self::Processor,
        controller: Self::Controller,
    ) -> Result<Self::Loaded, PluginError>;
}

/// The [`Activation`] of a [`Loaded`] plugin, through its own `activate` and
/// `deactivate`.
#[derive(Debug)]
pub struct Whole<L>(PhantomData<fn() -> L>);

impl<L: Loaded> Activation for Whole<L> {
    type Loaded = L;
    type Processor = L::Processor;
    type Controller = L::Controller;

    fn activate(
        main: &MainThread,
        loaded: L,
        prepare: Prepare,
    ) -> Result<(L::Processor, L::Controller), (Option<L>, PluginError)> {
        loaded
            .activate(main, prepare)
            .map_err(|refused| (refused.plugin, refused.reason))
    }

    fn deactivate(
        main: &MainThread,
        processor: L::Processor,
        controller: L::Controller,
    ) -> Result<L, PluginError> {
        L::deactivate(main, processor, controller).map_err(|stuck| stuck.reason)
    }
}

/// An activation's two halves held together, for the suites that drive one
/// plugin from a single thread.
///
/// The Processor and the Controller are separate values on purpose; most of
/// these suites want "the active plugin", so `Active` holds the pair and derefs
/// to the Controller. It does not deref to the loaded plugin, so a call meant
/// for the Controller cannot quietly skip what the Controller tells the
/// Processor. A format's own accessors are the host's tests' extension traits
/// over this type, not part of it.
pub struct Active<A: Activation> {
    pair: Option<(A::Processor, A::Controller)>,
    prepare: Prepare,
}

impl<A: Activation> fmt::Debug for Active<A>
where
    A::Processor: fmt::Debug,
    A::Controller: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Active")
            .field("pair", &self.pair)
            .field("prepare", &self.prepare)
            .finish()
    }
}

impl<A: Activation> Active<A> {
    /// Activates `loaded` at `prepare`; a refusal hands the plugin back when
    /// the format can.
    ///
    /// # Errors
    ///
    /// The plugin's refusal.
    #[allow(clippy::result_large_err)]
    pub fn activate(
        loaded: A::Loaded,
        prepare: Prepare,
    ) -> Result<Self, (Option<A::Loaded>, PluginError)> {
        A::activate(&main_thread(), loaded, prepare).map(|pair| Self {
            pair: Some(pair),
            prepare,
        })
    }

    /// The configuration the plugin was last activated at.
    pub fn prepare(&self) -> Prepare {
        self.prepare
    }

    fn halves(&self) -> &(A::Processor, A::Controller) {
        self.pair.as_ref().expect("an Active holds its pair")
    }

    fn halves_mut(&mut self) -> &mut (A::Processor, A::Controller) {
        self.pair.as_mut().expect("an Active holds its pair")
    }

    /// The realtime half.
    pub fn processor(&mut self) -> &mut A::Processor {
        &mut self.halves_mut().0
    }

    /// The realtime half, shared: for a counter or a flag read.
    pub fn processor_shared(&self) -> &A::Processor {
        &self.halves().0
    }

    /// The main-thread half.
    pub fn controller(&self) -> &A::Controller {
        &self.halves().1
    }

    /// The main-thread half, mutably.
    pub fn controller_mut(&mut self) -> &mut A::Controller {
        &mut self.halves_mut().1
    }

    /// Ends the activation: the plugin comes back deactivated.
    ///
    /// # Panics
    ///
    /// When the halves will not rejoin.
    #[must_use]
    pub fn deactivate(mut self) -> A::Loaded {
        let (processor, controller) = self.pair.take().expect("an Active holds its pair");
        A::deactivate(&main_thread(), processor, controller)
            .unwrap_or_else(|reason| panic!("the halves would not rejoin: {reason}"))
    }

    /// Deactivates and activates again at `wanted`, as a rate or block change
    /// does. A refusal activates again at the configuration it had, and is
    /// returned.
    ///
    /// # Errors
    ///
    /// The plugin's refusal of `wanted`; the activation is as it was.
    ///
    /// # Panics
    ///
    /// When the halves will not rejoin, or the plugin neither takes `wanted`
    /// nor hands itself back, nor takes the configuration it had.
    pub fn reconfigure(&mut self, wanted: Prepare) -> Result<(), PluginError> {
        let main = main_thread();
        let (processor, controller) = self.pair.take().expect("an Active holds its pair");
        let loaded = A::deactivate(&main, processor, controller)
            .unwrap_or_else(|reason| panic!("the halves would not rejoin: {reason}"));
        match A::activate(&main, loaded, wanted) {
            Ok(pair) => {
                self.pair = Some(pair);
                self.prepare = wanted;
                Ok(())
            }
            Err((plugin, reason)) => {
                let plugin = plugin.expect("the format hands the plugin back");
                match A::activate(&main, plugin, self.prepare) {
                    Ok(pair) => self.pair = Some(pair),
                    Err((_, again)) => {
                        panic!("the plugin refused the configuration it had: {again}")
                    }
                }
                Err(reason)
            }
        }
    }
}

impl<A: Activation> Deref for Active<A> {
    type Target = A::Controller;

    fn deref(&self) -> &A::Controller {
        self.controller()
    }
}

impl<A: Activation> DerefMut for Active<A> {
    fn deref_mut(&mut self) -> &mut A::Controller {
        self.controller_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin_types::{MaxBlock, SampleRate, Samples};

    /// A plugin that takes rates up to 50 kHz and hands itself back on refusal.
    struct Fake;

    #[derive(Debug, PartialEq)]
    struct Plugin;
    #[derive(Debug)]
    struct Half(f64);
    #[derive(Debug)]
    struct Other(f64);

    impl Activation for Fake {
        type Loaded = Plugin;
        type Processor = Half;
        type Controller = Other;

        fn activate(
            _: &MainThread,
            _: Plugin,
            prepare: Prepare,
        ) -> Result<(Half, Other), (Option<Plugin>, PluginError)> {
            let rate = prepare.rate().0;
            if rate > 50_000.0 {
                return Err((Some(Plugin), PluginError::Process(format!("no {rate} Hz"))));
            }
            Ok((Half(rate), Other(rate)))
        }

        fn deactivate(_: &MainThread, _: Half, _: Other) -> Result<Plugin, PluginError> {
            Ok(Plugin)
        }
    }

    fn at(rate: f64) -> Prepare {
        let block = MaxBlock::new(Samples(64)).expect("a block");
        Prepare::new(SampleRate(rate), block).expect("a rate")
    }

    /// A refused `reconfigure` reports the refusal and leaves the activation
    /// as it was: the previous configuration, both halves live.
    ///
    /// Mutation: dropping the re-activation of the previous configuration on
    /// refusal (`Ok(pair) => self.pair = Some(pair)` in the refusal arm of
    /// `reconfigure` made a no-op) leaves no pair, so the `processor` read
    /// below panics.
    #[test]
    fn a_refused_reconfigure_keeps_the_previous_activation() {
        let mut active = Active::<Fake>::activate(Plugin, at(48_000.0)).expect("activates");
        assert!(active.reconfigure(at(96_000.0)).is_err());
        assert_eq!(active.prepare().rate().0, 48_000.0);
        assert_eq!(active.processor().0, 48_000.0);
        assert_eq!(active.controller().0, 48_000.0);

        active.reconfigure(at(44_100.0)).expect("accepted");
        assert_eq!(active.prepare().rate().0, 44_100.0);
        assert_eq!(active.processor().0, 44_100.0);
        assert_eq!(active.deactivate(), Plugin);
    }
}
