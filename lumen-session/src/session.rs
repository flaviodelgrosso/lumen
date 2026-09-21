//! The shared sharing session: capture → encode → WebRTC fan-out plus the
//! HTTP/signaling server, with graceful shutdown.
//!
//! This is the exact pipeline `lumen serve` runs, stripped of all terminal
//! presentation (banners, QR, prompts, Ctrl+C): those stay in the CLI. The
//! desktop frontend drives the same code through [`crate::controller`].

use std::net::IpAddr;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use lumen_core::{Dimensions, PipelineStats, StreamConfig};
use lumen_media::capture::{
  AudioCaptureSource, CaptureError, CaptureSource, PlatformAudioCapture, PlatformCapture,
  list_displays,
};
use lumen_media::encoder::{
  AudioEncoder, OpusAudioEncoder, VideoEncoderSetup, create_video_encoder,
};
use lumen_server::{
  ApprovalQueue, Authorizer, PairingCode, PeerRegistry, ServerConfig, ServerHandle, SessionToken,
  StreamInfo, spawn_server,
};

use crate::error::SessionError;
use crate::mdns::{self, MdnsGuard};
use crate::network::{self, LanInterface, NetworkError};
use crate::pipeline::{self, PipelineHandle};

/// Default HTTP/signaling port (`lumen serve` without `--port`).
pub const DEFAULT_PORT: u16 = 3131;

/// Everything needed to start one sharing session, resolved by the frontend.
///
/// The struct is deliberately plain data: argument parsing, prompts and
/// platform defaults stay in the frontends, the session only executes.
#[derive(Clone, Debug)]
pub struct SessionConfig {
  /// Resolved stream configuration (fps, quality, audio, encoder, name).
  pub stream: StreamConfig,
  /// Display id to capture (`None` = primary display).
  pub display: Option<u32>,
  /// Window id to capture (takes precedence over `display`).
  pub window: Option<u32>,
  /// LAN address to advertise; `None` auto-selects the first suitable
  /// interface (frontends with a terminal may prompt and pass `Some`).
  pub bind: Option<IpAddr>,
  /// TCP port for the viewer/signaling server.
  pub port: u16,
  /// Admit viewers without host approval via a pairing code.
  pub auto_accept: bool,
  /// Allow non-localhost clients on the admin dashboard.
  pub allow_lan_admin: bool,
  /// Advertise the stable `lumen.local` hostname via mDNS.
  pub mdns: bool,
  /// Local UDP addresses for ICE (empty = all interfaces).
  pub webrtc_bind: Vec<String>,
}

impl Default for SessionConfig {
  fn default() -> Self {
    Self {
      stream: StreamConfig::default(),
      display: None,
      window: None,
      bind: None,
      port: DEFAULT_PORT,
      auto_accept: false,
      allow_lan_admin: false,
      mdns: true,
      webrtc_bind: Vec::new(),
    }
  }
}

/// Everything a frontend needs to describe a running session.
///
/// `admin_url` carries the session token: treat it as a capability URL —
/// show it to the host, never log it. The derived-free `Debug` impl keeps
/// the URL and pairing code out of accidental `{session:?}` log lines.
#[derive(Clone)]
pub struct SessionInfo {
  /// The selected (or manually given) LAN interface.
  pub iface: LanInterface,
  /// Bound HTTP/signaling port.
  pub port: u16,
  /// Captured dimensions.
  pub dims: Dimensions,
  /// Canonical viewer URL (stable hostname when mDNS is up, LAN IP
  /// otherwise). No secret in the URL.
  pub viewer_url: String,
  /// IP fallback URL, present only when mDNS is up.
  pub fallback_url: Option<String>,
  /// Whether the stable `lumen.local` hostname is live.
  pub mdns_ok: bool,
  /// Loopback admin dashboard URL (includes the session token).
  pub admin_url: String,
  /// LAN admin dashboard URL, present only when `allow_lan_admin`.
  pub lan_admin_url: Option<String>,
  /// Display form of the pairing code, present only for `auto_accept`.
  pub pairing_code: Option<String>,
  /// Capture description shown on the host dashboard and CLI banner.
  pub stream: StreamInfo,
}

