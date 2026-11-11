//! [`AuController`]: the main-thread half of an initialized Audio Unit.
//!
//! It holds the unit's [`AuLoaded`] state and derefs to it, so every call
//! available before activation (parameters, state, presets, bypass, identity,
//! editor) stays reachable while audio runs. What it adds is what only an
//! initialized unit has: the live latency and tail, the range table shared with
//! the Processor, and the push render path.

#![cfg(target_os = "macos")]

use std::cell::Cell;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use tutti_plugin_types::{Controller, MainThread, PluginTail, Samples};
use tutti_rt::ArcLend;

use crate::error::Result;
use crate::instance::AuLoaded;
use crate::processor::AuProcessor;
use crate::ranges::RangeTable;
use crate::types::*;

/// The main-thread half of an activation. `Send`, not `Sync`.
pub struct AuController {
    pub(crate) loaded: AuLoaded,
    /// The range table the Processor lends once per block; republished when the
    /// unit says its parameter list moved.
    pub(crate) ranges: Arc<ArcLend<RangeTable>>,
    _not_sync: PhantomData<Cell<()>>,
}

impl std::fmt::Debug for AuController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuController")
            .field("loaded", &self.loaded)
            .finish_non_exhaustive()
    }
}

impl Deref for AuController {
    type Target = AuLoaded;

    fn deref(&self) -> &AuLoaded {
        &self.loaded
    }
}

impl DerefMut for AuController {
    fn deref_mut(&mut self) -> &mut AuLoaded {
        &mut self.loaded
    }
}

impl AuController {
    pub(crate) fn new(loaded: AuLoaded, ranges: Arc<ArcLend<RangeTable>>) -> Self {
        Self {
            loaded,
            ranges,
            _not_sync: PhantomData,
        }
    }

    /// Publishes the range table to the Processor after the unit's parameter
    /// list moved, and sweeps the tables it has finished with.
    pub(crate) fn republish_ranges(&mut self) {
        self.ranges.reclaim();
        self.ranges.publish(self.loaded.range_snapshot());
    }

    /// Renders one block through the push path ([`crate::offline`]), for a host
    /// that drives the unit as a pull-less source.
    ///
    /// # Errors
    /// The `OSStatus` of the render.
    pub fn process_push(
        &mut self,
        scratch: &mut crate::offline::PushScratch,
        frames: u32,
    ) -> Result<AudioUnitRenderActionFlags> {
        crate::offline::process_push(self.loaded.unit(), scratch, frames)
    }

    /// [`process_push`](Self::process_push) over every output bus.
    ///
    /// # Errors
    /// As [`process_push`](Self::process_push).
    pub fn process_push_multiple(
        &mut self,
        scratch: &mut crate::offline::PushScratch,
        frames: u32,
    ) -> Result<AudioUnitRenderActionFlags> {
        crate::offline::process_push_multiple(self.loaded.unit(), scratch, frames)
    }
}

impl Controller for AuController {
    type Loaded = AuLoaded;
    type Processor = AuProcessor;

    /// The latency the unit reports now. A refusal answers the last figure that
    /// was read.
    fn latency(&self, _main: &MainThread) -> Samples {
        self.loaded.latency_figure()
    }

    /// The tail the unit reports now: a refusal is `Unknown`, a real answer (the
    /// unit has no tail concept), never the previous figure.
    fn tail(&self, _main: &MainThread) -> PluginTail {
        self.loaded.tail_figure()
    }
}
