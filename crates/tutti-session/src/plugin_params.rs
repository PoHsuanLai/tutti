//! A hosted plugin's parameters as views:
//! [`Params`], [`Param`], [`TypedParam`] and [`ParamGesture`].
//!
//! Every one of them borrows the session through [`Plugin`](crate::Plugin), so
//! none can outlive what it names. The descriptors a [`Params`] shows are the
//! session's own, read at load and read again when the plugin announces a
//! session's own, read at load and read again when the plugin announces a
//! rescan ([`PluginChange::ParamsRescanned`](crate::PluginChange::ParamsRescanned),
//! [`ParamTitlesChanged`](crate::PluginChange::ParamTitlesChanged)); the read
//! is a job on the session's spawner and lands in [`Session::update`](crate::Session::update), which
//! takes `&mut Session`. So **a handle cannot be stale**: holding one across
//! an update is a borrow error, and what a host keeps across updates is the
//! `ParamId`, resolved again with [`Params::get`] (which refuses an id the
//! plugin stopped declaring). No generation counter is checked, because there
//! is no state to check it against.
//!
//! The kept id resolved again after an update:
//!
//! ```
//! # use tutti_session::Session;
//! # use tutti_graph::NodeKey;
//! # use tutti_plugin_types::ParamId;
//! # fn rescan(session: &mut Session, node: NodeKey) {
//! let id = ParamId::new(0);
//! let before = session.plugin(node).unwrap().params().get(id).unwrap().normalized();
//! let _events = session.update();
//! let after = session.plugin(node).unwrap().params().get(id).map(|p| p.normalized());
//! # let _ = (before, after);
//! # }
//! ```
//!
//! A handle held across the update is the stale one, and does not compile:
//!
//! ```compile_fail
//! # use tutti_session::Session;
//! # use tutti_graph::NodeKey;
//! # use tutti_plugin_types::{Normalized, ParamId};
//! # fn rescan(session: &mut Session, node: NodeKey) {
//! let level = session.plugin(node).unwrap().params().get(ParamId::new(0)).unwrap();
//! let _events = session.update();
//! let _ = level.set(Normalized::of(0.5));
//! # }
//! ```
//!
//! None of the calls takes the main thread: a value reaches the plugin through
//! its handle's token-free calls (a parameter set goes to the plugin's edit ring
//! from any thread), so the views belong on the read-only `Plugin<'_>`, which
//! `MainPlugin::view` also lends. A call that asks the plugin something
//! (`text`, `set_text`, `plain`, `set_plain`) may be a round trip to the
//! plugin, and for an in-process plugin an answer from its main thread, so a
//! host does not make one per frame per parameter.
//!
//! What the host sets reads back at once (`normalized` answers the value just
//! set) and is not reported as a [`PluginChange`](crate::PluginChange) unless it
//! is made inside a [`ParamGesture`]: a gesture is what a host that records
//! automation touches a parameter with, so the session reports the host's own
//! begin, changes and end in the one stream with the plugin's, in order.
//!
//! The gesture is the one owned handle ([`ParamGesture`]): a drag spans frames,
//! each of which calls `update` under `&mut Session`, so it cannot borrow. A
//! shared gate stands in for the borrow and refuses a `set` once the plugin is
//! gone.
//!
//! The gesture is the host's own. No hosted format has a call that tells a plugin
//! the host holds a parameter (CLAP's and VST3's gesture events go from the
//! plugin to the host, VST2 has none, AU's are notifications to listeners), so a
//! plugin hears only the values.

use tutti_graph::NodeKey;
use tutti_plugin_host::{Notice, ParamReporter, PluginHandle};
use tutti_plugin_types::{
    AutomationMode, AutomationModeError, Normalized, ParamEdit, ParamId, ParamUnit, ParameterInfo,
    PlainUnit,
};

use crate::plugin::{Gate, HostedPlugin, Notices, queue};

