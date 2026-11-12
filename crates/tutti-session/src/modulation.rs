//! Modulation in the session (feature `modulation`; design 015 §4.4, §4.6):
//! the reconcile loop of doc 014 §8.2, run by [`Phase::Modulation`] and
//! [`Phase::AfterCommit`](crate::Phase::AfterCommit), over either the
//! session's own declarations ([`Session::mods`](crate::Session::mods)) or a
//! host's ([`ModInput`](crate::ModInput)).
//!
//! The loop, per update:
//!
//! 1. **Apply** (`Modulation`): the declarations onto the editor. A program
//!    edit republishes its lanes and stages nothing; a new lane, a signal
//!    node or a moved edge is staged for the commit.
//! 2. **Commit** (`Commit`).
//! 3. **Re-apply** (`AfterCommit`): a native target resolves only once its
//!    node is in a committed plan, so while a target is unresolved and the
//!    graph is otherwise clean, the declarations apply again and what that
//!    built is committed, so the lane plays from the update its node does.
//! 4. **Resync** (`AfterCommit`): the note-envelope resyncs the apply owes,
//!    once the commit that placed their nodes is out.
//!
//! A refusal changes nothing in the graph; it is reported once, when it
//! starts or changes, as [`SessionEvent::ModulationRefused`], keyed by the
//! host's own keys. A target that stays unresolved for
//! [`SessionBuilder::unresolved_warning`](crate::SessionBuilder::unresolved_warning)
//! (4 s by default) of [`PhaseCx::now`](crate::PhaseCx::now) is logged and
//! reported once ([`SessionEvent::ModulationUnresolved`]).
//!
//! [`Phase::Modulation`]: crate::Phase::Modulation
//! [`SessionEvent::ModulationRefused`]: crate::SessionEvent::ModulationRefused
//! [`SessionEvent::ModulationUnresolved`]: crate::SessionEvent::ModulationUnresolved

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tutti_graph::{EventOut, NodeKey};
use tutti_modulation::{
    Curve, Declarations, Follower, Lfo, Macro, NoteEnvelope, RouteDecl, RouteKey, RouteTarget,
    Source, SourceKey, TargetDecl, TargetKey,
};
use tutti_nodes::automation::{LaneControls, RecordMode, Recorder};
use tutti_nodes::{Feed, ReconcileError};
use tutti_types::{Depth, ParamAddr, UnitAddr, UnitParam};

use crate::Edge;
use crate::ModKeys;
use crate::ParamValue;
use crate::RangeError;
use crate::graph::Graph;

/// How long a modulation target may stay unresolved before it is logged:
/// today's 240 frames at 60 Hz (design 015 §4.4). A target waits an update
/// or two for its node's commit, and a plugin's for its values' read; one
/// still waiting after this long is likely never to resolve (a key no node
/// is ever bound to).
pub const UNRESOLVED_WARNING: Duration = Duration::from_secs(4);

/// One modulatable parameter of a node in the session's graph: the session's
/// own `ParamRef` (Bevy's names an entity).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ParamRef {
    /// The node with the param.
    pub node: NodeKey,
    /// The param's address on that node.
    pub param: ParamAddr,
}

impl ParamRef {
    /// `param` on `node`.
    pub fn new(node: impl Into<NodeKey>, param: impl Into<ParamAddr>) -> Self {
        let node = node.into();
        Self {
            node,
            param: param.into(),
        }
    }

    /// A native param of `node`, at index 0.
    pub fn unit(node: impl Into<NodeKey>, param: UnitParam) -> Self {
        Self {
            node: node.into(),
            param: ParamAddr::Unit(param),
        }
    }

    /// A native param of `node` at `index` among the node's params of that
    /// kind (a multiband compressor's threshold of band `index`).
    /// [`unit`](Self::unit) is index 0.
    pub fn unit_at(node: impl Into<NodeKey>, param: UnitParam, index: u8) -> Self {
        Self {
            node: node.into(),
            param: ParamAddr::unit(UnitAddr::new(param, index)),
        }
    }

    /// Parameter `id` of the plugin hosted on `node`.
    pub fn plugin(node: impl Into<NodeKey>, id: u32) -> Self {
        Self {
            node: node.into(),
            param: ParamAddr::Id(id),
        }
    }

    /// The reconciler's key: the node's key bits and the param. What a
    /// refusal of the session's own declarations names.
    #[must_use]
    pub fn key(self) -> TargetKey {
        TargetKey::param(self.node.0, self.param)
    }

    /// The `ParamRef` a key of the session's own names; `None` for the key
    /// of an edge ([`Edge`]) or a mix ([`MixRef`]).
    #[must_use]
    pub fn from_key(target: TargetKey) -> Option<Self> {
        match target {
            TargetKey::Param { node, param } => Some(Self {
                node: NodeKey(node),
                param,
            }),
            TargetKey::Edge(_) | TargetKey::Mix(_) => None,
        }
    }
}