impl std::fmt::Debug for SessionInfo {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SessionInfo")
      .field("iface", &self.iface)
      .field("port", &self.port)
      .field("dims", &self.dims)
      .field("fallback_url", &self.fallback_url)
      .field("mdns_ok", &self.mdns_ok)
      .field("viewer_url", &self.viewer_url)
      .field("admin_url", &"[redacted]")
      .field(
        "lan_admin_url",
        &self.lan_admin_url.as_ref().map(|_| "[redacted]"),
      )
      .field(
        "pairing_code",
        &self.pairing_code.as_ref().map(|_| "[redacted]"),
      )
      .field("stream", &self.stream)
      .finish()
  }
}

/// A running session. Dropping without [`Session::shutdown`] is a bug: the
/// pipeline and server keep running until the process exits.
pub struct Session {
  /// Session description for frontends (URLs, labels, dimensions).
  pub info: SessionInfo,
  approvals: ApprovalQueue,
  registry: Arc<PeerRegistry>,
  stats: Arc<PipelineStats>,
  shutdown: CancellationToken,
  server: Option<ServerHandle>,
  pipeline: Option<PipelineHandle>,
  mdns: Option<MdnsGuard>,
}

impl Session {
  /// Canonical viewer URL (no secret; share freely on the LAN).
  #[must_use]
  pub fn viewer_url(&self) -> &str {
    &self.info.viewer_url
  }

  /// Admin dashboard URL including the session token (capability URL).
  #[must_use]
  pub fn admin_url(&self) -> &str {
    &self.info.admin_url
  }

  /// Shared approval queue (the CLI terminal prompt and the dashboard poll
  /// the same queue; whichever surface answers first wins).
  #[must_use]
  pub fn approvals(&self) -> ApprovalQueue {
    self.approvals.clone()
  }

  /// Live peer registry for stats surfaces.
  #[must_use]
  pub fn registry(&self) -> &Arc<PeerRegistry> {
    &self.registry
  }

  /// Live pipeline counters for stats surfaces.
  #[must_use]
  pub fn stats(&self) -> &Arc<PipelineStats> {
    &self.stats
  }

  /// Token canceled by [`Session::shutdown`]; frontends hang their own
  /// background loops (stats, approval prompts) on it.
  #[must_use]
  pub fn shutdown_token(&self) -> CancellationToken {
    self.shutdown.clone()
  }

  /// Graceful, deterministic shutdown: cancel, drain the server, stop the
  /// mDNS advertisement, then stop the pipeline.
  ///
  /// # Errors
  ///
  /// Propagates a server join failure (the pipeline is still torn down).
  pub async fn shutdown(mut self) -> Result<(), SessionError> {
    self.shutdown.cancel();
    let mut error = None;
    if let Some(server) = self.server.take()
      && let Err(e) = server.join().await
    {
      error = Some(SessionError::from(e));
    }
    if let Some(guard) = self.mdns.take() {
      guard.stop();
    }
    if let Some(handle) = self.pipeline.take() {
      handle.shutdown().await;
    }
    error.map_or(Ok(()), Err)
  }
}

