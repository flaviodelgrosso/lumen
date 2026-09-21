//! LAN interface discovery and bind-address selection.
//!
//! Uses [`local_ip_address`] to enumerate interfaces, filters obviously
//! unsuitable addresses (loopback, link-local, unspecified) and lets the
//! caller decide when several plausible candidates remain.

use std::fmt;
use std::net::IpAddr;

use thiserror::Error;

/// A candidate host address Lumen could bind to and advertise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanInterface {
  /// OS interface name (e.g. `en0`, `wlan0`).
  pub name: String,
  /// The address.
  pub ip: IpAddr,
}

impl fmt::Display for LanInterface {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{} ({})", self.ip, self.name)
  }
}

/// Errors during interface discovery or bind selection.
#[derive(Debug, Error)]
pub enum NetworkError {
  /// The OS interface enumeration failed.
  #[error("failed to enumerate network interfaces: {source}")]
  Discovery {
    /// Underlying enumeration error.
    #[from]
    source: local_ip_address::Error,
  },
  /// The requested `--bind` address is not assigned to any interface.
  #[error("address {requested} is not assigned to any interface")]
  Unavailable {
    /// The requested address.
    requested: IpAddr,
    /// Plausible alternatives found on this host.
    available: Vec<LanInterface>,
  },
  /// No usable interface was found.
  #[error("no usable LAN interface found; pass --bind <ip> explicitly")]
  NoneFound,
}

/// Returns `true` when an address is plausible for LAN sharing.
///
/// Filtered out: loopback, unspecified, IPv6-local, IPv4 link-local
/// (`169.254/16`) and carrier-grade-NAT-ish `0.0.0.0/8` leftovers.
/// Everything else — private, VPN, Docker bridges — is kept; the user can
/// disambiguate with `--bind`.
#[must_use]
pub fn is_suitable(ip: IpAddr) -> bool {
  if ip.is_loopback() || ip.is_unspecified() {
    return false;
  }
  match ip {
    IpAddr::V4(v4) => {
      // 169.254/16 link-local (APIPA / failed DHCP).
      if v4.is_link_local() {
        return false;
      }
      // 0.x.x.x is reserved and never routable even on a LAN.
      v4.octets()[0] != 0
    }
    IpAddr::V6(v6) => {
      // Keep everything except IPv6 link-local fe80::/10.
      (v6.segments()[0] & 0xffc0) != 0xfe80
    }
  }
}

/// Enumerate all suitable LAN interfaces on this host.
///
/// # Errors
///
/// Returns [`NetworkError::Discovery`] when the OS call fails.
pub fn discover() -> Result<Vec<LanInterface>, NetworkError> {
  let interfaces = local_ip_address::list_afinet_netifas()?;
  let mut seen = Vec::new();
  for (name, ip) in interfaces {
    if is_suitable(ip) {
      seen.push(LanInterface { name, ip });
    }
  }
  // Stable, predictable ordering: IPv4 first, then by address.
  seen.sort_by(|a, b| match (a.ip, b.ip) {
    (IpAddr::V4(x), IpAddr::V4(y)) => x.cmp(&y),
    (IpAddr::V4(_), IpAddr::V6(_)) => std::cmp::Ordering::Less,
    (IpAddr::V6(_), IpAddr::V4(_)) => std::cmp::Ordering::Greater,
    (IpAddr::V6(x), IpAddr::V6(y)) => x.cmp(&y),
  });
  Ok(seen)
}

/// Pick the interface to advertise.
///
/// * `requested = Some(ip)` — must match a discovered address, otherwise
///   [`NetworkError::Unavailable`] with the alternatives.
/// * `requested = None` — exactly one suitable interface wins automatically;
///   several are returned so the caller can prompt the user.
///
/// # Errors
///
/// See [`NetworkError`] variants.
pub fn select(
  interfaces: &[LanInterface],
  requested: Option<IpAddr>,
) -> Result<Vec<LanInterface>, NetworkError> {
  let suitable: Vec<LanInterface> = interfaces
    .iter()
    .filter(|i| is_suitable(i.ip))
    .cloned()
    .collect();
  if let Some(ip) = requested {
    return match suitable.iter().find(|i| i.ip == ip) {
      Some(iface) => Ok(vec![iface.clone()]),
      None => Err(NetworkError::Unavailable {
        requested: ip,
        available: suitable,
      }),
    };
  }
  if suitable.is_empty() {
    return Err(NetworkError::NoneFound);
  }
  Ok(suitable)
}

