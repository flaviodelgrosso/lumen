//! Axum HTTP/WebSocket server: embedded viewer, join/approval API, typed
//! signaling, and per-peer media feeding.
//!
//! Media never crosses the WebSocket — video and audio flow through
//! WebRTC. The socket only carries session state and SDP/ICE exchange.
//!
//! Viewers enter through `/` with no URL secret: `POST /api/join` parks a
//! [`join::JoinManager`] request in the host approval queue, the polling
//! viewer receives an ephemeral post-approval [`grant::GrantStore`] grant,
//! and the signaling socket authenticates with that grant. The session
//! modules ([`auth`], [`grant`], [`join`], [`peer`], [`signal`], [`token`],
//! [`ua`]) hold the approval pool, grant/join state, the peer registry and
//! the typed signaling protocol. The signaling enums are the single source
//! of truth for the WebSocket JSON format shared by this server and the
//! embedded browser viewer.

pub mod auth;
pub mod grant;
pub mod join;
pub mod peer;
pub mod signal;
pub mod token;
pub mod ua;

pub use auth::{ApprovalQueue, AuthDecision, AuthError, Authorizer, PendingView};
pub use grant::GrantStore;
pub use join::{JoinError, JoinManager, JoinRequest, JoinStatus};
pub use peer::{PeerInfo, PeerRegistry};
pub use signal::{HostMessage, SignalMessage, ViewerMessage};
pub use token::{PairingCode, PeerId, SessionToken};
pub use ua::describe_user_agent;

use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
  Router,
  extract::{
    ConnectInfo, Path, Query, State,
    ws::{Message, WebSocket, WebSocketUpgrade},
  },
  http::{
    HeaderMap, StatusCode,
    header::{AUTHORIZATION, CONTENT_TYPE},
  },
  response::{IntoResponse, Response},
  routing::{get, post},
};
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use lumen_core::{EncodedAudioFrame, EncodedFrame, PipelineStats};
use lumen_webrtc::{Peer, PeerEvent};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
  net::TcpListener,
  sync::{broadcast, mpsc},
};
use tokio_util::sync::CancellationToken;

/// Errors starting the server.
#[derive(Debug, Error)]
pub enum ServerError {
  /// The TCP listener could not bind.
  #[error("failed to bind port {port}: {source}")]
  Bind {
    /// Requested port.
    port: u16,
    /// Underlying IO error.
    #[source]
    source: std::io::Error,
  },
  /// The server task failed.
  #[error("server error: {0}")]
  Server(String),
}

/// Embedded viewer assets from `web/`.
#[derive(RustEmbed)]
#[folder = "web"]
struct Assets;

fn mime_for(name: &str) -> &'static str {
  match name.rsplit('.').next() {
    Some("html") => "text/html; charset=utf-8",
    Some("css") => "text/css; charset=utf-8",
    Some("js") => "text/javascript; charset=utf-8",
    _ => "application/octet-stream",
  }
}

fn asset_response(name: &str) -> Response {
  match Assets::get(name) {
    Some(file) => ([(CONTENT_TYPE, mime_for(name))], file.data.to_vec()).into_response(),
    None => (StatusCode::NOT_FOUND, "not found").into_response(),
  }
}

/// Static description of the running capture, surfaced on the admin
/// dashboard (source, quality, and the canonical viewer link).
#[derive(Clone, Debug)]
pub struct StreamInfo {
  /// Human-readable capture source (e.g. `Built-in Retina Display`).
  pub source_label: String,
  /// Captured width in pixels.
  pub width: u32,
  /// Captured height in pixels.
  pub height: u32,
  /// Target capture/encode frame rate.
  pub target_fps: u32,
  /// Quality preset name (`auto`, `low`, …).
  pub quality: String,
  /// Effective encoder bitrate, human-readable (`8.0M` style).
  pub bitrate_label: String,
  /// Selected encoder backend (`openh264`, `videotoolbox`).
  pub encoder_label: String,
  /// Audio description (`Opus 128k stereo`, `off`, `unavailable`).
  pub audio_label: String,
  /// Canonical viewer URL (stable `lumen.local` hostname when mDNS
  /// registration succeeded, LAN IP otherwise). No secret in the URL.
  pub viewer_url: String,
}

