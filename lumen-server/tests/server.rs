//! Tests: routes, viewer/admin token split, approval workflow, signaling,
//! kicking.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use lumen_session::{ApprovalQueue, AuthDecision, Authorizer, PeerRegistry, SessionToken};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

use lumen_server::{ServerConfig, ServerHandle, StreamInfo, spawn_server};

struct TestServer {
  token: SessionToken,
  admin_token: SessionToken,
  addr: SocketAddr,
  registry: Arc<PeerRegistry>,
  approvals: ApprovalQueue,
  shutdown: CancellationToken,
  server: Option<ServerHandle>,
}

impl Drop for TestServer {
  fn drop(&mut self) {
    self.shutdown.cancel();
  }
}

impl TestServer {
  async fn stop(&mut self) {
    self.shutdown.cancel();
    if let Some(server) = self.server.take() {
      let _ = tokio::time::timeout(Duration::from_secs(5), server.join()).await;
    }
  }

  fn admin_query(&self) -> String {
    format!("token={}", self.admin_token)
  }
}

/// Answer every pending approval with `decision` as soon as it appears.
fn spawn_auto_responder(queue: ApprovalQueue, decision: AuthDecision) {
  tokio::spawn(async move {
    loop {
      let waiter = queue.wait_for_change();
      tokio::pin!(waiter);
      waiter.as_mut().enable();
      for pending in queue.poll() {
        let _ = queue.decide(pending.id, decision);
      }
      if queue.poll().is_empty() {
        waiter.await;
      }
    }
  });
}

/// `approve` decides immediately in the background; `false` leaves viewers
/// pending so tests can decide through the admin API.
async fn start(approve: bool) -> TestServer {
  let token = SessionToken::generate().expect("token");
  let admin_token = SessionToken::generate().expect("admin token");
  let (authorizer, approvals) = Authorizer::channel(16);
  if approve {
    spawn_auto_responder(approvals.clone(), AuthDecision::Allow);
  }
  let (frames_tx, _rx) = broadcast::channel::<Arc<lumen_core::EncodedFrame>>(8);
  let (kf_tx, _kf_rx) = mpsc::channel(64);
  let registry = Arc::new(PeerRegistry::new());
  let shutdown = CancellationToken::new();
  let server = spawn_server(ServerConfig {
    port: 0,
    token: token.clone(),
    admin_token: admin_token.clone(),
    admin_allow_lan: false,
    authorizer,
    approvals: approvals.clone(),
    registry: Arc::clone(&registry),
    frames: frames_tx,
    audio: None,
    keyframe_requests: kf_tx,
    shutdown: shutdown.clone(),
    webrtc_bind: Vec::new(),
    stream: StreamInfo {
      source_label: "Test Display".to_owned(),
      width: 1920,
      height: 1080,
      target_fps: 30,
      quality: "auto".to_owned(),
      bitrate_label: "8M".to_owned(),
      audio_label: "off".to_owned(),
      viewer_url: format!("http://127.0.0.1:0/s/{token}"),
    },
    stats: Arc::new(lumen_core::PipelineStats::default()),
  })
  .await
  .expect("server starts");
  TestServer {
    token,
    admin_token,
    addr: server.addr,
    registry,
    approvals,
    shutdown,
    server: Some(server),
  }
}

/// Manual mode: nothing answers approvals; the test decides via the API.
async fn start_manual() -> TestServer {
  let srv = start(false).await;
  assert!(srv.approvals.poll().is_empty());
  srv
}

/// Minimal HTTP/1.1 request; returns (status, full response text).
async fn http_request(addr: SocketAddr, method: &str, path: &str) -> (u16, String) {
  let mut stream = TcpStream::connect(dialable(addr)).await.expect("connect");
  let req = format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
  stream.write_all(req.as_bytes()).await.expect("write");
  let mut buf = Vec::new();
  stream.read_to_end(&mut buf).await.expect("read");
  let text = String::from_utf8_lossy(&buf).into_owned();
  let status: u16 = text
    .split_whitespace()
    .nth(1)
    .and_then(|s| s.parse().ok())
    .unwrap_or(0);
  (status, text)
}
type TestWs =
  tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A dialable address for the server's bound address: a wildcard bind