/// Why a call on a plugin's parameters did not do what it was asked.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum PluginParamError {
    /// The plugin's parameters have not been read yet (the read at load was
    /// not answered and its job has not landed). The session submits the
    /// read again in `Phase::Plugins` and lands it in a later update; the
    /// next [`Params::get`] after it landed answers.
    #[error("the plugin's parameters have not been read yet")]
    NotRead,
    /// The plugin declares no parameter with this id.
    #[error("the plugin declares no parameter {0}")]
    Unknown(ParamId),
    /// The parameter is declared read-only.
    #[error("parameter {0} is read-only")]
    ReadOnly(ParamId),
    /// The parameter declares no unit tutti types
    /// ([`ParameterInfo::measures`] is `None`), so a typed read cannot be made.
    #[error("parameter {0} declares no unit")]
    NoUnit(ParamId),
    /// The parameter's declared unit is not one the asked type reads.
    #[error("parameter {id} measures {declared:?}, which {wanted} does not read")]
    UnitMismatch {
        /// The parameter.
        id: ParamId,
        /// The unit it declares.
        declared: ParamUnit,
        /// The type asked for.
        wanted: &'static str,
    },
    /// The parameter's declared range holds values the asked type cannot
    /// (a bipolar percentage as a `Mix`), so its getter would not be total.
    #[error("the range parameter {id} declares in {declared:?} holds values {wanted} cannot")]
    RangeUnfit {
        /// The parameter.
        id: ParamId,
        /// The unit it declares.
        declared: ParamUnit,
        /// The type asked for.
        wanted: &'static str,
    },
    /// The plugin a [`ParamGesture`] began on is gone (removed, replaced by a
    /// crossfade or a reload) or stopped declaring the parameter (a rescan).
    #[error("the plugin of the gesture on parameter {0} is gone")]
    Gone(ParamId),
    /// The value lies outside the range the parameter declares, or is not
    /// finite.
    #[error("the value lies outside the range parameter {0} declares")]
    OutOfRange(ParamId),
    /// The format has no plain value for the parameter (VST2 without an integer
    /// range, a plugin that no longer answers).
    #[error("the format has no plain value for parameter {0}")]
    NoPlainValue(ParamId),
    /// The plugin could not read a value from the text.
    #[error("the plugin could not read a value of parameter {id} from {text:?}")]
    UnreadableText {
        /// The parameter.
        id: ParamId,
        /// The text it was handed.
        text: String,
    },
}

/// A hosted plugin's parameters, lent by [`Plugin::params`](crate::Plugin::params).
#[derive(Debug, Clone, Copy)]
pub struct Params<'a> {
    node: NodeKey,
    hosted: &'a HostedPlugin,
}

impl<'a> Params<'a> {
    pub(crate) fn new(node: NodeKey, hosted: &'a HostedPlugin) -> Self {
        Self { node, hosted }
    }

