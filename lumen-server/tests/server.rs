//! Tests: tokenless entry, join/approval/grant flow, admin isolation,
//! signaling, kicking.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

use lumen_server::{
  ApprovalQueue, AuthDecision, Authorizer, PairingCode, PeerRegistry, ServerConfig, ServerHandle,
  SessionToken, StreamInfo, spawn_server,
};

struct TestServer {
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

#[derive(Clone, Debug)]
struct ViewerJoin {
  id: String,
  token: String,
}

impl std::fmt::Display for ViewerJoin {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.id)
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

/// `approve` decides immediately in the background; `false` leaves joins
/// pending so tests can decide through the admin API.
async fn start(approve: bool) -> TestServer {
  start_with_pairing(approve, None).await
}

async fn start_with_pairing(approve: bool, pairing: Option<PairingCode>) -> TestServer {
  start_with(approve, pairing, None).await
}

async fn start_with(
  approve: bool,
  pairing: Option<PairingCode>,
  session_name: Option<&str>,
) -> TestServer {
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
    admin_token: admin_token.clone(),
    admin_allow_lan: false,
    auto_accept_pairing: pairing,
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
      encoder_label: "fake".to_owned(),
      audio_label: "off".to_owned(),
      viewer_url: "http://lumen.local:0".to_owned(),
      session_name: session_name.map(str::to_owned),
    },
    stats: Arc::new(lumen_core::PipelineStats::default()),
  })
  .await
  .expect("server starts");
  TestServer {
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
  http_request_with(addr, method, path, None, None).await
}

async fn http_request_with(
  addr: SocketAddr,
  method: &str,
  path: &str,
  bearer: Option<&str>,
  body: Option<&str>,
) -> (u16, String) {
  let mut stream = TcpStream::connect(dialable(addr)).await.expect("connect");
  let authorization = bearer.map_or_else(String::new, |token| {
    format!("Authorization: Bearer {token}\r\n")
  });
  let payload = body.unwrap_or_default();
  let content = body.map_or_else(String::new, |payload| {
    format!(
      "Content-Type: application/json\r\nContent-Length: {}\r\n",
      payload.len()
    )
  });
  let req = format!(
    "{method} {path} HTTP/1.1\r\nHost: test\r\n{authorization}{content}Connection: close\r\n\r\n{payload}"
  );
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
    if let WsMessage::Text(text) = frame
      && let Ok(value) = serde_json::from_str(&text)
    {
      return value;
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

/// Create a join request and keep its private polling capability.
async fn join(srv: &TestServer) -> ViewerJoin {
  let (status, join) = join_with_pairing(srv, "").await;
  assert_eq!(status, 201, "join must park in the approval queue");
  join.expect("join response")
}

async fn join_with_pairing(srv: &TestServer, code: &str) -> (u16, Option<ViewerJoin>) {
  let body = if code.is_empty() {
    None
  } else {
    Some(serde_json::json!({"pairingCode": code}).to_string())
  };
  let (status, body) =
    http_request_with(srv.addr, "POST", "/api/join", None, body.as_deref()).await;
  let join = body
    .rsplit("\r\n\r\n")
    .next()
    .and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok())
    .and_then(|body| {
      Some(ViewerJoin {
        id: body["requestId"].as_str()?.to_owned(),
        token: body["joinToken"].as_str()?.to_owned(),
      })
    });
  (status, join)
}

async fn poll_join(srv: &TestServer, join: &ViewerJoin) -> serde_json::Value {
  let (status, body) = http_request_with(
    srv.addr,
    "GET",
    &format!("/api/join/{}", join.id),
    Some(&join.token),
    None,
  )
  .await;
  assert!(status == 200 || status == 404, "poll status {status}");
  body
    .rsplit("\r\n\r\n")
    .next()
    .and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok())
    .expect("poll json")
}