/// reports `0.0.0.0:port`, which Windows refuses to `connect()` (macOS/Linux
/// alias it to loopback); loopback is what the tests actually mean.
fn dialable(addr: SocketAddr) -> SocketAddr {
  if addr.ip().is_unspecified() {
    (std::net::Ipv4Addr::LOCALHOST, addr.port()).into()
  } else {
    addr
  }
}

async fn ws_connect(addr: SocketAddr, path: &str) -> anyhow::Result<TestWs> {
  let url = format!("ws://{}{path}", dialable(addr));
  let (stream, _) = tokio_tungstenite::connect_async(&url).await?;
  Ok(stream)
}

async fn next_json(stream: &mut TestWs) -> serde_json::Value {
  for _ in 0..10 {
    let frame = tokio::time::timeout(Duration::from_secs(10), stream.next())
      .await
      .expect("ws message timeout")
      .expect("ws open")
      .expect("ws frame");
    if let WsMessage::Text(text) = frame {
      if let Ok(value) = serde_json::from_str(&text) {
        return value;
      }
    }
  }
  panic!("no json message");
}

async fn next_json_type(stream: &mut TestWs, ty: &str) -> serde_json::Value {
  for _ in 0..50 {
    let value = next_json(stream).await;
    if value["type"] == ty {
      return value;
    }
  }
  panic!("no {ty:?} message received");
}

/// Poll the admin state until the pending list is non-empty; returns the
/// first pending entry.
async fn wait_pending_json(srv: &TestServer) -> serde_json::Value {
  for _ in 0..100 {
    let (status, body) = http_request(
      srv.addr,
      "GET",
      &format!("/api/admin/state?{}", srv.admin_query()),
    )
    .await;
    assert_eq!(status, 200);
    let json: serde_json::Value = body
      .rsplit("\r\n\r\n")
      .next()
      .and_then(|b| serde_json::from_str(b).ok())
      .expect("state json");
    if let Some(first) = json["pending"].get(0) {
      return first.clone();
    }
    tokio::time::sleep(Duration::from_millis(25)).await;
  }
  panic!("pending viewer never appeared");
}

async fn admin_state(srv: &TestServer) -> serde_json::Value {
  let (status, body) = http_request(
    srv.addr,
    "GET",
    &format!("/api/admin/state?{}", srv.admin_query()),
  )
  .await;
  assert_eq!(status, 200);
  body
    .rsplit("\r\n\r\n")
    .next()
    .and_then(|b| serde_json::from_str(b).ok())
    .expect("state json")
}

#[tokio::test]
async fn invalid_tokens_are_rejected() {
  let mut srv = start(true).await;
  let (status, _) = http_request(srv.addr, "GET", "/s/wrong-token").await;
  assert_eq!(status, 404);
  let (status, body) = http_request(srv.addr, "GET", "/api/session/wrong-token").await;
  assert_eq!(status, 404);
  assert!(body.contains("false"));
  srv.stop().await;
}

#[tokio::test]
async fn valid_token_serves_viewer_and_session() {
  let mut srv = start(true).await;
  let (status, body) = http_request(srv.addr, "GET", &format!("/s/{}", srv.token)).await;
  assert_eq!(status, 200);
  assert!(body.contains("<video"));
  let (status, body) = http_request(srv.addr, "GET", "/viewer.js").await;
  assert_eq!(status, 200);
  assert!(body.contains("text/javascript"));
  let (status, _body) = http_request(srv.addr, "GET", "/viewer.css").await;
  assert_eq!(status, 200);
  let (status, _) = http_request(srv.addr, "GET", "/health").await;
  assert_eq!(status, 200);
  let (status, _body) = http_request(srv.addr, "GET", "/api/session/wrong-token").await;
  assert_eq!(status, 404);
  let (status, body) = http_request(srv.addr, "GET", &format!("/api/session/{}", srv.token)).await;
  assert_eq!(status, 200);
  assert!(body.contains("true"));
  srv.stop().await;
}