/// Runtime wiring the server needs from the orchestrator.
pub struct ServerConfig {
  /// TCP port to bind (all interfaces).
  pub port: u16,
  /// Secret admin token: grants the dashboard and `/api/admin/*`.
  pub admin_token: SessionToken,
  /// Allow non-localhost clients on the admin surface (default off).
  pub admin_allow_lan: bool,
  /// Pairing secret required for unattended joins; absent in normal mode.
  pub auto_accept_pairing: Option<PairingCode>,
  /// Host approval channel for new viewers.
  pub authorizer: Authorizer,
  /// Shared approval pool polled by the admin dashboard.
  pub approvals: ApprovalQueue,
  /// Live peer registry (shared with the admin surface for listing/kicking).
  pub registry: Arc<PeerRegistry>,
  /// Shared encoded-video fan-out; one encoder serves all peers.
  pub frames: broadcast::Sender<Arc<EncodedFrame>>,
  /// Shared encoded-audio fan-out; `None` streams video only.
  pub audio: Option<broadcast::Sender<Arc<EncodedAudioFrame>>>,
  /// Keyframe requests (a new viewer asks the encoder for an IDR).
  pub keyframe_requests: mpsc::Sender<()>,
  /// Cancels the HTTP server and all signaling.
  pub shutdown: CancellationToken,
  /// Local UDP addresses for ICE (empty = `0.0.0.0:0`, all interfaces).
  pub webrtc_bind: Vec<String>,
  /// Capture description for the admin dashboard.
  pub stream: StreamInfo,
  /// Live pipeline counters (fps sampling for the admin dashboard).
  pub stats: Arc<PipelineStats>,
}

/// A running server.
pub struct ServerHandle {
  /// Bound local address.
  pub addr: SocketAddr,
  task: tokio::task::JoinHandle<Result<(), ServerError>>,
}

impl ServerHandle {
  /// Wait for the server to stop (after shutdown).
  ///
  /// # Errors
  ///
  /// Propagates [`ServerError`] if the server failed.
  pub async fn join(self) -> Result<(), ServerError> {
    self
      .task
      .await
      .map_err(|e| ServerError::Server(format!("server task panicked: {e}")))?
  }
}

/// Start the HTTP/WebSocket server.
///
/// # Errors
///
/// See [`ServerError`].
pub async fn spawn_server(cfg: ServerConfig) -> Result<ServerHandle, ServerError> {
  let listener = TcpListener::bind(("0.0.0.0", cfg.port))
    .await
    .map_err(|source| ServerError::Bind {
      port: cfg.port,
      source,
    })?;
  let addr = listener.local_addr().map_err(|source| ServerError::Bind {
    port: cfg.port,
    source,
  })?;

  let grants = GrantStore::new();
  let joins = JoinManager::new(cfg.authorizer, grants.clone());
  let state = Arc::new(AppState {
    admin_token: cfg.admin_token,
    admin_allow_lan: cfg.admin_allow_lan,
    auto_accept_pairing: cfg.auto_accept_pairing,
    approvals: cfg.approvals,
    joins,
    grants,
    registry: cfg.registry,
    frames: cfg.frames,
    audio: cfg.audio,
    keyframe_requests: cfg.keyframe_requests,
    webrtc_bind: cfg.webrtc_bind,
    stream: cfg.stream,
    stats: cfg.stats,
    fps_sample: Mutex::new(None),
  });

  let app = build_router(state);
  let shutdown = cfg.shutdown;
  let server = axum::serve(
    listener,
    app.into_make_service_with_connect_info::<SocketAddr>(),
  )
  .with_graceful_shutdown(async move { shutdown.cancelled().await });

  let task =
    tokio::spawn(async move { server.await.map_err(|e| ServerError::Server(e.to_string())) });

  Ok(ServerHandle { addr, task })
}

struct AppState {
  admin_token: SessionToken,
  admin_allow_lan: bool,
  auto_accept_pairing: Option<PairingCode>,
  approvals: ApprovalQueue,
  joins: JoinManager,
  grants: GrantStore,
  registry: Arc<PeerRegistry>,
  frames: broadcast::Sender<Arc<EncodedFrame>>,
  audio: Option<broadcast::Sender<Arc<EncodedAudioFrame>>>,
  keyframe_requests: mpsc::Sender<()>,
  webrtc_bind: Vec<String>,
  stream: StreamInfo,
  stats: Arc<PipelineStats>,
  /// Last admin-status sample: (instant, captured, encoded, audio packets).
  fps_sample: Mutex<Option<(Instant, u64, u64, u64)>>,
}

