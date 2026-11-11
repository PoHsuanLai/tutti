use super::*;

/// A device reporting a buffer range is opened with a fixed callback size:
/// the preferred 512 when it fits, clamped to the range when not, and
/// never past the callback's own buffers; a device reporting none gets
/// its default buffer, and the spec says the size is unknown.
#[test]
fn a_device_range_gives_a_fixed_quantum() {
    use cpal::SupportedBufferSize::{Range, Unknown};
    assert_eq!(
        quantum_for(&Range { min: 64, max: 4096 }),
        Some(Samples(512))
    );
    assert_eq!(
        quantum_for(&Range {
            min: 1024,
            max: 4096
        }),
        Some(Samples(1024))
    );
    assert_eq!(
        quantum_for(&Range { min: 16, max: 256 }),
        Some(Samples(256))
    );
    assert_eq!(quantum_for(&Unknown), None);
    let spec = OutputSpec::new(
        SampleRate(48_000.0),
        ChannelLayout::STEREO,
        cpal::SampleFormat::F32,
    )
    .with_quantum(Samples(480));
    assert!(matches!(
        spec.stream_config().buffer_size,
        cpal::BufferSize::Fixed(480)
    ));
    let spec = OutputSpec::new(
        SampleRate(48_000.0),
        ChannelLayout::STEREO,
        cpal::SampleFormat::F32,
    );
    assert!(matches!(
        spec.stream_config().buffer_size,
        cpal::BufferSize::Default
    ));
}
