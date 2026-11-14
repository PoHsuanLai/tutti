//! Finding Hosts on the network (M2-124 section 4): DNS-SD over mDNS, by the
//! `mdns-sd` crate.
//!
//! [`Advertisement`] publishes a Host; [`Browser`] reports the Hosts that
//! others publish. [`txt_properties`] and [`FoundHost::from_service`] are the
//! one place the TXT keys are written and read.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use tutti_midi_device::{EndpointName, NameError, ProductInstanceId};

use crate::EndpointIdentity;

/// The service type a Host is published under.
pub const SERVICE_TYPE: &str = "_midi2._udp.local.";

/// The TXT key that holds the UMP Endpoint Name.
pub const KEY_ENDPOINT_NAME: &str = "UMPEndpointName";

/// The TXT key that holds the Product Instance Id.
pub const KEY_PRODUCT_INSTANCE_ID: &str = "ProductInstanceId";

/// The longest a DNS label (a service instance name) is, in bytes.
const LABEL_MAX: usize = 63;

/// Why discovery fails, or a published Host is refused.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// The mDNS daemon failed.
    #[error("mDNS: {0}")]
    Mdns(#[from] mdns_sd::Error),
    /// The mDNS hostname is not ASCII.
    #[error("the mDNS hostname must be ASCII")]
    Hostname,
    /// A TXT key the specification requires is missing.
    #[error("the TXT record has no {0} key")]
    MissingKey(&'static str),
    /// A TXT value is not UTF-8.
    #[error("the TXT value of {0} is not UTF-8")]
    Utf8(&'static str),
    /// The Endpoint Name in the TXT record is not a valid name.
    #[error("the UMPEndpointName is invalid: {0}")]
    EndpointName(NameError),
    /// The Product Instance Id in the TXT record is not a valid Id.
    #[error("the ProductInstanceId is invalid: {0}")]
    InstanceId(NameError),
    /// The service resolved with no address.
    #[error("the service has no address")]
    NoAddress,
}

/// The TXT key and value pairs a Host publishes for `identity`.
#[must_use]
pub fn txt_properties(identity: &EndpointIdentity) -> [(&'static str, String); 2] {
    [
        (KEY_ENDPOINT_NAME, identity.name.as_str().to_owned()),
        (
            KEY_PRODUCT_INSTANCE_ID,
            identity.instance_id.as_str().to_owned(),
        ),
    ]
}

/// The service instance name for `identity`: its Product Instance Id, a dash
/// and its name, cut to a DNS label.
#[must_use]
pub fn instance_name(identity: &EndpointIdentity) -> String {
    let full = format!("{}-{}", identity.instance_id, identity.name);
    let mut end = full.len().min(LABEL_MAX);
    while !full.is_char_boundary(end) {
        end -= 1;
    }
    full[..end].to_owned()
}

fn local_hostname(hostname: &str) -> Result<String, DiscoveryError> {
    if !hostname.is_ascii() {
        return Err(DiscoveryError::Hostname);
    }
    let bare = hostname.trim_end_matches('.');
    let bare = bare.strip_suffix(".local").unwrap_or(bare);
    Ok(format!("{bare}.local."))
}

/// A Host published on the network, until dropped.
///
/// The records follow M2-124 section 4: the PTR and SRV under
/// [`SERVICE_TYPE`], the TXT keys of [`txt_properties`] and the A and AAAA
/// records of the addresses given. The `mdns-sd` crate fixes the record
/// lifetimes at its own defaults, longer than the minute the specification
/// recommends, so a Host that vanishes without unpublishing is forgotten
/// by its neighbours later than that.
pub struct Advertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertisement {
    /// Publishes the Host `identity`, listening on `port` at `addresses`
    /// under the mDNS name `hostname`. No addresses publishes every address
    /// of the machine and follows their changes.
    ///
    /// # Errors
    ///
    /// [`DiscoveryError::Hostname`] for a hostname that is not ASCII, and
    /// [`DiscoveryError::Mdns`] when the daemon cannot start or publish.
    pub fn start(
        identity: &EndpointIdentity,
        hostname: &str,
        addresses: &[IpAddr],
        port: u16,
    ) -> Result<Self, DiscoveryError> {
        let host = local_hostname(hostname)?;
        let daemon = ServiceDaemon::new()?;
        let mut info = ServiceInfo::new(
            SERVICE_TYPE,
            &instance_name(identity),
            &host,
            addresses,
            port,
            &txt_properties(identity)[..],
        )?;
        if addresses.is_empty() {
            info = info.enable_addr_auto();
        }
        let fullname = info.get_fullname().to_owned();
        daemon.register(info)?;
        Ok(Self { daemon, fullname })
    }
}

impl std::fmt::Debug for Advertisement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Advertisement")
            .field("fullname", &self.fullname)
            .finish_non_exhaustive()
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        // Unpublish so neighbours drop the entry now, then stop the daemon. A
        // daemon that is already gone has nothing left to tell.
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// A Host found on the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundHost {
    /// The service instance's full name; the key to match a later removal.
    pub fullname: String,
    /// The Host's UMP Endpoint Name and Product Instance Id, from its TXT
    /// record.
    pub identity: EndpointIdentity,
    /// Where to send the Invitation: each address with the Host's port.
    pub addresses: Vec<SocketAddr>,
}

impl FoundHost {
    /// Reads a resolved service as a Host.
    ///
    /// # Errors
    ///
    /// A [`DiscoveryError`] for a missing or invalid TXT key, or no address.
    pub fn from_service(service: &ResolvedService) -> Result<Self, DiscoveryError> {
        let name = txt_text(service, KEY_ENDPOINT_NAME)?;
        let id = txt_text(service, KEY_PRODUCT_INSTANCE_ID)?;
        let mut addresses: Vec<SocketAddr> = service
            .get_addresses()
            .iter()
            .map(|ip| SocketAddr::new(ip.to_ip_addr(), service.get_port()))
            .collect();
        if addresses.is_empty() {
            return Err(DiscoveryError::NoAddress);
        }
        addresses.sort();
        Ok(Self {
            fullname: service.get_fullname().to_owned(),
            identity: EndpointIdentity {
                name: EndpointName::parse(name).map_err(DiscoveryError::EndpointName)?,
                instance_id: ProductInstanceId::parse(id).map_err(DiscoveryError::InstanceId)?,
            },
            addresses,
        })
    }
}

fn txt_text<'a>(
    service: &'a ResolvedService,
    key: &'static str,
) -> Result<&'a str, DiscoveryError> {
    let value = service
        .get_property_val(key)
        .flatten()
        .ok_or(DiscoveryError::MissingKey(key))?;
    std::str::from_utf8(value).map_err(|_| DiscoveryError::Utf8(key))
}