    /// Whether the plugin's parameters have been read. A plugin loaded through
    /// the session is read at load; one the host inserted itself
    /// ([`Graph::host_plugin`](crate::Graph::host_plugin)) is read by a job
    /// on the session's spawner, landed in [`Session::update`](crate::Session::update).
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.hosted.mirror.is_ready()
    }

    /// What the plugin declares, in the order it lists it. Empty until the
    /// parameters are [read](Self::is_ready).
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &'a ParameterInfo> + use<'a> {
        self.hosted.mirror.descriptors().iter()
    }

    /// How many parameters the plugin declares.
    #[must_use]
    pub fn len(&self) -> usize {
        self.hosted.mirror.descriptors().len()
    }

    /// Whether the plugin declares none, or they are not read yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Resolves parameter `id` once. The [`Param`] reads its value from the
    /// session's mirror and writes through the plugin's handle.
    ///
    /// # Errors
    ///
    /// [`PluginParamError::NotRead`] before the parameters are read;
    /// [`PluginParamError::Unknown`] for an id the plugin does not declare.
    pub fn get(&self, id: ParamId) -> Result<Param<'a>, PluginParamError> {
        if !self.is_ready() {
            return Err(PluginParamError::NotRead);
        }
        let position = self
            .hosted
            .mirror
            .position(id)
            .ok_or(PluginParamError::Unknown(id))?;
        Ok(Param {
            params: *self,
            position,
        })
    }

    /// Resolves parameter `id` as a tutti unit `U`: its declared
    /// [`ParamUnit`] is checked against `U` once, here, and
    /// [`TypedParam::get`] and [`TypedParam::set`] then go through the format's
    /// own value map, in `U`.
    ///
    /// # Errors
    ///
    /// As [`get`](Self::get), and [`PluginParamError::NoUnit`] for a parameter
    /// with no declared unit, [`PluginParamError::UnitMismatch`] for a unit `U`
    /// does not read (`typed::<Db>` on a parameter in hertz) and
    /// [`PluginParamError::RangeUnfit`] for a declared range `U` cannot hold
    /// all of.
    pub fn typed<U: PlainUnit>(&self, id: ParamId) -> Result<TypedParam<'a, U>, PluginParamError> {
        let param = self.get(id)?;
        let info = param.info();
        let wanted = std::any::type_name::<U>();
        let declared = info.measures().ok_or(PluginParamError::NoUnit(id))?;
        if !U::reads(declared) {
            return Err(PluginParamError::UnitMismatch {
                id,
                declared,
                wanted,
            });
        }
        // A unit is only declared over a plain range (`ParamUnit::fits`).
        let (min, max) = info.bounds().ok_or(PluginParamError::NoUnit(id))?;
        let unfit = PluginParamError::RangeUnfit {
            id,
            declared,
            wanted,
        };
        let default = U::from_plain(declared, info.default_value()).ok_or(unfit.clone())?;
        // Every type here is an interval: if both ends read, so does
        // everything between them.
        for end in [min, max] {
            U::from_plain(declared, end).ok_or(unfit.clone())?;
        }
        Ok(TypedParam {
            param,
            unit: declared,
            min,
            max,
            default,
        })
    }

    /// Announces the host's [`AutomationMode`] to the plugin so its editor can
    /// show it (a knob ring that glows while the host records). An advisory:
    /// `Ok` means it was handed over, not that the plugin reacted.
    ///
    /// # Errors
    ///
    /// [`AutomationModeError::Unsupported`] where the hosting carries no such
    /// channel (every in-process format), [`Dropped`](AutomationModeError::Dropped)
    /// when the push found the command queue full (the next change lands), and
    /// [`PluginDead`](AutomationModeError::PluginDead).
    pub fn set_automation_mode(&self, mode: AutomationMode) -> Result<(), AutomationModeError> {
        self.hosted.handle.set_automation_mode(mode)
    }

    /// The plugin's node.
    #[must_use]
    pub fn node(&self) -> NodeKey {
        self.node
    }
}

/// One parameter of a hosted plugin, resolved by [`Params::get`].
///
/// `Copy`, and good for as long as the borrow of the session it came from: see
/// the module docs for why that is as long as the parameter list can stay
/// as it is.
#[derive(Debug, Clone, Copy)]
pub struct Param<'a> {
    params: Params<'a>,
    /// Where the parameter is in the mirror's list: valid while the read that
    /// made it is the mirror's, which the borrow guarantees.
    position: usize,
}