/// Start one sharing session: capture, encoder, pipeline, mDNS and the
/// HTTP/signaling server.
///
/// On any failure everything already started is torn down before the error
/// is returned, so a failed start never leaks capture threads, sockets or
/// the mDNS daemon — frontends may retry.
///
/// # Errors
///
/// See [`SessionError`]. mDNS advertisement failure is never fatal: the
/// session degrades to the IP URL.
pub async fn start_session(cfg: SessionConfig) -> Result<Session, SessionError> {
  cfg.stream.validate()?;

  // ── capture + encoder ──
  let (source, dims, setup) = open_media(&cfg)?;

  // ── network ──
  let iface = resolve_interface(cfg.bind)?;

  // Preflight: resolving the `.local` ICE candidates Chromium browsers
  // advertise requires sending IPv4 multicast. A failure here is fatal
  // for those viewers (Safari offers plain IPs and still works), so
  // name the usual cause instead of leaving only webrtc-rs errors.
  if let Err(source) = network::probe_multicast() {
    tracing::warn!(
      "IPv4 multicast send failed ({source}); Chromium-based viewers \
       (mDNS-obfuscated ICE candidates) will not connect.\n{}",
      multicast_remediation()
    );
  }

  // ── session + services ──
  let admin_token = SessionToken::generate()?;
  let pairing_code = cfg.auto_accept.then(PairingCode::generate).transpose()?;
  let (authorizer, approvals) = Authorizer::channel(16);
  let registry = Arc::new(PeerRegistry::new());
  let (kf_tx, kf_rx) = mpsc::channel(64);
  let shutdown = CancellationToken::new();
  let audio = build_audio(&cfg.stream);

  let pipe = pipeline::start(
    source,
    setup.encoder,
    cfg.stream.fps,
    cfg.stream.keyframe_interval_secs,
    shutdown.clone(),
    kf_rx,
    audio,
  )
  .map_err(SessionError::Pipeline)?;

  let mdns = cfg
    .mdns
    .then(|| advertise_mdns(cfg.port, iface.ip))
    .flatten();
  let viewer_url = mdns::viewer_url(mdns.is_some(), iface.ip, cfg.port);
  let stream = build_stream_info(&cfg, dims, setup.backend, pipe.audio.is_some(), &viewer_url);

  let server = match spawn_server(ServerConfig {
    port: cfg.port,
    admin_token: admin_token.clone(),
    admin_allow_lan: cfg.allow_lan_admin,
    auto_accept_pairing: pairing_code.clone(),
    authorizer,
    approvals: approvals.clone(),
    registry: Arc::clone(&registry),
    frames: pipe.frames.clone(),
    audio: pipe.audio.clone(),
    keyframe_requests: kf_tx,
    shutdown: shutdown.clone(),
    webrtc_bind: cfg.webrtc_bind.clone(),
    stream: stream.clone(),
    stats: Arc::clone(&pipe.stats),
  })
  .await
  {
    Ok(server) => server,
    Err(e) => {
      // Clean up everything already running before handing back the error.
      shutdown.cancel();
      if let Some(guard) = mdns {
        guard.stop();
      }
      pipe.shutdown().await;
      return Err(SessionError::Server(e));
    }
  };

  let iface_ip = iface.ip;
  let port = server.addr.port();
  let info = SessionInfo {
    iface,
    port,
    dims,
    fallback_url: mdns.is_some().then(|| mdns::fallback_url(iface_ip, port)),
    mdns_ok: mdns.is_some(),
    admin_url: format!("http://127.0.0.1:{port}/admin/{admin_token}"),
    lan_admin_url: cfg
      .allow_lan_admin
      .then(|| format!("http://{iface_ip}:{port}/admin/{admin_token}")),
    pairing_code: pairing_code.map(|code| code.to_string()),
    viewer_url,
    stream,
  };

  Ok(Session {
    info,
    approvals,
    registry,
    stats: Arc::clone(&pipe.stats),
    shutdown,
    server: Some(server),
    pipeline: Some(pipe),
    mdns,
  })
}

/// Open the capture source and create the encoder for one session.
fn open_media(
  cfg: &SessionConfig,
) -> Result<(Box<dyn CaptureSource>, Dimensions, VideoEncoderSetup), SessionError> {
  let mut capture = match cfg.window {
    Some(id) => PlatformCapture::for_window(id, cfg.stream.fps)?,
    None => PlatformCapture::for_display(cfg.display, cfg.stream.fps)?,
  };
  let dims = capture.dimensions();
  capture.start()?;

  let bitrate = cfg.stream.effective_bitrate(dims);
  let keyframe_frames = cfg
    .stream
    .fps
    .saturating_mul(u32::try_from(cfg.stream.keyframe_interval_secs).unwrap_or(1))
    .max(1);
  // On encoder failure `capture` is dropped here, which stops the engine.
  let setup = create_video_encoder(
    cfg.stream.encoder,
    dims,
    cfg.stream.fps,
    bitrate,
    keyframe_frames,
  )
  .map_err(SessionError::Encoder)?;
  tracing::info!("video encoder backend: {}", setup.backend);
  Ok((Box::new(capture), dims, setup))
}

/// Resolve the advertised interface: a given address wins; otherwise the
/// first suitable candidate (discovery sorts IPv4 first, then by address).
/// Frontends with a terminal may prompt and pass the choice as `Some`.
fn resolve_interface(bind: Option<IpAddr>) -> Result<LanInterface, SessionError> {
  if let Some(ip) = bind {
    return Ok(LanInterface {
      name: "manual".into(),
      ip,
    });
  }
  let interfaces = network::discover()?;
  network::select(&interfaces, None)?
    .into_iter()
    .next()
    .ok_or_else(|| NetworkError::NoneFound.into())
}

