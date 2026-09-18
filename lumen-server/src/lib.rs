//! Axum HTTP/WebSocket server: embedded viewer, session API, typed
//! signaling, and per-peer media feeding.
//!
//! Media never crosses the WebSocket — video and audio flow through
//! WebRTC. The socket only carries session state and SDP/ICE exchange.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
  Router,
  extract::{
    ConnectInfo, Path, Query, State,
    ws::{Message, WebSocket, WebSocketUpgrade},
  },
  http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
  response::{IntoResponse, Response},
  routing::{get, post},
};
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use lumen_core::{EncodedAudioFrame, EncodedFrame};
use lumen_session::{
  AuthDecision, Authorizer, HostMessage, PeerId, PeerInfo, PeerRegistry, SessionToken,
  ViewerMessage,
};
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
#[folder = "../web"]
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

/// Runtime wiring the server needs from the orchestrator.
pub struct ServerConfig {
  /// TCP port to bind (all interfaces).
  pub port: u16,
  /// Secret session token for this `serve` run.
  pub token: SessionToken,
  /// Host approval channel for new viewers.
  pub authorizer: Authorizer,
  /// Live peer registry (shared with the CLI for listing/kicking).
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

  let state = Arc::new(AppState {
    token: cfg.token,
    authorizer: cfg.authorizer,
    registry: cfg.registry,
    frames: cfg.frames,
    audio: cfg.audio,
    keyframe_requests: cfg.keyframe_requests,
    webrtc_bind: cfg.webrtc_bind,
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
  token: SessionToken,
  authorizer: Authorizer,
  registry: Arc<PeerRegistry>,
  frames: broadcast::Sender<Arc<EncodedFrame>>,
  audio: Option<broadcast::Sender<Arc<EncodedAudioFrame>>>,
  keyframe_requests: mpsc::Sender<()>,
  webrtc_bind: Vec<String>,
}
#[derive(Deserialize)]
struct TokenQuery {
  token: Option<String>,
}

#[derive(Serialize)]
struct PeerView {
  id: String,
  address: Option<IpAddr>,
  user_agent: Option<String>,
  since_secs: u64,
}

fn build_router(state: Arc<AppState>) -> Router {
  Router::new()
    .route("/", get(root))
    .route("/s/{token}", get(viewer_page))
    .route(
      "/viewer.css",
      get(|| async { asset_response("viewer.css") }),
    )
    .route("/viewer.js", get(|| async { asset_response("viewer.js") }))
    .route(
      "/health",
      get(|| async { axum::Json(serde_json::json!({"status": "ok"})) }),
    )
    .route("/api/session/{token}", get(session_check))
    .route("/api/peers", get(list_peers))
    .route("/api/peers/{id}/disconnect", post(disconnect_peer))
    .route("/ws/{token}", get(ws_upgrade))
    .with_state(state)
}

async fn root(State(state): State<Arc<AppState>>) -> Response {
  // Convenience for the host machine's own browser.
  axum::response::Redirect::temporary(&format!("/s/{}", state.token)).into_response()
}

async fn viewer_page(State(state): State<Arc<AppState>>, Path(token): Path<String>) -> Response {
  if !state.token.matches(&token) {
    return (
      StatusCode::NOT_FOUND,
      [(CONTENT_TYPE, "text/html; charset=utf-8")],
      INVALID_LINK_HTML,
    )
      .into_response();
  }
  asset_response("index.html")
}

const INVALID_LINK_HTML: &str = r#"<!doctype html>
<meta charset="utf-8"><title>Lumen</title>
<div style="font:15px system-ui;color:#e7ecf2;background:#0b0d10;
 display:grid;place-items:center;height:100vh;margin:0">
 <div style="text-align:center"><h1>Link expired</h1>
 <p style="color:#8a94a3">This Lumen session is no longer active. Start a new
 <code>lumen serve</code> and scan the QR code again.</p></div></div>"#;

async fn session_check(
  State(state): State<Arc<AppState>>,
  Path(token): Path<String>,
) -> impl IntoResponse {
  if state.token.matches(&token) {
    axum::Json(serde_json::json!({"valid": true})).into_response()
  } else {
    (
      StatusCode::NOT_FOUND,
      axum::Json(serde_json::json!({"valid": false})),
    )
      .into_response()
  }
}

async fn list_peers(
  State(state): State<Arc<AppState>>,
  Query(q): Query<TokenQuery>,
) -> impl IntoResponse {
  let Some(token) = q.token.filter(|t| state.token.matches(t)) else {
    return StatusCode::FORBIDDEN.into_response();
  };
  let _ = token;
  let now = Instant::now();
  let peers: Vec<PeerView> = state
    .registry
    .list()
    .into_iter()
    .map(|p| PeerView {
      id: p.id.to_string(),
      address: p.address,
      user_agent: p.user_agent,
      since_secs: now.duration_since(p.joined_at).as_secs(),
    })
    .collect();
  axum::Json(peers).into_response()
}

async fn disconnect_peer(
  State(state): State<Arc<AppState>>,
  Path(id): Path<String>,
  Query(q): Query<TokenQuery>,
) -> impl IntoResponse {
  if !q.token.is_some_and(|t| state.token.matches(&t)) {
    return StatusCode::FORBIDDEN.into_response();
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

/// Wrapper so path ids round-trip to a [`PeerId`].
#[derive(Clone, Copy)]
struct PeerIdStr(PeerId);

impl std::str::FromStr for PeerIdStr {
  type Err = std::num::ParseIntError;
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    u64::from_str_radix(s, 16).map(|v| Self(PeerId::from_raw(v)))
  }
}

async fn ws_upgrade(
  ws: WebSocketUpgrade,
  State(state): State<Arc<AppState>>,
  Path(token): Path<String>,
  headers: HeaderMap,
  ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
  if !state.token.matches(&token) {
    return (StatusCode::NOT_FOUND, "invalid session token").into_response();
  }
  let user_agent = headers
    .get(axum::http::header::USER_AGENT)
    .and_then(|v| v.to_str().ok())
    .map(str::to_owned);
  ws.on_upgrade(move |socket| handle_socket(socket, state, addr.ip(), user_agent))
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

async fn handle_socket(socket: WebSocket, state: Arc<AppState>, addr: IpAddr, ua: Option<String>) {
  let (sink, mut stream) = socket.split();
  let (out_tx, out_rx) = mpsc::unbounded_channel::<HostMessage>();
  let writer = spawn_writer(sink, out_rx);

  let Ok(peer_id) = PeerId::generate() else {
    return;
  };
  let info = PeerInfo {
    id: peer_id,
    address: Some(addr),
    user_agent: ua,
    joined_at: Instant::now(),
  };
  let handle = state.registry.join(info.clone());
  let _ = out_tx.send(HostMessage::Waiting);

  // Host approval gate (fail closed).
  let allowed = matches!(
    state.authorizer.request(info.clone()).await,
    Ok(AuthDecision::Allow)
  );
  if !allowed {
    let _ = out_tx.send(HostMessage::Error {
      message: "The host declined this connection.".to_owned(),
    });
    grace_flush(&writer).await;
    state.registry.leave(peer_id);
    return;
  }

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
}
