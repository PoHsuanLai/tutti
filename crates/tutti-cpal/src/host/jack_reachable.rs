//! JACK, reached through cpal.

use super::{AudioHost, DeviceHost};
use crate::error::{Error, Result};

pub(super) fn open() -> Result<DeviceHost> {
    cpal::host_from_id(cpal::HostId::Jack)
        .map(|inner| DeviceHost {
            inner,
            id: AudioHost::Jack,
        })
        .map_err(|e| Error::HostUnavailable {
            host: AudioHost::Jack,
            reason: e.to_string(),
        })
}