/// What a [`Browser`] saw.
#[derive(Debug)]
pub enum HostEvent {
    /// A Host appeared, or its record changed.
    Found(FoundHost),
    /// A Host went away.
    Removed {
        /// The service instance's full name, as in [`FoundHost::fullname`].
        fullname: String,
    },
    /// A service of the right type whose records do not describe a Host.
    Refused {
        /// The service instance's full name.
        fullname: String,
        /// What is wrong.
        error: DiscoveryError,
    },
}

/// A search for Hosts, until dropped.
pub struct Browser {
    daemon: ServiceDaemon,
    events: mdns_sd::Receiver<ServiceEvent>,
}

impl Browser {
    /// Starts looking for Hosts.
    ///
    /// # Errors
    ///
    /// [`DiscoveryError::Mdns`] when the daemon cannot start.
    pub fn start() -> Result<Self, DiscoveryError> {
        let daemon = ServiceDaemon::new()?;
        let events = daemon.browse(SERVICE_TYPE)?;
        Ok(Self { daemon, events })
    }

    /// The next Host event, waiting up to `timeout` for one.
    pub fn next_event(&self, timeout: Duration) -> Option<HostEvent> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.events.recv_timeout(left).ok()? {
                ServiceEvent::ServiceResolved(service) => {
                    return Some(match FoundHost::from_service(&service) {
                        Ok(host) => HostEvent::Found(host),
                        Err(error) => HostEvent::Refused {
                            fullname: service.get_fullname().to_owned(),
                            error,
                        },
                    });
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    return Some(HostEvent::Removed { fullname });
                }
                _ => {}
            }
        }
    }
}

impl std::fmt::Debug for Browser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Browser").finish_non_exhaustive()
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}