/// Poll until the join is approved; returns the grant.
async fn wait_approved(srv: &TestServer, join: &ViewerJoin) -> String {
  for _ in 0..100 {
    let json = poll_join(srv, join).await;
    match json["status"].as_str() {
      Some("approved") => {
        return json["grant"]
          .as_str()
          .expect("approved carries a grant")
          .to_owned();
      }
      Some("pending") => {}
      other => panic!("unexpected join status: {other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(25)).await;
  }
  panic!("join never approved");
}

/// Join and wait for the grant (auto-approved servers only).
async fn join_and_grant(srv: &TestServer) -> String {
  let join = join(srv).await;
  wait_approved(srv, &join).await
}

/// Poll the admin state until the pending list is non-empty; returns the
/// first pending entry.
async fn wait_pending_json(srv: &TestServer) -> serde_json::Value {
  for _ in 0..100 {
    let json = admin_state(srv).await;
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
async fn root_serves_the_viewer_with_no_redirect_and_no_secret() {
  let mut srv = start(true).await;
  let (status, body) = http_request(srv.addr, "GET", "/").await;
  assert_eq!(status, 200, "GET / serves the viewer directly");
  assert!(body.contains("<video"));
  assert!(
    !body.to_lowercase().contains("location:"),
    "GET / must not redirect to a tokenized URL"
  );
  // The old tokenized entrypoints are gone.
  let (status, _) = http_request(srv.addr, "GET", "/s/any-token").await;
  assert_eq!(status, 404);
  let (status, _) = http_request(srv.addr, "GET", "/api/session/any-token").await;
  assert_eq!(status, 404);
  srv.stop().await;
}

#[tokio::test]
async fn assets_and_health_served() {
  let mut srv = start(true).await;
  let (status, body) = http_request(srv.addr, "GET", "/viewer.js").await;
  assert_eq!(status, 200);
  assert!(body.contains("text/javascript"));
  let (status, _body) = http_request(srv.addr, "GET", "/viewer.css").await;
  assert_eq!(status, 200);
  let (status, _) = http_request(srv.addr, "GET", "/health").await;
  assert_eq!(status, 200);
  srv.stop().await;
}

#[tokio::test]
async fn join_polling_requires_its_own_join_token() {
  let mut srv = start(true).await;
  let join = join(&srv).await;
  let other_token = SessionToken::generate().expect("other token").to_string();
  let admin_token = srv.admin_token.to_string();

  for token in [
    None,
    Some("wrong-token"),
    Some(other_token.as_str()),
    Some(admin_token.as_str()),
  ] {
    let (status, _) = http_request_with(
      srv.addr,
      "GET",
      &format!("/api/join/{}", join.id),
      token,
      None,
    )
    .await;
    assert_eq!(status, 404, "only this join's token can poll it");
  }

  assert!(matches!(
    poll_join(&srv, &join).await["status"].as_str(),
    Some("pending" | "approved")
  ));
  assert!(
    ws_connect(srv.addr, &format!("/ws/{}", join.token))
      .await
      .is_err(),
    "a join token must not authenticate signaling"
  );
  for path in [
    format!("/admin/{}", join.token),
    format!("/api/admin/state?token={}", join.token),
  ] {
    let (status, _) = http_request(srv.addr, "GET", &path).await;
    assert_eq!(status, 404, "a join token is not an admin capability");
  }
  srv.stop().await;
}

#[tokio::test]
async fn default_mode_does_not_require_a_pairing_code() {
  let mut srv = start_manual().await;
  let (status, body) = http_request(srv.addr, "GET", "/api/join/config").await;
  assert_eq!(status, 200);
  assert!(body.contains(r#""pairingRequired":false"#));
  assert_eq!(join(&srv).await.id.len(), 16);
  srv.stop().await;
}

#[tokio::test]
async fn auto_accept_requires_and_throttles_pairing_codes() {
  let code = PairingCode::generate().expect("pairing code");
  let expected = code.to_string();
  let wrong = if expected == "000000" {
    "999999"
  } else {
    "000000"
  };
  let mut srv = start_with_pairing(false, Some(code)).await;
  let (status, body) = http_request(srv.addr, "GET", "/api/join/config").await;
  assert_eq!(status, 200);
  assert!(body.contains(r#""pairingRequired":true"#));

  assert_eq!(join_with_pairing(&srv, "").await.0, 403);
  assert_eq!(join_with_pairing(&srv, wrong).await.0, 429);
  tokio::time::sleep(Duration::from_millis(1_050)).await;

  let (status, join) = join_with_pairing(&srv, &expected).await;
  assert_eq!(status, 201);
  let grant = wait_approved(&srv, &join.expect("paired join")).await;
  assert!(
    ws_connect(srv.addr, &format!("/ws/{grant}")).await.is_ok(),
    "the correct pairing code admits an unattended viewer"
  );
  srv.stop().await;
}

#[tokio::test]
async fn unapproved_client_cannot_reach_signaling() {
  let mut srv = start_manual().await;
  let id = join(&srv).await;
  assert_eq!(poll_join(&srv, &id).await["status"], "pending");

  // The join is visible to the host but grants nothing on its own.
  let pending = wait_pending_json(&srv).await;
  assert_eq!(pending["id"], id.id);
  assert_eq!(pending["address"], "127.0.0.1");

  // No signaling without a redeemed grant.
  let err = ws_connect(srv.addr, "/ws/not-a-grant").await;
  assert!(err.is_err(), "handshake must fail without a grant");
  assert_eq!(srv.registry.count(), 0);
  assert_eq!(
    poll_join(&srv, &id).await["status"],
    "pending",
    "still no capability while pending"
  );
  srv.stop().await;
}

#[tokio::test]
async fn denied_viewer_cannot_obtain_or_use_a_grant() {
  let mut srv = start_manual().await;
  let id = join(&srv).await;
  let pending = wait_pending_json(&srv).await;
  let pending_id = pending["id"].as_str().expect("pending id").to_owned();

  let (status, _) = http_request(
    srv.addr,
    "POST",
    &format!("/api/admin/pending/{pending_id}/deny?{}", srv.admin_query()),
  )
  .await;
  assert_eq!(status, 200);

  let json = poll_join(&srv, &id).await;
  assert_eq!(json["status"], "denied");
  assert!(
    json.get("grant").is_none(),
    "a denied join must never carry a grant"
  );
  assert_eq!(poll_join(&srv, &id).await["status"], "denied");
  assert_eq!(srv.registry.count(), 0);
  srv.stop().await;
}

#[tokio::test]
async fn approved_viewer_gets_grant_and_offer() {
  let mut srv = start(true).await;
  let grant = join_and_grant(&srv).await;
  let mut stream = ws_connect(srv.addr, &format!("/ws/{grant}"))
    .await
    .expect("grant authenticates the socket");

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
async fn grant_is_single_use() {
  let mut srv = start(true).await;
  let grant = join_and_grant(&srv).await;
  let mut stream = ws_connect(srv.addr, &format!("/ws/{grant}"))
    .await
    .expect("first redeem works");
  let _offer = next_json(&mut stream).await;
  stream.close(None).await.ok();
  tokio::time::sleep(Duration::from_millis(200)).await;

  let second = ws_connect(srv.addr, &format!("/ws/{grant}")).await;
  assert!(
    second.is_err(),
    "a redeemed grant must not authenticate again"
  );
  srv.stop().await;
}

#[tokio::test]
async fn garbage_grant_fails_handshake() {
  let mut srv = start(true).await;
  for path in ["/ws/", "/ws/x", "/ws/not-the-grant"] {
    let err = ws_connect(srv.addr, path).await;
    assert!(err.is_err(), "{path} must not authenticate");
  }
  assert_eq!(srv.registry.count(), 0);
  srv.stop().await;
}

#[tokio::test]
async fn join_requests_are_bounded() {
  let mut srv = start_manual().await;
  // Per-IP bounds reject the third pending viewer before one source can
  // occupy the global approval queue.
  let (status, _) = http_request(srv.addr, "POST", "/api/join").await;
  assert_eq!(status, 201);
  tokio::time::sleep(Duration::from_millis(1_050)).await;
  let (status, _) = http_request(srv.addr, "POST", "/api/join").await;
  assert_eq!(status, 201);
  tokio::time::sleep(Duration::from_millis(1_050)).await;
  let (status, _) = http_request(srv.addr, "POST", "/api/join").await;
  assert_eq!(status, 429, "one address cannot fill the global queue");
  srv.stop().await;
}

#[tokio::test]
async fn host_approval_admits_without_a_pairing_code() {
  let mut srv = start(true).await;
  let id = join(&srv).await;
  let grant = wait_approved(&srv, &id).await;
  let mut stream = ws_connect(srv.addr, &format!("/ws/{grant}"))
    .await
    .expect("auto-accepted join streams");
  let offer = next_json(&mut stream).await;
  assert_eq!(offer["type"], "offer");
  stream.close(None).await.ok();
  srv.stop().await;
}

#[tokio::test]
async fn multiple_viewers_connect_independently() {
  let mut srv = start(true).await;
  let id_a = join(&srv).await;
  let grant_a = wait_approved(&srv, &id_a).await;
  tokio::time::sleep(Duration::from_millis(1_050)).await;
  let id_b = join(&srv).await;
  let grant_b = wait_approved(&srv, &id_b).await;
  assert_ne!(grant_a, grant_b, "grants are per-peer");

  let mut stream_a = ws_connect(srv.addr, &format!("/ws/{grant_a}"))
    .await
    .expect("viewer A connects");
  let mut stream_b = ws_connect(srv.addr, &format!("/ws/{grant_b}"))
    .await
    .expect("viewer B connects");
  assert_eq!(next_json(&mut stream_a).await["type"], "offer");
  assert_eq!(next_json(&mut stream_b).await["type"], "offer");
  assert_eq!(srv.registry.count(), 2);

  // Kicking A leaves B serving.
  let json = admin_state(&srv).await;
  let ids: Vec<String> = json["peers"]
    .as_array()
    .expect("peers array")
    .iter()
    .filter_map(|p| p["id"].as_str().map(str::to_owned))
    .collect();
  assert_eq!(ids.len(), 2);
  assert!(
    ids.contains(&id_a.id) && ids.contains(&id_b.id),
    "the admin state exposes the join ids as peer ids"
  );
  let (status, _) = http_request(
    srv.addr,
    "POST",
    &format!("/api/admin/peers/{id_a}/disconnect?{}", srv.admin_query()),
  )
  .await;
  assert_eq!(status, 200);
  let _msg = next_json_type(&mut stream_a, "disconnected").await;
  // The real viewer closes its socket on the notice (viewer.js does).
  stream_a.close(None).await.ok();
  for _ in 0..100 {
    if srv.registry.count() == 1 {
      break;
    }
    tokio::time::sleep(Duration::from_millis(25)).await;
  }
  assert_eq!(srv.registry.count(), 1, "kicking A leaves B connected");
  let (status, _) = http_request(srv.addr, "GET", "/health").await;
  assert_eq!(status, 200);

  stream_b.close(None).await.ok();
  srv.stop().await;
}

#[tokio::test]
async fn viewer_grant_cannot_access_admin_apis() {
  let mut srv = start(true).await;
  let grant = join_and_grant(&srv).await;

  // The admin dashboard and every API reject the viewer grant and no token.
  for path in [
    format!("/admin/{grant}"),
    format!("/api/admin/state?token={grant}"),
    "/api/admin/state".to_owned(),
    format!("/api/admin/qr?token={grant}"),
    "/api/admin/qr".to_owned(),
  ] {
    let (status, _) = http_request(srv.addr, "GET", &path).await;
    assert_eq!(status, 404, "{path} must not be reachable with a grant");
  }
  let (status, _) = http_request(
    srv.addr,
    "POST",
    &format!("/api/admin/pending/deadbeef/allow?token={grant}"),
  )
  .await;
  assert_eq!(status, 404);
  let (status, _) = http_request(
    srv.addr,
    "POST",
    &format!("/api/admin/peers/deadbeef/disconnect?token={grant}"),
  )
  .await;
  assert_eq!(status, 404);
  srv.stop().await;
}

#[tokio::test]
async fn admin_token_is_not_a_signaling_grant() {
  let mut srv = start(true).await;
  let err = ws_connect(srv.addr, &format!("/ws/{}", srv.admin_token)).await;
  assert!(
    err.is_err(),
    "the admin token must not authenticate the signaling socket"
  );
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
  assert_eq!(
    json["encoder"].as_str(),
    Some("fake"),
    "the selected encoder backend must be reported to the dashboard"
  );
  assert_eq!(
    json["viewerUrl"].as_str(),
    Some("http://lumen.local:0"),
    "the canonical viewer URL is the stable entrypoint, no token path"
  );
  assert!(
    !json["viewerUrl"]
      .as_str()
      .is_some_and(|u| u.contains("/s/"))
  );
  assert!(json["pending"].is_array());
  assert!(json["peers"].is_array());

  let (status, _) = http_request(srv.addr, "GET", "/admin/wrong-token").await;
  assert_eq!(status, 404);
  srv.stop().await;
}

#[tokio::test]
async fn admin_can_approve_pending_viewer() {
  let mut srv = start_manual().await;
  let id = join(&srv).await;

  let pending = wait_pending_json(&srv).await;
  let pending_id = pending["id"].as_str().expect("pending id").to_owned();
  assert_eq!(pending_id, id.id);

  let (status, _) = http_request(
    srv.addr,
    "POST",
    &format!(
      "/api/admin/pending/{pending_id}/allow?{}",
      srv.admin_query()
    ),
  )
  .await;
  assert_eq!(status, 200);

  let grant = wait_approved(&srv, &id).await;
  let mut stream = ws_connect(srv.addr, &format!("/ws/{grant}"))
    .await
    .expect("approved join connects");
  let offer = next_json_type(&mut stream, "offer").await;
  assert!(offer["sdp"].as_str().is_some());

  // The decided request leaves the pending list.
  let json = admin_state(&srv).await;
  assert_eq!(json["pending"].as_array().map(Vec::len), Some(0));
  stream.close(None).await.ok();
  srv.stop().await;
}

#[tokio::test]
async fn admin_can_disconnect_viewer_without_stopping_server() {
  let mut srv = start(true).await;
  let grant = join_and_grant(&srv).await;
  let mut stream = ws_connect(srv.addr, &format!("/ws/{grant}"))
    .await
    .expect("ws connects");
  let offer = next_json(&mut stream).await;
  assert_eq!(offer["type"], "offer");

  // The peer id is the join request id.
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
  let grant = join_and_grant(&srv).await;
  let mut stream = ws_connect(srv.addr, &format!("/ws/{grant}"))
    .await
    .expect("ws connects");
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

#[tokio::test]
async fn join_config_publishes_the_session_name() {
  let mut srv = start_with(true, None, Some("Architecture Workshop")).await;
  let (status, body) = http_request(srv.addr, "GET", "/api/join/config").await;
  assert_eq!(status, 200);
  let json: serde_json::Value =
    serde_json::from_str(body.rsplit("\r\n\r\n").next().unwrap()).unwrap();
  assert_eq!(json["sessionName"], "Architecture Workshop");
  assert_eq!(json["pairingRequired"], false);
  srv.stop().await;
}

#[tokio::test]
async fn join_config_reports_no_session_name_when_unset() {
  let mut srv = start(true).await;
  let (status, body) = http_request(srv.addr, "GET", "/api/join/config").await;
  assert_eq!(status, 200);
  let json: serde_json::Value =
    serde_json::from_str(body.rsplit("\r\n\r\n").next().unwrap()).unwrap();
  assert!(json["sessionName"].is_null(), "no name → JSON null");
  srv.stop().await;
}

#[tokio::test]
async fn session_name_is_inert_json_text_never_markup() {
  // The public join-config endpoint must carry hostile names as plain
  // JSON string data (the viewer renders them with textContent): the
  // transported value equals the input exactly, with no HTML wrapping.
  let hostile = "<script>alert(1)</script>";
  let mut srv = start_with(true, None, Some(hostile)).await;
  let (status, body) = http_request(srv.addr, "GET", "/api/join/config").await;
  assert_eq!(status, 200);
  assert!(
    body.to_ascii_lowercase().contains("application/json"),
    "join config stays a JSON document, never an HTML fragment"
  );
  let json: serde_json::Value =
    serde_json::from_str(body.rsplit("\r\n\r\n").next().unwrap()).unwrap();
  assert_eq!(json["sessionName"].as_str(), Some(hostile));
  srv.stop().await;
}

#[tokio::test]
async fn session_name_never_reaches_join_secrets_or_the_viewer_url() {
  let marker = "WorkshopSecretName";
  let mut srv = start_with(false, None, Some(marker)).await;

  // The join response (request id + polling token) carries no name.
  let (status, body) = http_request(srv.addr, "POST", "/api/join").await;
  assert_eq!(status, 201);
  assert!(
    !body.contains(marker),
    "join response must not leak the name"
  );
  let json: serde_json::Value =
    serde_json::from_str(body.rsplit("\r\n\r\n").next().unwrap()).unwrap();
  let viewer = ViewerJoin {
    id: json["requestId"].as_str().unwrap().to_owned(),
    token: json["joinToken"].as_str().unwrap().to_owned(),
  };

  // Approving through the admin surface yields a grant that also carries
  // no name, and the canonical viewer URL stays name-free.
  let pending = wait_pending_json(&srv).await;
  let pending_id = pending["id"].as_str().expect("pending id").to_owned();
  let (status, _) = http_request(
    srv.addr,
    "POST",
    &format!(
      "/api/admin/pending/{pending_id}/allow?{}",
      srv.admin_query()
    ),
  )
  .await;
  assert_eq!(status, 200);
  let grant = wait_approved(&srv, &viewer).await;
  assert!(!grant.contains(marker), "grants must not embed the name");
  let state = admin_state(&srv).await;
  assert!(!state["viewerUrl"].as_str().unwrap().contains(marker));
  assert_eq!(state["sessionName"], marker, "the name is state metadata");
  srv.stop().await;
}

#[tokio::test]
async fn named_session_still_requires_host_approval() {
  // A session name is display metadata: the approval flow is unchanged.
  let mut srv = start_with(false, None, Some("Architecture Workshop")).await;
  let viewer = join(&srv).await;
  let pending = wait_pending_json(&srv).await;
  assert_eq!(pending["id"].as_str().expect("pending id"), viewer.id);
  let before = poll_join(&srv, &viewer).await;
  assert_eq!(before["status"], "pending", "the name admits nobody");
  let (status, _) = http_request(
    srv.addr,
    "POST",
    &format!(
      "/api/admin/pending/{}/allow?{}",
      pending["id"].as_str().unwrap(),
      srv.admin_query()
    ),
  )
  .await;
  assert_eq!(status, 200);
  let grant = wait_approved(&srv, &viewer).await;
  let mut stream = ws_connect(srv.addr, &format!("/ws/{grant}"))
    .await
    .expect("approval still works with a name set");
  let offer = next_json_type(&mut stream, "offer").await;
  assert!(offer["sdp"].as_str().is_some());
  stream.close(None).await.ok();
  srv.stop().await;
}