#[derive(Deserialize)]
struct TokenQuery {
  token: Option<String>,
}

/// (capture fps, encoded fps) since the previous admin poll; `None` until
/// a second sample exists.
impl AppState {
  #[expect(
    clippy::cast_precision_loss,
    reason = "displayed fps rates; counter deltas stay far below 2^52"
  )]
  fn sample_fps(&self) -> Option<(f64, f64)> {
    let captured = self.stats.captured.load(Ordering::Relaxed);
    let encoded = self.stats.encoded.load(Ordering::Relaxed);
    let audio = self.stats.audio_encoded.load(Ordering::Relaxed);
    let now = Instant::now();
    let mut last = self.fps_sample.lock().ok()?;
    let prev = last.replace((now, captured, encoded, audio));
    let (then, p_captured, p_encoded, _) = prev?;
    let elapsed = now.duration_since(then).as_secs_f64();
    if elapsed < 0.25 {
      return None; // too soon for a meaningful rate
    }
    Some((
      captured.saturating_sub(p_captured) as f64 / elapsed,
      encoded.saturating_sub(p_encoded) as f64 / elapsed,
    ))
  }
}

fn build_router(state: Arc<AppState>) -> Router {
  Router::new()
    .route("/", get(root))
    .route("/api/join", post(join_create))
    .route("/api/join/config", get(join_config))
    .route("/api/join/{id}", get(join_poll))
    .route(
      "/viewer.css",
      get(|| async { asset_response("viewer.css") }),
    )
    .route("/viewer.js", get(|| async { asset_response("viewer.js") }))
    .route("/admin.css", get(|| async { asset_response("admin.css") }))
    .route("/admin.js", get(|| async { asset_response("admin.js") }))
    .route(
      "/health",
      get(|| async { axum::Json(serde_json::json!({"status": "ok"})) }),
    )
    .route("/admin/{token}", get(admin_page))
    .route("/api/admin/state", get(admin_state))
    .route("/api/admin/qr", get(admin_qr))
    .route("/api/admin/pending/{id}/allow", post(admin_allow))
    .route("/api/admin/pending/{id}/deny", post(admin_deny))
    .route("/api/admin/peers/{id}/disconnect", post(admin_disconnect))
    .route("/ws/{grant}", get(ws_upgrade))
    .with_state(state)
}

/// The viewer page is the canonical entrypoint: reachable by anyone on
/// the LAN, and it carries no capability — stream access requires host
/// approval plus an ephemeral grant (see [`join_create`]).
async fn root() -> Response {
  asset_response("index.html")
}

const INVALID_LINK_HTML: &str = r#"<!doctype html>
<meta charset="utf-8"><title>Lumen</title>
<div style="font:15px system-ui;color:#e7ecf2;background:#0b0d10;
 display:grid;place-items:center;height:100vh;margin:0">
 <div style="text-align:center"><h1>Link expired</h1>
 <p style="color:#8a94a3">This Lumen session is no longer active. Start a new
 <code>lumen serve</code> and scan the QR code again.</p></div></div>"#;