/// The mix of a node in the session's graph (doc 013, "Bypass and mute are
/// graph state"): the node's blend with its aligned input, which
/// [`Graph::set_mix`] sets and a modulation route onto it ([`ModTarget`])
/// drives. Its range is `0..=1`: a route's depth is a fraction of the range
/// declared for it (`Mods::range`), and the value is clamped to the blend.
/// The mix is the lane's knob (one cell), so a route modulates around the mix
/// the graph holds and removing it returns to that mix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MixRef {
    /// The node whose mix it is.
    pub node: NodeKey,
}

impl MixRef {
    /// The mix of `node`.
    #[must_use]
    pub fn of(node: impl Into<NodeKey>) -> Self {
        Self { node: node.into() }
    }

    /// The reconciler's key: the node's key bits. What a refusal of the
    /// session's own declarations names.
    #[must_use]
    pub fn key(self) -> TargetKey {
        TargetKey::mix(self.node.0)
    }
}

/// What a modulation route, range or automation names: a param of a node
/// ([`ParamRef`]), the gain of an edge ([`Edge`]) or the mix of a node
/// ([`MixRef`]). Each converts: `mods.route(source, param_ref, depth)`,
/// `mods.route(source, edge_ref, depth)` and `mods.route(source, mix_ref,
/// depth)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModTarget {
    /// A param of a node.
    Param(ParamRef),
    /// The gain of an edge.
    Edge(Edge),
    /// The mix of a node.
    Mix(MixRef),
}

impl From<MixRef> for ModTarget {
    fn from(m: MixRef) -> Self {
        Self::Mix(m)
    }
}

impl From<ParamRef> for ModTarget {
    fn from(p: ParamRef) -> Self {
        Self::Param(p)
    }
}

impl<U> From<crate::TypedParamRef<U>> for ModTarget {
    fn from(p: crate::TypedParamRef<U>) -> Self {
        Self::Param(p.into())
    }
}

impl From<Edge> for ModTarget {
    fn from(e: Edge) -> Self {
        Self::Edge(e)
    }
}

/// A modulation target whose value is in a unit tutti knows: what
/// [`Mods::range`] takes a range of. A [`TypedParamRef`](crate::TypedParamRef) (the unit its node
/// declares), an [`Edge`] ([`Amplitude`](tutti_types::Amplitude)) and a
/// [`MixRef`] ([`Mix`](tutti_types::Mix)). Sealed.
pub trait Ranged: Into<ModTarget> + ranged::Sealed {
    /// The unit of the target's value.
    type Unit: crate::params::Scalar;
}

mod ranged {
    pub trait Sealed {}
}

impl<U: crate::params::Scalar> ranged::Sealed for crate::TypedParamRef<U> {}
impl<U: crate::params::Scalar> Ranged for crate::TypedParamRef<U> {
    type Unit = U;
}
impl ranged::Sealed for Edge {}
impl Ranged for Edge {
    type Unit = tutti_types::Amplitude;
}
impl ranged::Sealed for MixRef {}
impl Ranged for MixRef {
    type Unit = tutti_types::Mix;
}

/// Why [`Mods::route`] refused a route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RouteError {
    /// `target` has no range: a route's depth is a fraction of one, so
    /// declare it with [`Mods::range`] before routing into it.
    #[error("cannot route into {target:?}: it has no range (declare one with `Mods::range` first)")]
    UndeclaredRange {
        /// The target the route named.
        target: ModTarget,
    },
}

/// Why the session refused a host's modulation, in the host's keys (design
/// 015 §4.7): a headless host compares them with the keys
/// [`Session::mods`](crate::Session::mods) returned; Bevy decodes them to
/// its route or target entity.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ModRefusal {
    /// The declarations do not apply (doc 014 §8.2). The graph keeps what
    /// the last apply that succeeded built.
    Apply(ReconcileError),
    /// A host passed its declarations ([`ModInput`](crate::ModInput)) while
    /// [`Session::mods`](crate::Session::mods) holds some: a session has one
    /// declarer. Nothing applied.
    TwoDeclarers,
}

impl ModRefusal {
    /// The route it names: a route whose follower is downstream of its own
    /// target, or one the compile refused.
    pub fn route(&self) -> Option<RouteKey> {
        use tutti_modulation::CompileError as C;
        match self {
            Self::Apply(ReconcileError::Cycle { route })
            | Self::Apply(ReconcileError::Compile(
                C::DuplicateRoute(route)
                | C::BadRoute(route)
                | C::UnknownSource { route }
                | C::UnknownRoute { route }
                | C::UndeclaredTarget { route }
                | C::DepthByChain { route }
                | C::DepthBySignal { route }
                | C::TwoDepthBy { route },
            )) => Some(*route),
            _ => None,
        }
    }

