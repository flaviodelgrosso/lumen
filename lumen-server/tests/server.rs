//! Tests: routes, token validation, authorization, signaling, kicking.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use lumen_session::{AuthDecision, Authorizer, PeerRegistry, SessionToken};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

use lumen_server::{ServerConfig, ServerHandle, spawn_server};

struct TestServer {
  token: SessionToken,
  addr: SocketAddr,
  registry: Arc<PeerRegistry>,
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
}

async fn start(approve: bool) -> TestServer {
  let token = SessionToken::generate().expect("token");
  let (authorizer, mut requests) = Authorizer::channel(16);
  tokio::spawn(async move {
    while let Some(req) = requests.recv().await {
      let decision = if approve {
        AuthDecision::Allow
      } else {
        AuthDecision::Deny
      };
      let _ = req.respond.send(decision);
    }
  });
  let (frames_tx, _rx) = broadcast::channel::<Arc<lumen_core::EncodedFrame>>(8);
  let (kf_tx, _kf_rx) = mpsc::channel(64);
  let registry = Arc::new(PeerRegistry::new());
  let shutdown = CancellationToken::new();
  let server = spawn_server(ServerConfig {
    port: 0,
    token: token.clone(),
    authorizer,
    registry: Arc::clone(&registry),
    frames: frames_tx,
    audio: None,
    keyframe_requests: kf_tx,
    shutdown: shutdown.clone(),
    webrtc_bind: Vec::new(),
  })
  .await
  .expect("server starts");
  TestServer {
    token,
    addr: server.addr,
    registry,
    shutdown,
    server: Some(server),
  }
}

/// Minimal HTTP/1.1 GET; returns (status, full response text).
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
#[tokio::test]
async fn invalid_tokens_are_rejected() {
  let mut srv = start(true).await;
  let (status, _) = http_request(srv.addr, "GET", "/s/wrong-token").await;
  assert_eq!(status, 404);
  let (status, _) = http_request(srv.addr, "GET", "/api/session/wrong-token").await;
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
  let mut srv = start(false).await;
  let mut stream = ws_connect(srv.addr, &format!("/ws/{}", srv.token))
    .await
    .expect("ws connects");

  let waiting = next_json(&mut stream).await;
  assert_eq!(waiting["type"], "waiting");
  let error = next_json(&mut stream).await;
  assert_eq!(error["type"], "error");

  // Give the server a moment to deregister, then check.
  tokio::time::sleep(Duration::from_millis(200)).await;
  assert_eq!(srv.registry.count(), 0);
  srv.stop().await;
}

#[tokio::test]
async fn peers_endpoint_requires_token() {
  let mut srv = start(true).await;
  let (status, _) = http_request(srv.addr, "GET", "/api/peers").await;
  assert_eq!(status, 403);
  let (status, _) = http_request(srv.addr, "GET", "/api/peers?token=nope").await;
  assert_eq!(status, 403);
  let (status, body) =
    http_request(srv.addr, "GET", &format!("/api/peers?token={}", srv.token)).await;
  assert_eq!(status, 200);
  assert!(body.contains('['));
  srv.stop().await;
}

#[tokio::test]
async fn host_can_disconnect_a_viewer_without_stopping_server() {
  let mut srv = start(true).await;
  let mut stream = ws_connect(srv.addr, &format!("/ws/{}", srv.token))
    .await
    .expect("ws connects");
  let _waiting = next_json(&mut stream).await;
  let offer = next_json(&mut stream).await;
  assert_eq!(offer["type"], "offer");

  // Find the peer id via the token-protected API.
  let (status, body) =
    http_request(srv.addr, "GET", &format!("/api/peers?token={}", srv.token)).await;
  assert_eq!(status, 200);
  let peers: serde_json::Value = body
    .rsplit("\r\n\r\n")
    .next()
    .and_then(|b| serde_json::from_str(b).ok())
    .expect("peers json");
  let id = peers[0]["id"].as_str().expect("peer id").to_owned();

  let (status, _) = http_request(
    srv.addr,
    "POST",
    &format!("/api/peers/{id}/disconnect?token={}", srv.token),
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