/// Pairing is only required in unattended mode. The response exposes no
/// capability and lets the device-agnostic viewer choose the right UI.
async fn join_config(State(state): State<Arc<AppState>>) -> Response {
  axum::Json(serde_json::json!({"pairingRequired": state.auto_accept_pairing.is_some()}))
    .into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JoinCreateBody {
  pairing_code: Option<String>,
}

/// Extract a join polling capability from a standard bearer header.
fn join_token(headers: &HeaderMap) -> Option<&str> {
  headers
    .get(AUTHORIZATION)
    .and_then(|value| value.to_str().ok())
    .and_then(|value| value.strip_prefix("Bearer "))
    .filter(|token| !token.is_empty())
}

/// Create a join request: parks the viewer in the host approval queue.
/// The response carries a public peer id plus a per-request polling secret.
async fn join_create(
  State(state): State<Arc<AppState>>,
  headers: HeaderMap,
  ConnectInfo(addr): ConnectInfo<SocketAddr>,
  body: Option<axum::Json<JoinCreateBody>>,
) -> Response {
  let pairing_code = body.and_then(|body| body.0.pairing_code);
  if let Some(expected) = &state.auto_accept_pairing {
    let valid = pairing_code
      .as_deref()
      .is_some_and(|candidate| expected.matches(candidate));
    if !valid {
      return if state.joins.record_failed_attempt(addr.ip()) {
        StatusCode::FORBIDDEN.into_response()
      } else {
        StatusCode::TOO_MANY_REQUESTS.into_response()
      };
    }
  }
  let Ok(id) = PeerId::generate() else {
    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
  };
  let info = PeerInfo {
    id,
    address: Some(addr.ip()),
    user_agent: headers
      .get(axum::http::header::USER_AGENT)
      .and_then(|v| v.to_str().ok())
      .map(str::to_owned),
    joined_at: Instant::now(),
  };
  match state.joins.create(info) {
    Ok(join) => {
      if state.auto_accept_pairing.is_some() {
        let _ = state.approvals.decide(join.id, AuthDecision::Allow);
      }
      (
        StatusCode::CREATED,
        axum::Json(serde_json::json!({
          "requestId": join.id.to_string(),
          "joinToken": join.token.to_string(),
        })),
      )
        .into_response()
    }
    Err(JoinError::Full | JoinError::RateLimited | JoinError::NoApprover) => {
      StatusCode::TOO_MANY_REQUESTS.into_response()
    }
    Err(JoinError::Random(_)) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
  }
}

/// Poll a join request. On approval the first authorized poll issues the
/// ephemeral single-use grant the viewer redeems on `/ws/<grant>`.
async fn join_poll(
  State(state): State<Arc<AppState>>,
  Path(id): Path<String>,
  headers: HeaderMap,
) -> Response {
  let Ok(parsed) = id.parse::<PeerIdStr>() else {
    return StatusCode::NOT_FOUND.into_response();
  };
  let Some(token) = join_token(&headers) else {
    return StatusCode::NOT_FOUND.into_response();
  };
  let (status, body) = match state.joins.poll(parsed.0, token) {
    JoinStatus::Pending => (StatusCode::OK, serde_json::json!({"status": "pending"})),
    JoinStatus::Approved { grant } => (
      StatusCode::OK,
      serde_json::json!({"status": "approved", "grant": grant}),
    ),
    JoinStatus::Denied => (StatusCode::OK, serde_json::json!({"status": "denied"})),
    JoinStatus::Gone => (StatusCode::NOT_FOUND, serde_json::json!({"status": "gone"})),
  };
  (status, axum::Json(body)).into_response()
}

/// Admin origin rule: loopback always, LAN only when explicitly allowed.
#[must_use]
fn admin_origin_allowed(ip: IpAddr, allow_lan: bool) -> bool {
  allow_lan || ip.is_loopback()
}

/// Admin gate: wrong/missing token is a plain 404 (the surface must not be
/// distinguishable from a dead route); a wrong-origin request is a 403.
/// Returns `None` when the request is authorized.
fn admin_denied(state: &AppState, token: Option<&str>, ip: IpAddr) -> Option<Response> {
  if !token.is_some_and(|t| state.admin_token.matches(t)) {
    return Some(StatusCode::NOT_FOUND.into_response());
  }
  if !admin_origin_allowed(ip, state.admin_allow_lan) {
    return Some(StatusCode::FORBIDDEN.into_response());
  }
  None
}

async fn admin_page(
  State(state): State<Arc<AppState>>,
  Path(token): Path<String>,
  ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
  if !state.admin_token.matches(&token) {
    return (
      StatusCode::NOT_FOUND,
      [(CONTENT_TYPE, "text/html; charset=utf-8")],
      INVALID_LINK_HTML,
    )
      .into_response();
  }
  if !admin_origin_allowed(addr.ip(), state.admin_allow_lan) {
    return StatusCode::FORBIDDEN.into_response();
  }
  asset_response("admin.html")
}

#[derive(Serialize)]
struct AdminPeerView {
  id: String,
  device: String,
  address: Option<IpAddr>,
  since_secs: u64,
}

#[derive(Serialize)]
struct AdminPendingView {
  id: String,
  device: String,
  address: Option<IpAddr>,
  waited_secs: u64,
}

/// Round an fps rate to one decimal for display.
fn round_fps(value: f64) -> f64 {
  (value * 10.0).round() / 10.0
}

async fn admin_state(
  State(state): State<Arc<AppState>>,
  Query(q): Query<TokenQuery>,
  ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
  if let Some(denied) = admin_denied(&state, q.token.as_deref(), addr.ip()) {
    return denied;
  }
  let now = Instant::now();
  let fps = state.sample_fps();
  let peers: Vec<AdminPeerView> = state
    .registry
    .list()
    .into_iter()
    .map(|p| AdminPeerView {
      id: p.id.to_string(),
      device: describe_user_agent(p.user_agent.as_deref()),
      address: p.address,
      since_secs: now.duration_since(p.joined_at).as_secs(),
    })
    .collect();
  let pending: Vec<AdminPendingView> = state
    .approvals
    .poll()
    .into_iter()
    .map(|req| AdminPendingView {
      id: req.id.to_string(),
      device: describe_user_agent(req.peer.user_agent.as_deref()),
      address: req.peer.address,
      waited_secs: req.waited.as_secs(),
    })
    .collect();
  axum::Json(serde_json::json!({
    "status": "streaming",
    "source": {
      "label": state.stream.source_label,
      "width": state.stream.width,
      "height": state.stream.height,
    },
    "fps": {
      "target": state.stream.target_fps,
      "capture": fps.map(|(captured, _)| round_fps(captured)),
      "encoded": fps.map(|(_, encoded)| round_fps(encoded)),
    },
    "quality": state.stream.quality,
    "bitrate": state.stream.bitrate_label,
    "encoder": state.stream.encoder_label,
    "audio": state.stream.audio_label,
    "viewerUrl": state.stream.viewer_url,
    "pending": pending,
    "peers": peers,
  }))
  .into_response()
}

async fn admin_allow(
  State(state): State<Arc<AppState>>,
  Path(id): Path<String>,
  Query(q): Query<TokenQuery>,
  ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
  admin_decide(
    &state,
    &id,
    q.token.as_deref(),
    addr.ip(),
    AuthDecision::Allow,
  )
}

async fn admin_deny(
  State(state): State<Arc<AppState>>,
  Path(id): Path<String>,
  Query(q): Query<TokenQuery>,
  ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
  admin_decide(
    &state,
    &id,
    q.token.as_deref(),
    addr.ip(),
    AuthDecision::Deny,
  )
}

fn admin_decide(
  state: &AppState,
  id: &str,
  token: Option<&str>,
  ip: IpAddr,
  decision: AuthDecision,
) -> Response {
  if let Some(denied) = admin_denied(state, token, ip) {
    return denied;
  }
  let Ok(parsed) = id.parse::<PeerIdStr>() else {
    return StatusCode::NOT_FOUND.into_response();
  };
  if state.approvals.decide(parsed.0, decision) {
    StatusCode::OK.into_response()
  } else {
    StatusCode::NOT_FOUND.into_response()
  }
}

async fn admin_disconnect(
  State(state): State<Arc<AppState>>,
  Path(id): Path<String>,
  Query(q): Query<TokenQuery>,
  ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
  if let Some(denied) = admin_denied(&state, q.token.as_deref(), addr.ip()) {
    return denied;
  }
  let Ok(parsed) = id.parse::<PeerIdStr>() else {
    return StatusCode::NOT_FOUND.into_response();
  };
  if state.registry.disconnect(parsed.0) {
    StatusCode::OK.into_response()
  } else {
    StatusCode::NOT_FOUND.into_response()
  }
}

async fn admin_qr(
  State(state): State<Arc<AppState>>,
  Query(q): Query<TokenQuery>,
  ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
  if let Some(denied) = admin_denied(&state, q.token.as_deref(), addr.ip()) {
    return denied;
  }
  match qr_svg(&state.stream.viewer_url) {
    Some(svg) => ([(CONTENT_TYPE, "image/svg+xml; charset=utf-8")], svg).into_response(),
    None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
  }
}

/// Minimal dependency-free SVG QR rendering (light background, dark
/// modules — the contrast scanners expect).
fn qr_svg(data: &str) -> Option<String> {
  let code = qrcode::QrCode::new(data).ok()?;
  let side = code.width();
  let quiet = 2usize;
  let total = side + quiet * 2;
  let mut path = String::new();
  for (index, color) in code.to_colors().iter().enumerate() {
    if *color == qrcode::types::Color::Dark {
      let x = index % side;
      let y = index / side;
      // Writing into a String cannot fail.
      let _ = write!(path, "M{x} {y}h1v1h-1z");
    }
  }
  Some(format!(
    r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {total} {total}" shape-rendering="crispEdges"><rect width="{total}" height="{total}" fill="#e7ecf2"/><g transform="translate({quiet} {quiet})" fill="#0b0d10"><path d="{path}"/></g></svg>"##
  ))
}

/// Wrapper so path ids round-trip to a [`PeerId`].
#[derive(Clone, Copy)]
struct PeerIdStr(PeerId);

impl std::str::FromStr for PeerIdStr {
  type Err = std::num::ParseIntError;
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    u64::from_str_radix(s, 16).map(|v| Self(PeerId::from_raw(v)))
  }
}