    /// The target it names.
    pub fn target(&self) -> Option<TargetKey> {
        use tutti_modulation::CompileError as C;
        match self {
            Self::Apply(
                ReconcileError::WrongTarget(t)
                | ReconcileError::NoSuchParam(t)
                | ReconcileError::NoSuchEventInput(t)
                | ReconcileError::PortTaken(t)
                | ReconcileError::SharedPort { first: t, .. }
                | ReconcileError::Compile(
                    C::DuplicateTarget(t)
                    | C::NanRange(t)
                    | C::LiveCurve(t)
                    | C::LayerIdCollision(t)
                    | C::TooManyLayers { target: t, .. }
                    | C::TooManyInputs { target: t, .. },
                ),
            ) => Some(*t),
            _ => None,
        }
    }

    /// The source it names.
    pub fn source(&self) -> Option<SourceKey> {
        use tutti_modulation::CompileError as C;
        match self {
            Self::Apply(
                ReconcileError::WrongFeed(s)
                | ReconcileError::Compile(
                    C::DuplicateSource(s) | C::BadSource(s) | C::AnchorDeclared(s),
                ),
            ) => Some(*s),
            _ => None,
        }
    }
}

impl std::fmt::Display for ModRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Apply(e) => e.fmt(f),
            Self::TwoDeclarers => f.write_str(
                "a host passed its modulation while the session holds its own: one declarer",
            ),
        }
    }
}

impl std::error::Error for ModRefusal {}

/// The targets of a set of declarations, as `tutti_modulation` takes them, from a
/// host's ranges and automations: a range and an automation on one target
/// make one [`TargetDecl`]; an automation with no range is unclamped
/// (`±∞`); an automation whose curve is `None` (a recording take is open on
/// it) declares the target with no automation, so its knob plays.
///
/// **A target takes one range and one automation.** A second of either is
/// never merged (which one won would be the host's iteration order): both go
/// in, and the compile refuses the pair as
/// `CompileError::DuplicateTarget`, the one refusal path.
pub fn declare_targets(
    ranges: impl IntoIterator<Item = (TargetKey, f32, f32)>,
    automation: impl IntoIterator<Item = (TargetKey, Option<Arc<dyn Curve>>)>,
) -> Vec<(TargetKey, TargetDecl)> {
    let mut targets: BTreeMap<TargetKey, Vec<TargetDecl>> = BTreeMap::new();
    for (key, min, max) in ranges {
        targets.entry(key).or_default().push(TargetDecl {
            min,
            max,
            automation: None,
        });
    }
    let mut automated = BTreeSet::new();
    for (key, curve) in automation {
        let decls = targets.entry(key).or_default();
        if automated.insert(key) {
            match decls.first_mut() {
                Some(decl) => decl.automation = curve,
                None => decls.push(unclamped(curve)),
            }
        } else {
            // A second automation: its own entry, which the compile refuses.
            decls.push(unclamped(curve));
        }
    }
    targets
        .into_iter()
        .flat_map(|(key, decls)| decls.into_iter().map(move |d| (key, d)))
        .collect()
}

fn unclamped(automation: Option<Arc<dyn Curve>>) -> TargetDecl {
    TargetDecl {
        min: f32::NEG_INFINITY,
        max: f32::INFINITY,
        automation,
    }
}

/// A recording take on an automated target (doc 014 §8.2, "Recording").
/// While it is open the target declares no automation, so its knob (the
/// gesture) drives it; when it is done, its trajectory becomes the target's
/// automation.
///
/// It is done once [`close`](Self::close)d, or, in a mode that stops on
/// release ([`RecordMode::Touch`]), once the recorder is released after a
/// touch. Feed the recorder through [`recorder_mut`](Self::recorder_mut) as
/// the gesture moves.
#[derive(Debug)]
pub struct Take {
    recorder: Recorder,
    touched: bool,
    closed: bool,
}

impl Take {
    /// A take recording into `recorder` (a fresh one, or
    /// `Recorder::overdub` over the lane's trajectory).
    #[must_use]
    pub fn new(recorder: Recorder) -> Self {
        Self {
            recorder,
            touched: false,
            closed: false,
        }
    }

    /// A fresh `mode` take over `min..=max`.
    pub fn record(mode: RecordMode, min: f32, max: f32) -> Self {
        Self::new(Recorder::new(mode, min, max))
    }

    /// The recorder.
    pub fn recorder(&self) -> &Recorder {
        &self.recorder
    }

    /// The recorder, to touch, record and release.
    pub fn recorder_mut(&mut self) -> &mut Recorder {
        &mut self.recorder
    }