impl<'a> Param<'a> {
    /// What the plugin declares about the parameter.
    #[must_use]
    pub fn info(&self) -> &'a ParameterInfo {
        &self.params.hosted.mirror.descriptors()[self.position]
    }

    /// The parameter's id.
    #[must_use]
    pub fn id(&self) -> ParamId {
        self.info().id()
    }

    /// The value now: what the plugin last reported or the host last set, else
    /// what was read. From the session's mirror, so it asks the plugin nothing.
    #[must_use]
    pub fn normalized(&self) -> Normalized {
        self.params.hosted.mirror.value_at(self.position)
    }

    /// The reference that targets this parameter in a modulation route
    /// (feature `modulation`): the one a native node's param method gives, so a
    /// route targets a plugin's parameter and a native one through one type.
    #[cfg(feature = "modulation")]
    #[must_use]
    pub fn target(&self) -> crate::ParamRef {
        crate::Key::<crate::PluginNode>::new(self.params.node).param(self.id())
    }

    fn writable(&self) -> Result<(), PluginParamError> {
        if self.info().flag(tutti_plugin_types::ParamFlags::READ_ONLY) == Some(true) {
            Err(PluginParamError::ReadOnly(self.id()))
        } else {
            Ok(())
        }
    }

    /// Sends `value` to the plugin and reads it back at once.
    fn send(&self, value: Normalized) {
        let hosted = self.params.hosted;
        hosted
            .handle
            .any_thread()
            .set_parameter_value(self.id(), value);
        hosted.mirror.report(self.id(), value);
    }

    /// Sets the parameter. Not a gesture: use [`edit`](Self::edit) for a value
    /// that moves under a hand.
    ///
    /// # Errors
    ///
    /// [`PluginParamError::ReadOnly`] for a parameter declared read-only.
    pub fn set(&self, value: Normalized) -> Result<(), PluginParamError> {
        self.writable()?;
        self.send(value);
        Ok(())
    }

    /// The plugin's own text for the value now ("-6.0 dB"); `None` when the
    /// plugin (or its format) has none.
    #[must_use]
    pub fn text(&self) -> Option<String> {
        self.params
            .hosted
            .handle
            .any_thread()
            .parameter_text(self.id(), self.normalized())
    }

    /// Sets the parameter from text the plugin reads ("-6 dB"), and returns
    /// the value it took.
    ///
    /// # Errors
    ///
    /// [`PluginParamError::ReadOnly`], and [`PluginParamError::UnreadableText`]
    /// when the plugin cannot read a value from `text`.
    pub fn set_text(&self, text: &str) -> Result<Normalized, PluginParamError> {
        self.writable()?;
        let value = self
            .params
            .hosted
            .handle
            .any_thread()
            .parameter_value_from_text(self.id(), text)
            .ok_or_else(|| PluginParamError::UnreadableText {
                id: self.id(),
                text: text.to_owned(),
            })?;
        self.send(value);
        Ok(value)
    }

    /// The value now in the plugin's own units, through the format's own map;
    /// `None` where the format has none.
    ///
    /// A bare `f64`, on purpose: a plugin's plain value has no tutti unit unless
    /// the plugin declares one, and [`Params::typed`] is the typed door for that.
    #[must_use]
    pub fn plain(&self) -> Option<f64> {
        self.params
            .hosted
            .handle
            .any_thread()
            .parameter_plain(self.id(), self.normalized())
    }

    /// Sets the parameter to `plain` in the plugin's own units (a bare `f64` for
    /// the reason [`plain`](Self::plain) gives), through the
    /// format's own map, and returns the position it took. A value past an end
    /// of the range takes that end (the map's law).
    ///
    /// # Errors
    ///
    /// [`PluginParamError::ReadOnly`], and [`PluginParamError::NoPlainValue`]
    /// where the format has no plain value for the parameter.
    pub fn set_plain(&self, plain: f64) -> Result<Normalized, PluginParamError> {
        self.writable()?;
        let value = self
            .params
            .hosted
            .handle
            .any_thread()
            .parameter_normalized(self.id(), plain)
            .ok_or(PluginParamError::NoPlainValue(self.id()))?;
        self.send(value);
        Ok(value)
    }

    /// Takes hold of the parameter: the session reports the host's gesture
    /// (a [`PluginChange::GestureBegan`](crate::PluginChange::GestureBegan) now,
    /// each [`set`](ParamGesture::set) as a
    /// [`ParamChanged`](crate::PluginChange::ParamChanged), and a
    /// [`GestureEnded`](crate::PluginChange::GestureEnded) when the guard
    /// drops, a drop in the middle of a drag included).
    ///
    /// # Errors
    ///
    /// [`PluginParamError::ReadOnly`]: a gesture on a parameter that cannot be
    /// set is refused before it begins, so the guard's `set` cannot fail.
    pub fn edit(&self) -> Result<ParamGesture, PluginParamError> {
        self.writable()?;
        let hosted = self.params.hosted;
        hosted.hear(ParamEdit::Begin(self.id()));
        Ok(ParamGesture {
            node: self.params.node,
            id: self.id(),
            gate: hosted.gate.clone(),
            handle: hosted.handle.clone(),
            notices: std::sync::Arc::clone(&hosted.notices),
            reporter: hosted.mirror.reporter(),
        })
    }
}