/// Signaling entry: the path carries an ephemeral post-approval grant.
/// Redeeming it (constant-time, single-use, TTL-checked) is what admits
/// the socket — an unknown, used or expired grant is a plain 404.
async fn ws_upgrade(
  ws: WebSocketUpgrade,
  State(state): State<Arc<AppState>>,
  Path(grant): Path<String>,
) -> Response {
  let Some(info) = state.grants.redeem(&grant) else {
    return (StatusCode::NOT_FOUND, "invalid or expired grant").into_response();
  };
  ws.on_upgrade(move |socket| handle_socket(socket, state, info))
}

/// UDP addresses to bind for a viewer's ICE agent.
///
/// `configured` (the `--webrtc-bind` override) wins verbatim. Otherwise the
/// wildcard is expanded per interface by the facade — which skips loopback —
/// so a viewer on the host itself needs an explicit loopback socket.
///
/// That loopback socket must be bound **only for a viewer that is itself on
/// loopback**. A loopback socket becomes a host candidate that can pair with
/// any remote candidate, so a LAN viewer pairs with it and ICE pings the LAN
/// from `127.0.0.1`; the kernel rejects that with `EADDRNOTAVAIL` (`Can't
/// assign requested address`) on every check. The pair can never succeed, yet
/// it consumes binding requests and stalls connectivity checks that would
/// otherwise succeed over the real LAN interface.
fn ice_bind_addrs(configured: &[String], viewer: IpAddr) -> Vec<String> {
  if !configured.is_empty() {
    return configured.to_vec();
  }
  let mut bind = vec!["0.0.0.0:0".to_owned()];
  if viewer.is_loopback() {
    bind.push("127.0.0.1:0".to_owned());
  }
  bind
}

