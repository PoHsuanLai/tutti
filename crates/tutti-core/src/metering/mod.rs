//! Real-time audio metering — amplitude, stereo analysis, CPU tracking.

mod amplitude;
mod atomic_lufs;
mod cpu;
mod handle;
mod manager;
mod rt;
mod stereo;

pub use amplitude::AtomicAmplitude;
pub use atomic_lufs::AtomicLufs;
pub use cpu::{CpuMeter, CpuMetrics};
pub use handle::MeteringHandle;
pub use manager::MeteringManager;
pub use rt::MeteringContext;
pub use stereo::{AtomicStereoAnalysis, StereoAnalysisSnapshot};