#[tokio::test]
async fn ws_with_bad_token_fails_handshake() {
  let mut srv = start(true).await;
  let err = ws_connect(srv.addr, "/ws/not-the-token").await;
  assert!(err.is_err(), "handshake must fail for invalid token");
  srv.stop().await;
}

#[tokio::test]
async fn accepted_viewer_receives_waiting_then_offer() {
  let mut srv = start(true).await;
  let mut stream = ws_connect(srv.addr, &format!("/ws/{}", srv.token))
    .await
    .expect("ws connects");

  let waiting = next_json(&mut stream).await;
  assert_eq!(waiting["type"], "waiting");

  let offer = next_json(&mut stream).await;
  assert_eq!(offer["type"], "offer");
  let sdp = offer["sdp"].as_str().expect("sdp string");
  assert!(sdp.contains("v=0"), "offer must be an SDP");
  assert!(sdp.to_uppercase().contains("H264"));
  assert!(sdp.contains("m=video"), "offer needs a video m-line");
  assert!(
    !sdp.contains("m=audio"),
    "video-only server must not advertise an audio m-line"
  );

  // Registry reflects the live peer.
  assert_eq!(srv.registry.count(), 1);

  stream.close(None).await.ok();
  srv.stop().await;
}

#[tokio::test]
async fn declined_viewer_gets_error_and_is_not_registered() {
  let mut srv = start_manual().await;
  let mut stream = ws_connect(srv.addr, &format!("/ws/{}", srv.token))
    .await
    .expect("ws connects");

  let waiting = next_json(&mut stream).await;
  assert_eq!(waiting["type"], "waiting");

  let pending = wait_pending_json(&srv).await;
  let id = pending["id"].as_str().expect("pending id").to_owned();
  let (status, _) = http_request(
    srv.addr,
    "POST",
    &format!("/api/admin/pending/{id}/deny?{}", srv.admin_query()),
  )
  .await;
  assert_eq!(status, 200);

  let error = next_json(&mut stream).await;
  assert_eq!(error["type"], "error");

  // Give the server a moment to deregister, then check.
  tokio::time::sleep(Duration::from_millis(200)).await;
  assert_eq!(srv.registry.count(), 0);
  srv.stop().await;
}

#[tokio::test]
async fn viewer_token_cannot_access_admin_apis() {
  let mut srv = start(true).await;
  let viewer = srv.token.to_string();

  // The admin dashboard and every API reject the viewer token and no token.
  for path in [
    format!("/admin/{viewer}"),
    format!("/api/admin/state?token={viewer}"),
    "/api/admin/state".to_owned(),
    format!("/api/admin/qr?token={viewer}"),
    "/api/admin/qr".to_owned(),
  ] {
    let (status, _) = http_request(srv.addr, "GET", &path).await;
    assert_eq!(
      status, 404,
      "{path} must not be reachable with a viewer token"
    );
  }
  let (status, _) = http_request(
    srv.addr,
    "POST",
    &format!("/api/admin/pending/deadbeef/allow?token={viewer}"),
  )
  .await;
  assert_eq!(status, 404);
  let (status, _) = http_request(
    srv.addr,
    "POST",
    &format!("/api/admin/peers/deadbeef/disconnect?token={viewer}"),
  )
  .await;
  assert_eq!(status, 404);
  srv.stop().await;
}