    /// Ends the take: on the next modulation phase its trajectory becomes the
    /// automation.
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// Whether the take is done: closed, or a touch take released after a
    /// touch. Notes a touch in progress, so call it once per update.
    pub fn is_done(&mut self) -> bool {
        if self.recorder.is_taking() {
            self.touched = true;
        }
        self.closed
            || (self.touched
                && !self.recorder.is_taking()
                && self.recorder.mode().stops_on_release())
    }

    /// The finished trajectory, as the automation curve it becomes.
    #[must_use]
    pub fn finish(self) -> Arc<dyn Curve> {
        Arc::new(self.recorder.finish())
    }
}

/// The session's modulation as applied: the reconciler, and what its last
/// apply left unresolved or refused. Read it with
/// [`Session::mod_state`](crate::Session::mod_state).
#[derive(Debug)]
pub struct ModState {
    modulation: tutti_nodes::Modulation,
    refusal: Option<ModRefusal>,
    unresolved_targets: Vec<TargetKey>,
    unresolved_sources: Vec<SourceKey>,
    /// Since when each unresolved target has been unresolved, and whether it
    /// was reported: the warning's clock.
    unresolved_since: BTreeMap<TargetKey, (Instant, bool)>,
    warn_after: Duration,
    /// An apply has run: from then on every modulation phase applies, so
    /// taking every declaration away tears down what they built.
    applied: bool,
    /// The plugin targets the last successful apply resolved, by node and
    /// parameter: where a plugin's GUI edit goes (`Phase::Plugins`).
    plugin_targets: crate::graph::PluginTargets,
}

impl ModState {
    pub(crate) fn new(warn_after: Duration) -> Self {
        Self {
            modulation: tutti_nodes::Modulation::new(),
            refusal: None,
            unresolved_targets: Vec::new(),
            unresolved_sources: Vec::new(),
            unresolved_since: BTreeMap::new(),
            warn_after,
            applied: false,
            plugin_targets: BTreeMap::new(),
        }
    }

    /// The lanes' retired programs, freed (`Phase::Reclaim`).
    pub(crate) fn reclaim(&self) -> usize {
        self.modulation.reclaim()
    }

    /// Why the declarations are refused now; `None` while they apply.
    pub fn refusal(&self) -> Option<&ModRefusal> {
        self.refusal.as_ref()
    }

    /// `target`'s lane: its node and its controls (the knob, the range, the
    /// counters). `None` while it has none.
    pub fn lane(&self, target: TargetKey) -> Option<(NodeKey, &LaneControls)> {
        self.modulation.lane(target)
    }

    /// Writes `value` into `target`'s knob: a plugin parameter's lane knob,
    /// or a native param's node cell the lane reads. `false` when `target`
    /// has no lane with a knob.
    pub fn set_knob(&self, target: TargetKey, value: f32) -> bool {
        self.lane(target)
            .is_some_and(|(_, controls)| controls.set_knob(value))
    }

    /// The target of the lane on plugin parameter `id` at `node`, when the
    /// last apply resolved one.
    #[cfg(feature = "plugin")]
    pub(crate) fn plugin_target(&self, node: NodeKey, id: u32) -> Option<TargetKey> {
        self.plugin_targets.get(&(node, id)).copied()
    }

    /// Signal source `source`'s node (a follower's, a note envelope's).
    pub fn source_node(&self, source: SourceKey) -> Option<NodeKey> {
        self.modulation.source(source)
    }

    /// The targets the last apply could not resolve: no node, a node not
    /// committed yet, or a plugin's values not read yet.
    pub fn unresolved_targets(&self) -> &[TargetKey] {
        &self.unresolved_targets
    }

    /// The followers the last apply could not feed.
    pub fn unresolved_sources(&self) -> &[SourceKey] {
        &self.unresolved_sources
    }

    /// Note-envelope resyncs owed to the next `AfterCommit`.
    pub fn resyncs_owed(&self) -> usize {
        self.modulation.resyncs_owed()
    }

    /// Applies that found their inputs unchanged and did nothing
    /// (`tutti_nodes::Modulation::skipped`): a steady update's count moves.
    pub fn skipped_applies(&self) -> u64 {
        self.modulation.skipped()
    }

    /// Resyncs refused for good (a full queue), counted.
    pub fn resyncs_dropped(&self) -> u64 {
        self.modulation.resyncs_dropped()
    }