/// Assemble the dashboard/banner stream description from resolved config.
fn build_stream_info(
  cfg: &SessionConfig,
  dims: Dimensions,
  encoder_backend: &str,
  audio_active: bool,
  viewer_url: &str,
) -> StreamInfo {
  StreamInfo {
    source_label: source_label(cfg, dims),
    width: dims.width,
    height: dims.height,
    target_fps: cfg.stream.fps,
    quality: cfg.stream.quality.to_string(),
    bitrate_label: cfg.stream.effective_bitrate(dims).to_string(),
    encoder_label: encoder_backend.to_owned(),
    audio_label: audio_label(&cfg.stream, audio_active),
    viewer_url: viewer_url.to_owned(),
    session_name: cfg.stream.session_name.clone(),
  }
}

/// Build the system-audio capture + encoder pair, or `None` (with a warning)
/// when audio is disabled or unavailable — the stream then carries video only.
fn build_audio(cfg: &StreamConfig) -> Option<(Box<dyn AudioCaptureSource>, Box<dyn AudioEncoder>)> {
  if !cfg.audio {
    return None;
  }
  let source = match PlatformAudioCapture::new() {
    Ok(source) => source,
    Err(CaptureError::AudioNotSupported) => {
      tracing::warn!("system audio is not available here; streaming video only");
      return None;
    }
    Err(e) => {
      tracing::warn!("audio capture unavailable ({e}); streaming video only");
      return None;
    }
  };
  match OpusAudioEncoder::new(cfg.audio_bitrate) {
    Ok(encoder) => Some((
      Box::new(source) as Box<dyn AudioCaptureSource>,
      Box::new(encoder) as Box<dyn AudioEncoder>,
    )),
    Err(e) => {
      tracing::warn!("audio encoder unavailable ({e}); streaming video only");
      None
    }
  }
}

/// Advertise `lumen.local`, warning and degrading to the IP URL on failure
/// (discovery is never fatal).
fn advertise_mdns(port: u16, ip: IpAddr) -> Option<MdnsGuard> {
  mdns::advertise(port, ip).map_or_else(
    |error| {
      tracing::warn!(
        "could not announce {} via mDNS ({error}); viewers must use {}.\n{}",
        mdns::HOSTNAME,
        mdns::fallback_url(ip, port),
        multicast_remediation()
      );
      None
    },
    Some,
  )
}

/// OS-specific corrective guidance for the multicast preflight warning; a
/// Windows user must never be told to open macOS System Settings.
#[cfg(target_os = "macos")]
fn multicast_remediation() -> &'static str {
  "On macOS 15+ this is the Local Network privacy permission: open \
   System Settings -> Privacy & Security -> Local Network and allow \
   your TERMINAL app (the prompt is often suppressed for binaries \
   run from a terminal), then restart the terminal and lumen. \
   A VPN or VM NIC without multicast routing fails the same way."
}

#[cfg(target_os = "windows")]
fn multicast_remediation() -> &'static str {
  "On Windows this is usually Windows Defender Firewall blocking UDP multicast \
   for this app — allow lumen on the current network profile (see \
   Troubleshooting in the README) — or a VPN/VM adapter without multicast \
   routing, which fails the same way."
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn multicast_remediation() -> &'static str {
  "A VPN or VM NIC without multicast routing fails the same way."
}

/// Human-readable capture source for the banner and the dashboard.
fn source_label(cfg: &SessionConfig, dims: Dimensions) -> String {
  if let Some(id) = cfg.window {
    format!("Window {id} — {}x{}", dims.width, dims.height)
  } else {
    let name = cfg.display.map_or_else(
      || "Primary display".to_owned(),
      |id| {
        list_displays()
          .unwrap_or_default()
          .into_iter()
          .find(|d| d.id == id)
          .map_or_else(|| "Display".to_owned(), |d| d.title)
      },
    );
    format!("{name} — {}x{}", dims.width, dims.height)
  }
}

/// Audio description for the banner and the dashboard.
fn audio_label(cfg: &StreamConfig, audio_enabled: bool) -> String {
  if audio_enabled {
    format!("system audio — Opus, {} stereo", cfg.audio_bitrate)
  } else if cfg.audio {
    "unavailable — video only".to_owned()
  } else {
    "off (--no-audio)".to_owned()
  }
}