/// A host's hold on a parameter: made by [`Param::edit`], which begins the
/// gesture, and ended when it drops.
///
/// **It is the one handle that is owned, with no borrow of the session.** A drag
/// is a mouse down, many frames each calling [`Session::update`](crate::Session::update)
/// (which takes `&mut Session`), and a mouse up, so the guard has to live across
/// them; the other handles stay borrowed because a stale one is then a compile
/// error. What stands in for the borrow is a shared gate the session ends when
/// the plugin is removed, replaced (a crossfade, a reload) or stops declaring the
/// parameter (a rescan landed): [`set`](Self::set) then refuses with
/// [`PluginParamError::Gone`] and nothing is sent, and the drop sends nothing.
/// The notices go through the plugin's own queue, so the begin, the changes and
/// the end are reported in the order they were made, with the plugin's.
///
/// There is no way to end it but to drop it, so a drag that is cut short (an
/// early return, a panic unwinding) still lets go.
#[must_use = "the gesture ends when this drops: keep it for as long as the hand is down"]
#[derive(Debug)]
pub struct ParamGesture {
    node: NodeKey,
    id: ParamId,
    gate: Gate,
    handle: PluginHandle,
    notices: Notices,
    reporter: ParamReporter,
}

impl ParamGesture {
    /// The plugin's node.
    #[must_use]
    pub fn node(&self) -> NodeKey {
        self.node
    }

    /// The parameter held.
    #[must_use]
    pub fn id(&self) -> ParamId {
        self.id
    }

    /// Moves the parameter to `value`, inside the gesture: the plugin takes the
    /// value, and the session reports a
    /// [`ParamChanged`](crate::PluginChange::ParamChanged) for it.
    ///
    /// # Errors
    ///
    /// [`PluginParamError::Gone`] once the plugin was removed or replaced, or no
    /// longer declares the parameter.
    pub fn set(&self, value: Normalized) -> Result<(), PluginParamError> {
        if !self.gate.holds(self.id) {
            return Err(PluginParamError::Gone(self.id));
        }
        self.handle.any_thread().set_parameter_value(self.id, value);
        self.reporter.report(self.id, value);
        queue(
            &self.notices,
            Notice::Param(ParamEdit::change(self.id, value)),
        );
        Ok(())
    }
}

impl Drop for ParamGesture {
    fn drop(&mut self) {
        if self.gate.holds(self.id) {
            queue(&self.notices, Notice::Param(ParamEdit::End(self.id)));
        }
    }
}

/// One parameter of a hosted plugin in a tutti unit `U`: made by
/// [`Params::typed`], which checked the parameter's declared unit once.
#[derive(Debug, Clone, Copy)]
pub struct TypedParam<'a, U> {
    param: Param<'a>,
    unit: ParamUnit,
    min: f64,
    max: f64,
    /// The declared default, read at the check: what [`get`](Self::get) answers
    /// when the plugin answers no plain value.
    default: U,
}

impl<'a, U: PlainUnit> TypedParam<'a, U> {
    /// The untyped parameter, for its value in the plugin's own terms, its text
    /// and its gesture.
    #[must_use]
    pub fn param(&self) -> Param<'a> {
        self.param
    }

    /// The value now, in `U`: the mirror's position through the format's own
    /// map. A plugin that stops answering the map reads as the declared
    /// default.
    #[must_use]
    pub fn get(&self) -> U {
        self.param
            .plain()
            .map(|plain| plain.clamp(self.min, self.max))
            .and_then(|plain| U::from_plain(self.unit, plain))
            .unwrap_or(self.default)
    }

    /// Sets the parameter to `value`, through the declared range and the
    /// format's own map.
    ///
    /// # Errors
    ///
    /// [`PluginParamError::OutOfRange`] for a value outside the declared range,
    /// [`ReadOnly`](PluginParamError::ReadOnly), and
    /// [`NoPlainValue`](PluginParamError::NoPlainValue) where the format has no
    /// plain value.
    pub fn set(&self, value: U) -> Result<(), PluginParamError> {
        let plain = value.to_plain(self.unit);
        if !(self.min..=self.max).contains(&plain) {
            return Err(PluginParamError::OutOfRange(self.param.id()));
        }
        self.param.set_plain(plain).map(drop)
    }
}

impl HostedPlugin {
    /// Queues an edit the host made as if the plugin had reported it, for the
    /// next `Phase::Plugins`'s event, in the order it was made.
    pub(crate) fn hear(&self, edit: ParamEdit) {
        crate::plugin::queue(&self.notices, Notice::Param(edit));
    }
}