    /// Records an apply's outcome; returns whether it asks for a commit.
    fn applied(
        &mut self,
        applied: Result<(tutti_nodes::Applied, crate::graph::PluginTargets), ReconcileError>,
        now: Instant,
        out: &mut Vec<crate::SessionEvent>,
    ) -> bool {
        self.applied = true;
        match applied {
            Ok((applied, plugin_targets)) => {
                self.plugin_targets = plugin_targets;
                self.refusal = None;
                let warn_after = self.warn_after;
                let mut since = std::mem::take(&mut self.unresolved_since);
                self.unresolved_since = applied
                    .unresolved_targets
                    .iter()
                    .map(|&target| {
                        let (from, mut warned) = since.remove(&target).unwrap_or((now, false));
                        let waited = now.saturating_duration_since(from);
                        if !warned && waited >= warn_after {
                            warned = true;
                            tracing::warn!(
                                "modulation target {target:?} still unresolved after \
                                 {waited:?}: no node is bound to its key, or the plugin's \
                                 values were never read"
                            );
                            out.push(crate::SessionEvent::ModulationUnresolved { target, waited });
                        }
                        (target, (from, warned))
                    })
                    .collect();
                self.unresolved_targets = applied.unresolved_targets;
                self.unresolved_sources = applied.unresolved_sources;
                applied.commit
            }
            Err(error) => {
                self.refuse(ModRefusal::Apply(error), out);
                false
            }
        }
    }

    /// Holds `refusal`, reporting it when it starts or changes.
    fn refuse(&mut self, refusal: ModRefusal, out: &mut Vec<crate::SessionEvent>) {
        if self.refusal.as_ref() != Some(&refusal) {
            tracing::error!("modulation refused: {refusal}");
            self.refusal = Some(refusal.clone());
            out.push(crate::SessionEvent::ModulationRefused(refusal));
        }
    }
}

/// What feeds one of the session's own signal sources.
#[derive(Clone, Copy, Debug)]
enum OwnFeed {
    Audio(NodeKey),
    Events(NodeKey, u16),
}

#[derive(Debug)]
struct OwnSource {
    source: Source,
    feed: Option<OwnFeed>,
}

struct OwnAutomation {
    curve: Arc<dyn Curve>,
    take: Option<Take>,
}

impl std::fmt::Debug for OwnAutomation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnAutomation")
            .field("take", &self.take)
            .finish_non_exhaustive()
    }
}

/// The session's own declarations, made through [`Mods`].
#[derive(Default, Debug)]
pub(crate) struct OwnMods {
    /// The last key minted, sources and routes alike.
    minted: u64,
    sources: BTreeMap<SourceKey, OwnSource>,
    routes: BTreeMap<RouteKey, RouteDecl>,
    ranges: BTreeMap<TargetKey, (f32, f32)>,
    automation: BTreeMap<TargetKey, OwnAutomation>,
    /// The key the reconciler is given for each edge a target names (an
    /// edge has no node bits to be its key): minted on first use, kept for
    /// the edge's life as a target.
    edge_keys: BTreeMap<Edge, u64>,
    /// The inverse, for [`OwnKeys`].
    edge_refs: BTreeMap<u64, Edge>,
    /// The declarations as the reconciler takes them, built from the maps
    /// above on the first apply after they may have changed: cleared when
    /// [`Mods`] is handed out (whatever it then edits), when a take closes
    /// and when a node is forgotten.
    built: Option<OwnDeclarations>,
}

/// The session's own declarations as the reconciler takes them, built
/// from [`OwnMods`] and kept until they may change.
#[derive(Debug)]
pub(crate) struct OwnDeclarations {
    sources: Vec<(SourceKey, Source)>,
    routes: Vec<(RouteKey, RouteDecl)>,
    targets: Vec<(TargetKey, TargetDecl)>,
}

impl OwnDeclarations {
    fn borrow(&self) -> Declarations<'_> {
        Declarations {
            sources: &self.sources,
            routes: &self.routes,
            targets: &self.targets,
        }
    }
}

impl OwnMods {
    pub(crate) fn is_empty(&self) -> bool {
        self.sources.is_empty()
            && self.routes.is_empty()
            && self.ranges.is_empty()
            && self.automation.is_empty()
    }

    fn mint(&mut self) -> u64 {
        self.minted += 1;
        self.minted
    }

    /// `target`'s key for the reconciler: a param's is its node's bits and
    /// address, a mix's its node's bits, an edge's a key minted for it.
    fn key_of(&mut self, target: ModTarget) -> TargetKey {
        match target {
            ModTarget::Param(p) => p.key(),
            ModTarget::Mix(m) => m.key(),
            ModTarget::Edge(e) => {
                let key = match self.edge_keys.get(&e) {
                    Some(&key) => key,
                    None => {
                        let key = self.mint();
                        self.edge_keys.insert(e, key);
                        self.edge_refs.insert(key, e);
                        key
                    }
                };
                TargetKey::edge(key)
            }
        }
    }

    /// Closes the takes that are done: each one's trajectory becomes its
    /// target's automation.
    fn close_takes(&mut self) {
        for automation in self.automation.values_mut() {
            if automation.take.as_mut().is_some_and(Take::is_done)
                && let Some(take) = automation.take.take()
            {
                automation.curve = take.finish();
                // A take becomes done through `Mods` (closed, or
                // released), which already cleared it; cleared here
                // too so the build never depends on that.
                self.built = None;
            }
        }
    }

