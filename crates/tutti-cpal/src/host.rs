//! Which platform host, and which device on it.
//!
//! The host has to be named: `cpal::default_host()` returns ALSA even when
//! cpal's `jack` dependency is compiled in. Every host is opened through
//! [`DeviceHost::open`], and every device is resolved by
//! [`DeviceHost::device`].

use cpal::traits::{DeviceTrait, HostTrait};

use crate::driver::DeviceInfo;
use crate::error::{Error, Result};
use crate::roster::{DeviceId, Direction, Snapshot};

/// Which platform audio host to open devices through.
///
/// Every variant exists on every platform, so a configuration struct holding
/// an `AudioHost` has the same shape on Linux, macOS and Windows (cpal's own
/// `HostId` is generated per target). Asking for a host this build cannot
/// reach fails at runtime with [`Error::HostUnavailable`]; use
/// [`available_hosts`] to list the ones that work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AudioHost {
    /// Whatever cpal calls the platform default: ALSA, WASAPI, CoreAudio.
    #[default]
    Default,
    /// JACK, on Linux and the BSDs.
    ///
    /// Needs this crate's `jack` feature. Elsewhere, or without the feature,
    /// opening it returns [`Error::HostUnavailable`].
    Jack,
}

impl std::fmt::Display for AudioHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioHost::Default => f.write_str("default"),
            AudioHost::Jack => f.write_str("JACK"),
        }
    }
}

/// Which device on a host to open.
///
/// A [`DeviceId`] is a name and an order among same-named devices, so a saved
/// selection survives a hot-plug of another card (a position would not). It
/// does not follow a device unplugged and replugged under a new name.
///
/// An id that matches nothing is an error ([`Error::InvalidDevice`]), never a
/// fallback to the default device.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum DeviceSelector {
    /// The host's default device.
    #[default]
    Default,
    /// A device by identity.
    Id(DeviceId),
}

impl DeviceSelector {
    /// The first device called `name`.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self::Id(DeviceId::new(name))
    }
}

impl From<DeviceId> for DeviceSelector {
    fn from(id: DeviceId) -> Self {
        Self::Id(id)
    }
}

/// An opened platform audio host, used to enumerate its devices.
pub struct DeviceHost {
    inner: cpal::Host,
    id: AudioHost,
}

impl std::fmt::Debug for DeviceHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `cpal::Host` is not Debug.
        f.debug_struct("DeviceHost").field("id", &self.id).finish()
    }
}

impl DeviceHost {
    /// Opens a platform host.
    ///
    /// # Errors
    /// [`Error::HostUnavailable`] when this build cannot reach `which` — the
    /// feature is off, the platform has no such host, or the server is not
    /// running.
    pub fn open(which: AudioHost) -> Result<Self> {
        match which {
            AudioHost::Default => Ok(Self {
                inner: cpal::default_host(),
                id: which,
            }),
            AudioHost::Jack => Self::open_jack(),
        }
    }

    #[cfg(all(
        feature = "jack",
        any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd"
        )
    ))]
    fn open_jack() -> Result<Self> {
        cpal::host_from_id(cpal::HostId::Jack)
            .map(|inner| Self {
                inner,
                id: AudioHost::Jack,
            })
            .map_err(|e| Error::HostUnavailable {
                host: AudioHost::Jack,
                reason: e.to_string(),
            })
    }

    #[cfg(not(all(
        feature = "jack",
        any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd"
        )
    )))]
    fn open_jack() -> Result<Self> {
        Err(Error::HostUnavailable {
            host: AudioHost::Jack,
            reason: "built without the `jack` feature, or not a JACK platform".into(),
        })
    }

    /// Returns which host this is.
    pub fn id(&self) -> AudioHost {
        self.id
    }

    /// Lists this host's output devices.
    ///
    /// A device whose name cannot be read is listed with an empty name.
    ///
    /// # Errors
    /// [`Error::DevicesError`] if the host cannot enumerate.
    pub fn output_devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(DeviceInfo::list(
            self.inner
                .output_devices()?
                .map(|d| d.name().unwrap_or_default()),
        ))
    }

    /// Lists this host's input devices, as
    /// [`output_devices`](Self::output_devices) does.
    ///
    /// # Errors
    /// [`Error::DevicesError`] if the host cannot enumerate.
    pub fn input_devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(DeviceInfo::list(
            self.inner
                .input_devices()?
                .map(|d| d.name().unwrap_or_default()),
        ))
    }

    /// Every device and both defaults, in one look: what
    /// [`DevicePoller`](crate::DevicePoller) publishes. Blocks as long as the
    /// host's enumeration does.
    ///
    /// # Errors
    /// [`Error::DevicesError`] if the host cannot enumerate.
    pub fn snapshot(&self) -> Result<Snapshot> {
        let name = |d: cpal::Device| d.name().unwrap_or_default();
        let default_output = self.inner.default_output_device().map(name);
        let default_input = self.inner.default_input_device().map(name);
        Ok(Snapshot::from_names(
            self.inner.output_devices()?.map(name),
            self.inner.input_devices()?.map(name),
            default_output.as_deref(),
            default_input.as_deref(),
        ))
    }

    /// Resolve a selector to a device. **The only place a device is resolved.**
    pub(crate) fn device(
        &self,
        direction: Direction,
        sel: &DeviceSelector,
    ) -> Result<cpal::Device> {
        let list = || -> Result<Vec<cpal::Device>> {
            Ok(match direction {
                Direction::Output => self.inner.output_devices()?.collect(),
                Direction::Input => self.inner.input_devices()?.collect(),
            })
        };
        let what = match direction {
            Direction::Output => "output",
            Direction::Input => "input",
        };

        match sel {
            DeviceSelector::Default => match direction {
                Direction::Output => self.inner.default_output_device(),
                Direction::Input => self.inner.default_input_device(),
            }
            .ok_or_else(|| Error::InvalidDevice(format!("No {what} device available"))),

            // An unknown id is an error rather than a fall back to the
            // default: silently playing out of the wrong device is the
            // failure this selector exists to prevent.
            DeviceSelector::Id(id) => list()?
                .into_iter()
                .filter(|d| d.name().is_ok_and(|n| n == id.name()))
                .nth(id.ordinal())
                .ok_or_else(|| Error::InvalidDevice(format!("no {what} device {id}"))),
        }
    }
}

/// Returns the hosts this build can reach right now, default first.
///
/// A settings UI should offer these rather than every [`AudioHost`] variant:
/// the enum is platform-independent and names hosts this binary may not have
/// been built with.
pub fn available_hosts() -> Vec<AudioHost> {
    let mut out = vec![AudioHost::Default];
    if DeviceHost::open(AudioHost::Jack).is_ok() {
        out.push(AudioHost::Jack);
    }
    out
}

#[cfg(test)]
mod tests;