/// Drive one approved viewer: the grant already carries host approval,
/// so the socket goes straight to registration and negotiation.
async fn handle_socket(socket: WebSocket, state: Arc<AppState>, info: PeerInfo) {
  let (sink, mut stream) = socket.split();
  let (out_tx, out_rx) = mpsc::unbounded_channel::<HostMessage>();
  let writer = spawn_writer(sink, out_rx);

  let peer_id = info.id;
  let addr = info.address.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
  let handle = state.registry.join(info);

  // WebRTC peer with a complete (non-trickle) offer. The media fan-out is
  // the source of truth: unavailable audio must not create an SDP track.
  let mut peer = match Peer::with_bind(
    ice_bind_addrs(&state.webrtc_bind, addr),
    state.audio.is_some(),
  )
  .await
  {
    Ok(peer) => peer,
    Err(e) => {
      tracing::warn!("peer setup failed: {e}");
      let _ = out_tx.send(HostMessage::Error {
        message: "Could not start the media session.".to_owned(),
      });
      grace_flush(&writer).await;
      state.registry.leave(peer_id);
      return;
    }
  };
  let _ = out_tx.send(HostMessage::Offer {
    sdp: peer.offer_sdp.clone(),
  });
  // A fresh IDR lets this viewer start decoding immediately.
  let _ = state.keyframe_requests.try_send(());

  let events = peer.take_events();
  let peer = Arc::new(peer);

  let local_cancel = CancellationToken::new();

  let relay = tokio::spawn(relay_events(
    events,
    out_tx.clone(),
    state.keyframe_requests.clone(),
  ));

  let feed = tokio::spawn(feed_peer(
    Arc::clone(&peer),
    state.frames.clone(),
    local_cancel.clone(),
    handle.disconnect.clone(),
    out_tx.clone(),
  ));

  let audio_feed = state.audio.clone().map(|audio_frames| {
    tokio::spawn(feed_audio(
      Arc::clone(&peer),
      audio_frames,
      local_cancel.clone(),
      handle.disconnect.clone(),
    ))
  });

  inbound_signaling(&mut stream, &peer, &out_tx).await;

  // Teardown: cancel feed/relay, close peer, deregister.
  local_cancel.cancel();
  let _ = tokio::time::timeout(Duration::from_millis(200), feed).await;
  if let Some(task) = audio_feed {
    let _ = tokio::time::timeout(Duration::from_millis(200), task).await;
  }
  relay.abort();
  peer.close().await;
  drop(out_tx);
  let _ = tokio::time::timeout(Duration::from_millis(200), writer).await;
  state.registry.leave(peer_id);
}