/// Probe whether this host can actually send IPv4 multicast.
///
/// Chromium browsers obfuscate their ICE host candidates as mDNS
/// `.local` names, which `lumen` resolves by querying `224.0.0.251:5353`
/// (IPv4 multicast). If the OS refuses to route multicast datagrams,
/// every such candidate is dropped and LAN viewers never connect.
///
/// On macOS 15+ the most common cause is the **Local Network** privacy
/// permission, which is owned by the app responsible for the process —
/// for a CLI binary run from a terminal, the *terminal app* itself.
///
/// Sends one null byte to the mDNS group (responders discard it).
///
/// # Errors
///
/// Returns the OS error when the send fails — `EHOSTUNREACH`
/// ("No route to host") on macOS means the permission is missing or
/// the route is genuinely broken.
pub fn probe_multicast() -> Result<(), std::io::Error> {
  let socket = std::net::UdpSocket::bind(std::net::SocketAddr::from(([0, 0, 0, 0], 0)))?;
  socket
    .send_to(b"\0", (std::net::Ipv4Addr::new(224, 0, 0, 251), MDNS_PORT))
    .map(|_| ())
}

/// The mDNS/UDP port `lumen` queries browser `.local` candidates on.
pub const MDNS_PORT: u16 = 5353;

#[cfg(test)]
mod tests {
  use super::*;

  fn iface(name: &str, ip: &str) -> LanInterface {
    LanInterface {
      name: name.to_owned(),
      ip: ip.parse().unwrap(),
    }
  }

  #[test]
  fn filters_unsuitable_addresses() {
    assert!(!is_suitable("127.0.0.1".parse().unwrap()));
    assert!(!is_suitable("::1".parse().unwrap()));
    assert!(!is_suitable("0.0.0.0".parse().unwrap()));
    assert!(!is_suitable("169.254.11.22".parse().unwrap()));
    assert!(!is_suitable("0.1.2.3".parse().unwrap()));
    assert!(!is_suitable("fe80::1".parse().unwrap()));
  }

  #[test]
  fn keeps_plausible_addresses() {
    for ip in [
      "192.168.1.42",
      "10.8.0.5",    // VPN
      "172.17.0.1",  // docker bridge
      "100.64.0.3",  // tailscale/CGNAT
      "203.0.113.7", // public (direct-attached LAN)
      "fd00::5",     // ULA
    ] {
      assert!(is_suitable(ip.parse().unwrap()), "{ip} should be kept");
    }
  }

  #[test]
  fn single_interface_wins_automatically() {
    let ifaces = vec![iface("en0", "192.168.1.42")];
    let chosen = select(&ifaces, None).unwrap();
    assert_eq!(chosen.len(), 1);
    assert_eq!(chosen[0].ip.to_string(), "192.168.1.42");
  }

  #[test]
  fn ambiguous_selection_is_returned_for_prompt() {
    let ifaces = vec![
      iface("en0", "192.168.1.42"),
      iface("utun3", "10.8.0.5"),
      iface("en5", "172.17.0.1"),
    ];
    let chosen = select(&ifaces, None).unwrap();
    assert_eq!(chosen.len(), 3);
  }

  #[test]
  fn unsuitable_interfaces_are_excluded_from_choices() {
    let ifaces = vec![
      iface("lo0", "127.0.0.1"),
      iface("en0", "192.168.1.42"),
      iface("en9", "169.254.9.9"),
    ];
    let chosen = select(&ifaces, None).unwrap();
    assert_eq!(chosen.len(), 1);
    assert_eq!(chosen[0].name, "en0");
  }

  #[test]
  fn explicit_bind_must_exist() {
    let ifaces = vec![iface("en0", "192.168.1.42")];
    let ok = select(&ifaces, Some("192.168.1.42".parse().unwrap())).unwrap();
    assert_eq!(ok.len(), 1);

    let err = select(&ifaces, Some("10.0.0.9".parse().unwrap())).unwrap_err();
    assert!(matches!(err, NetworkError::Unavailable { available, .. } if available.len() == 1));
  }

  #[test]
  fn no_usable_interface_errors() {
    let ifaces = vec![iface("lo0", "127.0.0.1")];
    assert!(matches!(
      select(&ifaces, None),
      Err(NetworkError::NoneFound)
    ));
  }

  #[test]
  fn discovery_does_not_panic_on_real_host() {
    // Smoke: real enumeration must at least return Ok on dev machines.
    let _ = discover();
  }
  #[test]
  fn multicast_probe_returns_without_panic() {
    // Outcome is environment-dependent (a denied Local Network
    // permission must yield Err, not panic); only the shape is
    // guaranteed here.
    let _ = probe_multicast();
  }
}