    /// The declarations may change: built again on the next apply.
    pub(crate) fn edited(&mut self) {
        self.built = None;
    }

    /// Builds the declarations if they may have changed since the last
    /// build ([`built`](Self::built) then holds them).
    fn build(&mut self) {
        if self.built.is_none() {
            self.built = Some(self.declarations());
        }
    }

    fn declarations(&self) -> OwnDeclarations {
        OwnDeclarations {
            sources: self
                .sources
                .iter()
                .map(|(&k, s)| (k, s.source.clone()))
                .collect(),
            routes: self.routes.iter().map(|(&k, r)| (k, *r)).collect(),
            targets: declare_targets(
                self.ranges.iter().map(|(&k, &(min, max))| (k, min, max)),
                self.automation.iter().map(|(&k, a)| {
                    // An open take: the knob (the gesture) drives the param.
                    (k, a.take.is_none().then(|| Arc::clone(&a.curve)))
                }),
            ),
        }
    }

    /// Forgets every declaration on `node`'s params: a removed node takes
    /// its ranges, automation and the routes into it with it.
    pub(crate) fn forget_node(&mut self, node: NodeKey) {
        self.built = None;
        let bits = node.0;
        // The edges that touched the node go with it, and their keys.
        let dead: BTreeSet<u64> = self
            .edge_refs
            .iter()
            .filter(|(_, e)| e.touches(node))
            .map(|(&k, _)| k)
            .collect();
        self.edge_refs.retain(|k, _| !dead.contains(k));
        self.edge_keys.retain(|_, k| !dead.contains(k));
        let gone = |t: &TargetKey| match t {
            TargetKey::Param { node: n, .. } | TargetKey::Mix(n) => *n == bits,
            TargetKey::Edge(k) => dead.contains(k),
        };
        self.ranges.retain(|t, _| !gone(t));
        self.automation.retain(|t, _| !gone(t));
        self.routes
            .retain(|_, r| !matches!(r.to, RouteTarget::Param(t) if gone(&t)));
    }
}

/// The session's own resolver: a target's key is its node's key bits, and a
/// follower's feed is the node it was declared with.
struct OwnKeys<'a>(&'a OwnMods);

impl ModKeys for OwnKeys<'_> {
    fn node(&self, target: u64) -> Option<NodeKey> {
        // A key no node has any more names one the editor does not hold:
        // the reconciler leaves its target unresolved.
        Some(NodeKey(target))
    }

    fn edge(&self, target: u64) -> Option<Edge> {
        self.0.edge_refs.get(&target).copied()
    }

    fn feed(&self, source: SourceKey) -> Option<Feed> {
        Some(match self.0.sources.get(&source)?.feed? {
            OwnFeed::Audio(node) => Feed::Audio(node),
            OwnFeed::Events(node, port) => Feed::Events(EventOut { node, port }),
        })
    }
}

/// The session's modulation: what it applied, and its own declarations.
#[derive(Debug)]
pub(crate) struct ModSession {
    pub(crate) state: ModState,
    pub(crate) own: OwnMods,
}

impl ModSession {
    pub(crate) fn new(warn_after: Duration) -> Self {
        Self {
            state: ModState::new(warn_after),
            own: OwnMods::default(),
        }
    }

    /// Applies the declarations: the host's, or with `None` the session's
    /// own. Returns whether the apply asks for a commit.
    fn apply(
        &mut self,
        graph: &mut Graph,
        host: Option<crate::ModInput<'_>>,
        now: Instant,
        out: &mut Vec<crate::SessionEvent>,
    ) -> bool {
        let applied = match host {
            Some(input) => {
                graph.modulate(&mut self.state.modulation, &input.declarations, input.keys)
            }
            None => {
                self.own.build();
                let own = self.own.built.as_ref().expect("built");
                graph.modulate(
                    &mut self.state.modulation,
                    &own.borrow(),
                    &OwnKeys(&self.own),
                )
            }
        };
        self.state.applied(applied, now, out)
    }

    /// Whether this phase has a declarer: `false` when a host passed its
    /// own while the session holds some (refused), or when there is nothing
    /// to apply and nothing was ever applied.
    fn declarer(
        &mut self,
        host: Option<crate::ModInput<'_>>,
        out: &mut Vec<crate::SessionEvent>,
    ) -> bool {
        if host.is_some() && !self.own.is_empty() {
            self.state.refuse(ModRefusal::TwoDeclarers, out);
            return false;
        }
        host.is_some() || !self.own.is_empty() || self.state.applied
    }

    /// `Phase::Modulation`: closes the session's own takes that are done,
    /// then applies.
    pub(crate) fn modulation_phase(
        &mut self,
        graph: &mut Graph,
        host: Option<crate::ModInput<'_>>,
        now: Instant,
        out: &mut Vec<crate::SessionEvent>,
    ) {
        if host.is_none() {
            self.own.close_takes();
        }
        if self.declarer(host, out) {
            self.apply(graph, host, now, out);
        }
    }