#[tokio::test]
async fn admin_page_and_state_require_admin_token() {
  let mut srv = start(true).await;
  let (status, body) = http_request(srv.addr, "GET", &format!("/admin/{}", srv.admin_token)).await;
  assert_eq!(status, 200);
  assert!(body.contains("text/html"));

  let json = admin_state(&srv).await;
  assert_eq!(json["status"], "streaming");
  assert_eq!(json["source"]["label"], "Test Display");
  assert_eq!(json["source"]["width"], 1920);
  assert_eq!(json["fps"]["target"], 30);
  assert!(
    json["viewerUrl"]
      .as_str()
      .is_some_and(|u| u.contains("/s/"))
  );
  assert!(json["pending"].is_array());
  assert!(json["peers"].is_array());
  srv.stop().await;
}

#[tokio::test]
async fn admin_can_approve_pending_viewer() {
  let mut srv = start_manual().await;
  let mut stream = ws_connect(srv.addr, &format!("/ws/{}", srv.token))
    .await
    .expect("ws connects");
  let waiting = next_json(&mut stream).await;
  assert_eq!(waiting["type"], "waiting");

  let pending = wait_pending_json(&srv).await;
  let id = pending["id"].as_str().expect("pending id").to_owned();
  assert_eq!(pending["address"], "127.0.0.1");

  let (status, _) = http_request(
    srv.addr,
    "POST",
    &format!("/api/admin/pending/{id}/allow?{}", srv.admin_query()),
  )
  .await;
  assert_eq!(status, 200);

  let offer = next_json_type(&mut stream, "offer").await;
  assert!(offer["sdp"].as_str().is_some());

  // The decided request leaves the pending list.
  let json = admin_state(&srv).await;
  assert_eq!(json["pending"].as_array().map(Vec::len), Some(0));
  srv.stop().await;
}

#[tokio::test]
async fn admin_can_disconnect_viewer_without_stopping_server() {
  let mut srv = start(true).await;
  let mut stream = ws_connect(srv.addr, &format!("/ws/{}", srv.token))
    .await
    .expect("ws connects");
  let _waiting = next_json(&mut stream).await;
  let offer = next_json(&mut stream).await;
  assert_eq!(offer["type"], "offer");

  // Find the peer id via the admin API.
  let json = admin_state(&srv).await;
  let id = json["peers"][0]["id"].as_str().expect("peer id").to_owned();

  let (status, _) = http_request(
    srv.addr,
    "POST",
    &format!("/api/admin/peers/{id}/disconnect?{}", srv.admin_query()),
  )
  .await;
  assert_eq!(status, 200);

  // The viewer receives the disconnect notice.
  let _msg = next_json_type(&mut stream, "disconnected").await;

  // Server keeps serving afterwards.
  let (status, _) = http_request(srv.addr, "GET", "/health").await;
  assert_eq!(status, 200);
  srv.stop().await;
}

#[tokio::test]
async fn admin_qr_serves_svg() {
  let mut srv = start(true).await;
  let (status, body) = http_request(
    srv.addr,
    "GET",
    &format!("/api/admin/qr?{}", srv.admin_query()),
  )
  .await;
  assert_eq!(status, 200);
  assert!(body.contains("image/svg+xml"));
  assert!(body.contains("<svg"));
  srv.stop().await;
}

#[tokio::test]
async fn malformed_signaling_frames_are_ignored() {
  let mut srv = start(true).await;
  let mut stream = ws_connect(srv.addr, &format!("/ws/{}", srv.token))
    .await
    .expect("ws connects");
  let _waiting = next_json(&mut stream).await;
  let _offer = next_json(&mut stream).await;

  stream
    .send(WsMessage::Text("not json at all".into()))
    .await
    .expect("send junk");
  stream
    .send(WsMessage::Text(r#"{"type":"bogus"}"#.into()))
    .await
    .expect("send unknown type");

  // Connection survives: an answer to the real offer still processes.
  // (Sending an invalid SDP yields a host error, proving the session is alive.)
  stream
    .send(WsMessage::Text(
      serde_json::json!({"type":"answer","sdp":"garbage"})
        .to_string()
        .into(),
    ))
    .await
    .expect("send answer");
  let _err = next_json_type(&mut stream, "error").await;
  srv.stop().await;
}