/// Relay the viewer's SDP answer and trickled candidates to the peer.
async fn inbound_signaling<S>(
  stream: &mut S,
  peer: &Peer,
  out_tx: &mpsc::UnboundedSender<HostMessage>,
) where
  S: futures_util::Stream<Item = Result<Message, axum::Error>> + Unpin,
{
  while let Some(msg) = stream.next().await {
    match msg {
      Ok(Message::Text(text)) => {
        let Ok(viewer_msg) = serde_json::from_str::<ViewerMessage>(&text) else {
          tracing::trace!("ignored non-protocol ws frame");
          continue;
        };
        match viewer_msg {
          ViewerMessage::Answer { sdp } => {
            if let Err(e) = peer.apply_answer(&sdp).await {
              tracing::warn!("answer rejected: {e}");
              let _ = out_tx.send(HostMessage::Error {
                message: "SDP negotiation failed.".to_owned(),
              });
              break;
            }
          }
          ViewerMessage::IceCandidate {
            candidate,
            sdp_m_line_index,
          } => {
            if let Err(e) = peer.add_ice_candidate(&candidate, sdp_m_line_index).await {
              tracing::debug!("ice candidate rejected: {e}");
            }
          }
        }
      }
      Ok(Message::Close(_)) | Err(_) => break,
      _ => {}
    }
  }
}

/// Forward peer connection events into the viewer's WebSocket writer.
async fn relay_events(
  mut events: mpsc::UnboundedReceiver<PeerEvent>,
  relay_tx: mpsc::UnboundedSender<HostMessage>,
  keyframe_requests: mpsc::Sender<()>,
) {
  while let Some(event) = events.recv().await {
    let msg = match event {
      PeerEvent::Connected => {
        // Frames written before this point were dropped; ask for a
        // fresh IDR so the viewer decodes as soon as media flows.
        let _ = keyframe_requests.try_send(());
        HostMessage::Connected
      }
      PeerEvent::Disconnected | PeerEvent::Failed => {
        let _ = relay_tx.send(HostMessage::Error {
          message: "The live connection was lost.".to_owned(),
        });
        break;
      }
      PeerEvent::IceCandidate {
        candidate,
        sdp_m_line_index,
      } => HostMessage::IceCandidate {
        candidate,
        sdp_m_line_index,
      },
    };
    let _ = relay_tx.send(msg);
  }
}