    /// `Phase::AfterCommit`'s re-apply: while a target is unresolved and
    /// the graph is otherwise clean (the update's own commit went out),
    /// applies again. Returns whether that asks for a commit.
    pub(crate) fn reapply(
        &mut self,
        graph: &mut Graph,
        host: Option<crate::ModInput<'_>>,
        now: Instant,
        out: &mut Vec<crate::SessionEvent>,
    ) -> bool {
        // Nothing to resolve while the update's own commit waits (refused
        // for now): its nodes are not in a plan yet.
        if self.state.unresolved_targets.is_empty() || graph.is_edited() {
            return false;
        }
        self.declarer(host, out) && self.apply(graph, host, now, out)
    }

    /// `Phase::AfterCommit`'s resyncs: sent on an update that owes them and
    /// has nothing left to commit.
    pub(crate) fn resync(&mut self, graph: &mut Graph) {
        if self.state.modulation.resyncs_owed() > 0 && !graph.is_edited() {
            graph.send_mod_resyncs(&mut self.state.modulation);
        }
    }
}

/// The session's own modulation declarations (design 015 §4.3, §5.1):
/// sources, routes, ranges, automation and recording takes, keyed by the
/// [`SourceKey`]s and [`RouteKey`]s it mints and by each target's
/// [`ParamRef`]. [`Session::mods`](crate::Session::mods) hands it out; the
/// next update applies what it holds.
///
/// A session has one declarer: while this holds anything, a host's
/// [`ModInput`](crate::ModInput) is refused
/// ([`ModRefusal::TwoDeclarers`]).
#[derive(Debug)]
pub struct Mods<'a> {
    pub(crate) own: &'a mut OwnMods,
}

