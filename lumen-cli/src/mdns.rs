//! mDNS advertisement of the stable `lumen.local` hostname.
//!
//! Viewers type `http://lumen.local:<port>` on any LAN device; the daemon
//! answers `A`/`AAAA` queries for `lumen.local.` with the selected LAN
//! address and also browses as an `_http._tcp` service. The advertisement
//! lives exactly as long as the server: stopping the guard deregisters
//! the record and exits the daemon. Failure to advertise is never fatal —
//! the caller degrades to the IP URL.

use std::net::IpAddr;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceInfo};
use thiserror::Error;

/// The stable hostname viewers type: `http://lumen.local:<port>`.
pub const HOSTNAME: &str = "lumen.local";

/// Wire-format FQDN the daemon advertises (trailing dot included).
const HOST_FQDN: &str = "lumen.local.";
/// Service type advertised alongside the hostname.
const SERVICE_DOMAIN: &str = "_http._tcp.local.";
/// Service instance name shown by service browsers.
const SERVICE_INSTANCE: &str = "Lumen";
/// Bound wait for the daemon to acknowledge shutdown.
const SHUTDOWN_WAIT: Duration = Duration::from_millis(500);

/// Errors advertising the service.
#[derive(Debug, Error)]
pub enum MdnsError {
  /// The mDNS daemon rejected the daemon start or the registration.
  #[error("mDNS advertisement failed: {0}")]
  Daemon(#[from] mdns_sd::Error),
}

/// Live advertisement. [`MdnsGuard::stop`] (or dropping it) deregisters
/// the record and exits the daemon thread.
pub struct MdnsGuard {
  daemon: ServiceDaemon,
  fullname: String,
}

impl MdnsGuard {
  /// Deregister `lumen.local` and stop the daemon, bounded.
  ///
  /// Idempotent: the `Drop` that follows `self`'s consumption re-runs
  /// the same best-effort teardown, which the daemon tolerates.
  pub fn stop(&self) {
    let _ = self.daemon.unregister(&self.fullname);
    if let Ok(status) = self.daemon.shutdown() {
      let _ = status.recv_timeout(SHUTDOWN_WAIT);
    }
  }
}

impl Drop for MdnsGuard {
  fn drop(&mut self) {
    // Best effort: a guard dropped without `stop` still exits the daemon.
    let _ = self.daemon.unregister(&self.fullname);
    let _ = self.daemon.shutdown();
  }
}

/// Advertise `lumen.local` → `ip` for the HTTP service on `port`.
///
/// # Errors
///
/// [`MdnsError`] when the daemon cannot start or the record is rejected
/// (no usable interface, multicast blocked, missing Local Network
/// permission). Callers must warn and fall back to the IP URL — never
/// abort the run over a discovery failure.
pub fn advertise(port: u16, ip: IpAddr) -> Result<MdnsGuard, MdnsError> {
  let daemon = ServiceDaemon::new()?;
  let info = ServiceInfo::new(
    SERVICE_DOMAIN,
    SERVICE_INSTANCE,
    HOST_FQDN,
    ip,
    port,
    &[("path", "/")][..],
  )?;
  let fullname = info.get_fullname().to_owned();
  daemon.register(info)?;
  Ok(MdnsGuard { daemon, fullname })
}

/// Canonical viewer entrypoint: the stable hostname when mDNS
/// registration succeeded, the LAN IP otherwise.
#[must_use]
pub fn viewer_url(mdns_ok: bool, ip: IpAddr, port: u16) -> String {
  if mdns_ok {
    format!("http://{HOSTNAME}:{port}")
  } else {
    fallback_url(ip, port)
  }
}

/// IP fallback: works on every LAN, with or without mDNS.
#[must_use]
pub fn fallback_url(ip: IpAddr, port: u16) -> String {
  if ip.is_ipv6() {
    format!("http://[{ip}]:{port}")
  } else {
    format!("http://{ip}:{port}")
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn mdns_success_advertises_the_stable_hostname() {
    let url = viewer_url(true, "192.168.1.42".parse().unwrap(), 3131);
    assert_eq!(url, "http://lumen.local:3131");
  }

  #[test]
  fn mdns_failure_degrades_to_the_ip_url() {
    let url = viewer_url(false, "192.168.1.42".parse().unwrap(), 3131);
    assert_eq!(url, "http://192.168.1.42:3131");
    let custom = viewer_url(false, "10.8.0.5".parse().unwrap(), 8080);
    assert_eq!(custom, "http://10.8.0.5:8080");
  }

  #[test]
  fn custom_port_keeps_the_hostname() {
    let url = viewer_url(true, "192.168.1.42".parse().unwrap(), 9999);
    assert_eq!(url, "http://lumen.local:9999");
  }

  #[test]
  fn ipv6_fallback_is_a_valid_url() {
    let url = fallback_url("fd00::5".parse().unwrap(), 3131);
    assert_eq!(url, "http://[fd00::5]:3131");
  }

  #[test]
  fn advertise_and_stop_never_panic() {
    // Real multicast depends on the environment (CI runners, sandboxed
    // macOS, VPNs): the guarantee is that registration either works or
    // errors cleanly, and that stopping a live guard completes.
    let result = advertise(3131, "127.0.0.1".parse().unwrap());
    if let Ok(guard) = result {
      guard.stop();
    }
  }
}
