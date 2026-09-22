//! Which platform host, and which device on it.
//!
//! Before this module there were four `cpal::default_host()` calls and no way
//! to reach anything else. That mattered twice over: JACK was unreachable even
//! with cpal's `jack` dependency compiled in (because `default_host()` returns
//! ALSA regardless — the host has to be named), and a device was addressed
//! only by its position in one enumeration, which any hot-plug invalidates.
//!
//! All four call sites collapse into [`DeviceHost::open`].

use cpal::traits::{DeviceTrait, HostTrait};

use crate::driver::DeviceInfo;
use crate::error::{Error, Result};

/// Which platform audio host to open devices through.
///
/// **Every variant exists on every platform, deliberately.** cpal's own
/// `HostId` is cfg-generated per target, so mirroring it would make a host's
/// configuration struct — and `bevy_tutti::TuttiPlugin`'s field — a different
/// type on Linux than on macOS. This enum is compile-time stable and resolves
/// at runtime: asking for a host this build cannot reach is
/// [`Error::HostUnavailable`], not a compile error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AudioHost {
    /// Whatever cpal calls the platform default: ALSA, WASAPI, CoreAudio.
    #[default]
    Default,
    /// JACK, on Linux and the BSDs.
    ///
    /// Needs this crate's `jack` feature, which enables cpal's. Note that
    /// cpal declares no `jack` feature of its own — it is the *implicit*
    /// feature of an optional dependency that appears only in cpal's
    /// Linux/BSD target table, so enabling it on macOS or Windows compiles
    /// cleanly and reaches nothing. That is why this variant answers
    /// [`Error::HostUnavailable`] rather than failing to exist.
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

/// How a device is addressed.
///
/// [`Index`](Self::Index) is what the old `Option<usize>` meant and carries
/// the same defect: it is positional within one enumeration, so a device
/// appearing or disappearing renumbers everything after it.
/// [`Name`](Self::Name) survives re-enumeration, which is the cheapest
/// available mitigation — cpal 0.15 exposes no hot-plug notification on any
/// backend, so there is nothing better short of per-platform code.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DeviceSelector {
    /// The host's default device.
    #[default]
    Default,
    /// Position in this host's enumeration. Invalidated by hot-plug.
    Index(usize),
    /// Exact device name. Stable across re-enumeration.
    Name(String),
}

impl From<Option<usize>> for DeviceSelector {
    /// Keeps every call site that predates this type working unchanged.
    fn from(index: Option<usize>) -> Self {
        match index {
            Some(i) => DeviceSelector::Index(i),
            None => DeviceSelector::Default,
        }
    }
}

/// Which way a device is being enumerated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Output,
    /// Constructed only by `mic.rs`, which is `#[cfg(feature = "capture")]`.
    /// The variant stays unconditional so `device()`'s match arms do not have
    /// to be — a cfg'd enum variant would put `#[cfg]` on four match arms in
    /// two functions to save one line here.
    #[cfg_attr(not(feature = "capture"), allow(dead_code))]
    Input,
}

/// An opened platform host, and the device enumeration that goes with it.
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
    /// Open a platform host.
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

    /// Which host this is.
    pub fn id(&self) -> AudioHost {
        self.id
    }

    /// Output devices, as `(index, name)` pairs. The index is positional —
    /// see [`DeviceSelector::Index`].
    ///
    /// # Errors
    /// [`Error::DevicesError`] if the host cannot enumerate.
    pub fn output_devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(self
            .inner
            .output_devices()?
            .enumerate()
            .map(|(index, d)| DeviceInfo {
                index,
                name: d.name().unwrap_or_default(),
            })
            .collect())
    }

    /// Input devices. Same indexing caveat as [`output_devices`](Self::output_devices).
    ///
    /// # Errors
    /// [`Error::DevicesError`] if the host cannot enumerate.
    pub fn input_devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(self
            .inner
            .input_devices()?
            .enumerate()
            .map(|(index, d)| DeviceInfo {
                index,
                name: d.name().unwrap_or_default(),
            })
            .collect())
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

            DeviceSelector::Index(i) => {
                let devices = list()?;
                let count = devices.len();
                devices.into_iter().nth(*i).ok_or_else(|| {
                    Error::InvalidDevice(format!(
                        "{what} device index {i} out of range ({count} available)"
                    ))
                })
            }

            // Name beats index precisely because it survives re-enumeration.
            // An unknown name is an error rather than a fall back to the
            // default: silently playing out of the wrong device is the
            // failure this selector exists to prevent.
            DeviceSelector::Name(name) => {
                let devices = list()?;
                devices
                    .into_iter()
                    .find(|d| d.name().is_ok_and(|n| &n == name))
                    .ok_or_else(|| Error::InvalidDevice(format!("no {what} device named {name:?}")))
            }
        }
    }
}

/// The hosts this build can actually reach, in preference order.
///
/// A host UI should offer these rather than every [`AudioHost`] variant, since
/// the enum is deliberately platform-independent and lists hosts this binary
/// may not have been built with.
pub fn available_hosts() -> Vec<AudioHost> {
    let mut out = vec![AudioHost::Default];
    if DeviceHost::open(AudioHost::Jack).is_ok() {
        out.push(AudioHost::Jack);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default host is always reachable — every supported platform has
    /// one, and `available_hosts` must not return an empty list.
    #[test]
    fn the_default_host_is_always_available() {
        assert!(available_hosts().contains(&AudioHost::Default));
    }

    /// **Asking for a host this build cannot serve is an error, not a silent
    /// fallback.** Falling back to the default would send audio out of a
    /// different device than the user chose, which is exactly the class of
    /// failure this module exists to remove.
    ///
    /// Mutation-checked: making `open_jack`'s unavailable arm return
    /// `Ok(default_host())` fails this.
    #[cfg(not(all(
        feature = "jack",
        any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd"
        )
    )))]
    #[test]
    fn an_unreachable_host_is_refused_rather_than_substituted() {
        let err =
            DeviceHost::open(AudioHost::Jack).expect_err("JACK is not reachable in this build");
        assert!(
            matches!(
                err,
                Error::HostUnavailable {
                    host: AudioHost::Jack,
                    ..
                }
            ),
            "expected HostUnavailable, got {err:?}"
        );
    }

    /// The compatibility shim that keeps `Option<usize>` call sites working.
    #[test]
    fn an_optional_index_maps_to_the_selector_it_used_to_mean() {
        assert_eq!(DeviceSelector::from(None), DeviceSelector::Default);
        assert_eq!(DeviceSelector::from(Some(3)), DeviceSelector::Index(3));
    }
}