impl Mods<'_> {
    fn add_source(&mut self, source: Source, feed: Option<OwnFeed>) -> SourceKey {
        let key = SourceKey(self.own.mint());
        self.own.sources.insert(key, OwnSource { source, feed });
        key
    }

    /// A source of any kind with no node to feed (an LFO, a macro).
    pub fn source(&mut self, source: impl Into<Source>) -> SourceKey {
        self.add_source(source.into(), None)
    }

    /// An LFO.
    pub fn lfo(&mut self, lfo: Lfo) -> SourceKey {
        self.source(lfo)
    }

    /// A macro at `value`.
    pub fn macro_(&mut self, value: f32) -> SourceKey {
        self.source(Macro(value))
    }

    /// A follower of every audio output of `input`.
    pub fn follower(&mut self, follower: Follower, input: impl Into<NodeKey>) -> SourceKey {
        let input = input.into();
        self.add_source(follower.into(), Some(OwnFeed::Audio(input)))
    }

    /// A note envelope on event output `port` of `midi` (a clip node, a
    /// live input).
    pub fn note_envelope(
        &mut self,
        envelope: NoteEnvelope,
        midi: impl Into<NodeKey>,
        port: u16,
    ) -> SourceKey {
        let midi = midi.into();
        self.add_source(envelope.into(), Some(OwnFeed::Events(midi, port)))
    }

    /// Replaces `key`'s source, keeping what feeds it: a macro's value, an
    /// LFO's rate. An edit of a time function republishes the lanes it
    /// reaches, with no commit and no phase reset. `false` when there is no
    /// such source.
    pub fn set_source(&mut self, key: SourceKey, source: impl Into<Source>) -> bool {
        match self.own.sources.get_mut(&key) {
            Some(s) => {
                s.source = source.into();
                true
            }
            None => false,
        }
    }

    /// Removes a source and the routes from it.
    pub fn remove_source(&mut self, key: SourceKey) -> bool {
        let had = self.own.sources.remove(&key).is_some();
        self.own.routes.retain(|_, r| r.source != key);
        had
    }

    /// A route from `source` into `target` at `depth` of its range, bipolar
    /// and linear ([`route_mut`](Self::route_mut) changes those).
    ///
    /// **`target` needs its [`range`](Self::range) first**: a route is a
    /// fraction of a range, and the apply refuses a route onto a target with
    /// none (and holds back every later edit with it), so the call refuses
    /// here instead.
    ///
    /// # Errors
    ///
    /// [`RouteError::UndeclaredRange`], naming `target`, when no range was
    /// declared for it; nothing is added.
    pub fn route(
        &mut self,
        source: SourceKey,
        target: impl Into<ModTarget>,
        depth: Depth,
    ) -> Result<RouteKey, RouteError> {
        let target = target.into();
        let target_key = self.own.key_of(target);
        if !self.own.ranges.contains_key(&target_key) {
            return Err(RouteError::UndeclaredRange { target });
        }
        let key = RouteKey(self.own.mint());
        let to = RouteTarget::Param(target_key);
        self.own.routes.insert(
            key,
            RouteDecl {
                source,
                to,
                depth,
                polarity: tutti_modulation::Polarity::Bipolar,
                curve: tutti_modulation::Law::Linear,
            },
        );
        Ok(key)
    }

    /// A route, to edit in place: its depth, polarity, curve or target.
    pub fn route_mut(&mut self, key: RouteKey) -> Option<&mut RouteDecl> {
        self.own.routes.get_mut(&key)
    }

    /// Removes a route.
    pub fn remove_route(&mut self, key: RouteKey) -> bool {
        self.own.routes.remove(&key).is_some()
    }

    /// `target`'s range, `range.start..=range.end` in its own unit: what a
    /// route's depth is a fraction of, and what the value is clamped to
    /// (both ends inclusive). Replaces the one it had; an edit is a store
    /// into the lane, never a plan.
    ///
    /// `target` is a [`Key`](crate::Key)'s param method (`lp.cutoff()`, in
    /// [`Hz`](tutti_types::Hz)), an [`Edge`] (its gain, in
    /// [`Amplitude`](tutti_types::Amplitude)) or a [`MixRef`] (in
    /// [`Mix`](tutti_types::Mix)), and the range is a range of that unit:
    /// `m.range(lp.cutoff(), Hz(100.0)..Hz(4000.0))?`. A plugin's param, which
    /// has no unit tutti knows, takes [`range_raw`](Self::range_raw).
    ///
    /// # Errors
    ///
    /// [`RangeError`] when a bound is NaN or the start is above the end;
    /// nothing is stored.
    pub fn range<T: Ranged>(
        &mut self,
        target: T,
        range: std::ops::Range<T::Unit>,
    ) -> Result<(), RangeError> {
        self.range_raw(target, range.start.to_cell(), range.end.to_cell())
    }

    /// [`range`](Self::range) for a target whose unit is not tutti's: a
    /// plugin's param ([`ParamRef::plugin`]), whose range is in whatever the
    /// plugin's own value is (normalized, usually), or any target given as
    /// bare numbers.
    ///
    /// # Errors
    ///
    /// As [`range`](Self::range).
    pub fn range_raw(
        &mut self,
        target: impl Into<ModTarget>,
        min: f32,
        max: f32,
    ) -> Result<(), RangeError> {
        if min.is_nan() || max.is_nan() {
            return Err(RangeError::NotANumber { min, max });
        }
        if min > max {
            return Err(RangeError::MinAboveMax { min, max });
        }
        let key = self.own.key_of(target.into());
        self.own.ranges.insert(key, (min, max));
        Ok(())
    }

    /// Takes `target`'s range away.
    pub fn clear_range(&mut self, target: impl Into<ModTarget>) -> bool {
        let key = self.own.key_of(target.into());
        self.own.ranges.remove(&key).is_some()
    }

    /// `target` follows `curve` (frozen: a trajectory) where it has a value,
    /// in place of its knob. Replaces the automation it had.
    pub fn automate(&mut self, target: impl Into<ModTarget>, curve: Arc<dyn Curve>) {
        let key = self.own.key_of(target.into());
        self.own
            .automation
            .insert(key, OwnAutomation { curve, take: None });
    }

    /// Takes `target`'s automation away (and any take on it).
    pub fn clear_automation(&mut self, target: impl Into<ModTarget>) -> bool {
        let key = self.own.key_of(target.into());
        self.own.automation.remove(&key).is_some()
    }

    /// Opens a recording take on `target`'s automation: until it is done
    /// the knob plays, then its trajectory replaces the automation. `false`
    /// (and the take dropped) when `target` has no automation.
    pub fn record(&mut self, target: impl Into<ModTarget>, take: Take) -> bool {
        let key = self.own.key_of(target.into());
        match self.own.automation.get_mut(&key) {
            Some(automation) => {
                automation.take = Some(take);
                true
            }
            None => false,
        }
    }

    /// The open take on `target`, to feed its recorder.
    pub fn take_mut(&mut self, target: impl Into<ModTarget>) -> Option<&mut Take> {
        let key = self.own.key_of(target.into());
        self.own
            .automation
            .get_mut(&key)
            .and_then(|a| a.take.as_mut())
    }

    /// The key the reconciler holds `target` under: what
    /// [`ModState::lane`] and [`ModRefusal::target`] speak. A param's is
    /// [`ParamRef::key`], a mix's [`MixRef::key`]; an edge's is minted on first
    /// use and kept.
    pub fn key(&mut self, target: impl Into<ModTarget>) -> TargetKey {
        self.own.key_of(target.into())
    }

    /// Whether nothing is declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.own.is_empty()
    }
}