/// Fan-out → peer feed. Slow-consumer lag re-syncs at the next keyframe.
async fn feed_peer(
  peer: Arc<Peer>,
  frames_tx: broadcast::Sender<Arc<EncodedFrame>>,
  peer_cancel: CancellationToken,
  host_kick: CancellationToken,
  feed_tx: mpsc::UnboundedSender<HostMessage>,
) {
  let mut rx = frames_tx.subscribe();
  let mut started = false;
  let mut prev: Option<Instant> = None;
  loop {
    tokio::select! {
        () = peer_cancel.cancelled() => break,
        () = host_kick.cancelled() => {
            let _ = feed_tx.send(HostMessage::Disconnected);
            break;
        }
        received = rx.recv() => match received {
            Ok(frame) => {
                if !peer.is_connected() {
                    // Pre-connection frames are dropped; re-arm the
                    // keyframe gate so the stream starts decodable.
                    started = false;
                    prev = None;
                    continue;
                }
                if !started {
                    if !frame.keyframe {
                        continue; // wait for a decodable point
                    }
                    started = true;
                }
                let duration = prev
                    .map_or(Duration::from_millis(33), |p| {
                        frame.captured_at.saturating_duration_since(p)
                    });
                prev = Some(frame.captured_at);
                if let Err(e) = peer.write_frame(&frame, duration).await {
                    tracing::debug!("frame write failed: {e}");
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // Viewer too slow: drop to next keyframe.
                started = false;
                prev = None;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
  }
  peer.close().await;
}

/// Fan-out → peer audio feed. Opus has no keyframes: a lagging viewer
/// resumes at the next packet and the decoder conceals the gap, so a
/// `Lagged` simply continues.
async fn feed_audio(
  peer: Arc<Peer>,
  audio_tx: broadcast::Sender<Arc<EncodedAudioFrame>>,
  peer_cancel: CancellationToken,
  host_kick: CancellationToken,
) {
  let mut rx = audio_tx.subscribe();
  loop {
    tokio::select! {
        () = peer_cancel.cancelled() => break,
        () = host_kick.cancelled() => break,
        received = rx.recv() => match received {
            Ok(frame) => {
                if let Err(e) = peer.write_audio(&frame).await {
                    tracing::debug!("audio write failed: {e}");
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // Audio has no keyframe concept: the Opus decoder at the
                // viewer conceals the gap. Skip to the next packet.
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
  }
}

/// Serialized writer task for one viewer socket.
fn spawn_writer(
  mut sink: SplitSink<WebSocket, Message>,
  mut out_rx: mpsc::UnboundedReceiver<HostMessage>,
) -> tokio::task::JoinHandle<()> {
  tokio::spawn(async move {
    while let Some(msg) = out_rx.recv().await {
      let Ok(text) = serde_json::to_string(&msg) else {
        continue;
      };
      if sink.send(Message::Text(text.into())).await.is_err() {
        break;
      }
    }
  })
}

/// Give queued messages a moment to be written before dropping the channel.
async fn grace_flush(writer: &tokio::task::JoinHandle<()>) {
  for _ in 0..10 {
    if writer.is_finished() {
      break;
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::net::Ipv4Addr;

  #[test]
  fn lan_viewer_gets_no_loopback_bind() {
    let bind = ice_bind_addrs(&[], IpAddr::V4(Ipv4Addr::new(192, 168, 1, 179)));
    assert_eq!(
      bind,
      vec!["0.0.0.0:0".to_owned()],
      "a LAN viewer must not bind a loopback socket: it would pair with \
             the LAN peer and ICE would ping from 127.0.0.1, which the kernel \
             rejects with EADDRNOTAVAIL"
    );
  }

  #[test]
  fn local_viewer_gets_loopback_bind() {
    let bind = ice_bind_addrs(&[], IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(
      bind,
      vec!["0.0.0.0:0".to_owned(), "127.0.0.1:0".to_owned()],
      "a viewer on the host itself needs a loopback socket because the \
             facade's wildcard expansion skips loopback"
    );
    let bind = ice_bind_addrs(&[], IpAddr::V6(std::net::Ipv6Addr::LOCALHOST));
    assert!(bind.contains(&"127.0.0.1:0".to_owned()));
  }

  #[test]
  fn configured_bind_wins_verbatim() {
    let configured = vec!["10.0.0.1:5000".to_owned(), "127.0.0.1:0".to_owned()];
    let bind = ice_bind_addrs(&configured, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 179)));
    assert_eq!(bind, configured, "an explicit bind list is used as given");
  }

  #[test]
  fn admin_defaults_to_loopback_only() {
    assert!(admin_origin_allowed(IpAddr::V4(Ipv4Addr::LOCALHOST), false));
    assert!(admin_origin_allowed(
      IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
      false
    ));
    assert!(
      !admin_origin_allowed(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)), false),
      "LAN origins must not reach the admin surface by default"
    );
    assert!(admin_origin_allowed(
      IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)),
      true
    ));
  }

  #[test]
  fn qr_svg_renders_viewer_url() {
    let svg = qr_svg("http://192.168.1.5:3131/s/sometoken").expect("a URL always fits a QR code");
    assert!(svg.starts_with("<svg"));
    assert!(svg.contains("<path d=\"M"), "dark modules must be drawn");
  }

  #[test]
  fn qr_svg_rejects_impossible_data() {
    // Beyond QR capacity: the helper must return None, not panic.
    let huge = "x".repeat(8000);
    assert!(qr_svg(&huge).is_none());
  }
}
