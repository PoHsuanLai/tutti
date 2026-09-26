//! [`DriveUnit`]: the fixture for the `AudioUnit` modulation path.
//!
//! The capture and value-path suites pin what happens to an `AudioUnit`
//! bound through `spawn_audio_node` / `AudioGraphRes::insert`: its controls
//! are captured through the `ModTargetRegistry` before it goes in, and a
//! route reaches its own cell. Their fixture was `DistortionNode` until it
//! became a native graph node (its controls are a `ParamSet`, captured with
//! no registry entry). A unit of the suites' own keeps them on the path
//! they test, whichever engine nodes are ported next.

use std::sync::Arc;

use tutti_core::{AtomicF32, AudioUnit, BufferMut, BufferRef, Drive, Param, Setting, SignalFrame};
use tutti_nodes::{AtomicTarget, ModParams, ModTarget};
use tutti_types::{ParamAddr, UnitParam};

/// One input, one output: the input times its drive. `Drive` is its one
/// setting (`UnitParam::Drive`, through `AudioUnit::set`) and its one
/// control-rate target, mirrored into the cell it reads (as every
/// registry-modulated node's is); `Clone` shares the cell.
#[derive(Clone)]
pub struct DriveUnit {
    drive: Param<Drive>,
}

impl DriveUnit {
    /// A unit at `drive`.
    pub fn new(drive: f32) -> Self {
        Self {
            drive: Param::new(Drive(drive)),
        }
    }

    /// The drive cell the unit reads, shared with every clone.
    pub fn drive(&self) -> Arc<AtomicF32> {
        self.drive.as_atomic()
    }
}

impl AudioUnit for DriveUnit {
    fn inputs(&self) -> usize {
        1
    }

    fn outputs(&self) -> usize {
        1
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        output[0] = input[0] * self.drive.load().get();
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let d = self.drive.load().get();
        for i in 0..size {
            output.set_f32(0, i, input.at_f32(0, i) * d);
        }
    }

    fn isolate(&mut self) {
        self.drive.detach();
    }

    /// `UnitParam::Drive` through the settings ring, as a `Legacy` unit
    /// takes an `AudioParam`.
    fn set(&mut self, setting: Setting) {
        if let Some((UnitParam::Drive, v)) = tutti_core::unit_param::from_setting(&setting) {
            self.drive.store(Drive(v));
        }
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(1);
        out.set(0, input.at(0).scale(f64::from(self.drive.load().get())));
        out
    }

    fn get_id(&self) -> u64 {
        0x_4452_4956_4555_4E54 // "DRIVEUNT"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

impl ModParams for DriveUnit {
    fn mod_target(
        &self,
        p: ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn ModTarget>> {
        match p {
            ParamAddr::Unit(UnitParam::Drive) => Some(Arc::new(AtomicTarget::with_mirror(
                base,
                min,
                max,
                self.drive(),
            ))),
            _ => None,
        }
    }
}
