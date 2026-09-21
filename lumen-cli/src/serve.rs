//! `lumen serve`: terminal presentation for the shared Lumen session.
//!
//! The capture → encode → WebRTC → HTTP pipeline lives in `lumen-session`;
//! this module only adds what belongs to a terminal: argument mapping, the
//! startup banner, QR code, the interactive approval prompt, the stats loop
//! and Ctrl+C shutdown.

use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;
use tokio_util::sync::CancellationToken;

use crate::ServeArgs;
use crate::pick_interface;
use crate::stream_config;
use lumen_core::{Dimensions, PipelineStats};
use lumen_server::{ApprovalQueue, AuthDecision, PeerRegistry, describe_user_agent};
use lumen_session::Session;
use lumen_session::network::LanInterface;
use lumen_session::session::{DEFAULT_PORT, SessionConfig, SessionInfo};
use lumen_session::start_session;

/// Run `lumen serve` to completion (returns after Ctrl+C).
///
/// # Errors
///
/// Returns an error with corrective guidance when capture, network, or
/// server startup fails.
pub fn run(args: ServeArgs) -> anyhow::Result<()> {
  let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
    if args.verbose {
      "info,lumen=debug,webrtc=warn,rtc=warn".into()
    } else {
      "warn,lumen=info".into()
    }
  });
  tracing_subscriber::fmt()
    .with_env_filter(filter)
    .with_target(false)
    .without_time()
    .init();

  let runtime = tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .build()
    .context("failed to start async runtime")?;
  runtime.block_on(serve(args))
}

async fn serve(args: ServeArgs) -> anyhow::Result<()> {
  let cfg = stream_config(&args)?;
  // The interface choice may need an interactive prompt, so it stays here;
  // the session receives the resolved address.
  let iface = match args.bind {
    Some(ip) => LanInterface {
      name: "manual".into(),
      ip,
    },
    None => pick_interface(&lumen_session::network::select(
      &lumen_session::network::discover()?,
      None,
    )?)?,
  };

  let session = start_session(SessionConfig {
    stream: cfg,
    display: args.display,
    window: args.window,
    bind: Some(iface.ip),
    port: args.port.unwrap_or(DEFAULT_PORT),
    auto_accept: args.auto_accept,
    allow_lan_admin: args.allow_lan_admin,
    mdns: true,
    webrtc_bind: Vec::new(),
  })
  .await?;

  print_banner(&args, &session.info);

  let stats_task = args.verbose.then(|| {
    spawn_stats_loop(
      Arc::clone(session.stats()),
      Arc::clone(session.registry()),
      session.info.stream.bitrate_label.clone(),
      session.info.dims,
      session.shutdown_token(),
    )
  });

  // The terminal prompt is one approval surface; the host dashboard is
  // another. Without an interactive stdin the dashboard carries approvals.
  if !args.auto_accept && std::io::stdin().is_terminal() {
    spawn_approval_loop(session.approvals(), session.shutdown_token());
  }

  wait_shutdown(session, stats_task).await
}

/// Graceful shutdown: hand the session its teardown, then reap the stats loop.
async fn wait_shutdown(
  session: Session,
  stats_task: Option<tokio::task::JoinHandle<()>>,
) -> anyhow::Result<()> {
  tokio::signal::ctrl_c()
    .await
    .context("failed to install Ctrl+C handler")?;
  println!("\nShutting down...");
  session.shutdown().await?;
  if let Some(task) = stats_task {
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
  }
  println!("Stopped.");
  Ok(())
}

fn print_banner(args: &ServeArgs, info: &SessionInfo) {
  println!();
  println!("Lumen screen sharing server started.");
  println!();
  if let Some(name) = &info.stream.session_name {
    println!("Session:      {name}");
  }
  println!("Display:      {}", info.stream.source_label);
  println!(
    "Quality:      {} ({} fps, max {})",
    info.stream.quality, info.stream.target_fps, info.stream.bitrate_label
  );
  println!("Audio:        {}", info.stream.audio_label);
  println!("Encoder:      {}", info.stream.encoder_label);
  println!("Listening on: {}:{}", info.iface.ip, info.port);
  println!();
  println!("Open on any device:");
  println!();
  println!("  {}", info.viewer_url);
  println!();
  if let Some(pairing_code) = &info.pairing_code {
    println!("Pairing code:");
    println!();
    println!("  {} {}", &pairing_code[..3], &pairing_code[3..]);
    println!();
  }
  if let Some(fallback) = &info.fallback_url {
    println!("IP fallback:");
    println!();
    println!("  {fallback}");
    println!();
  }
  println!("Host dashboard:");
  println!("{}", info.admin_url);
  if let Some(lan_admin) = &info.lan_admin_url {
    println!("LAN admin access enabled (--allow-lan-admin):");
    println!("{lan_admin}");
  }
  println!();
  if !args.no_qr {
    // The QR carries the canonical entrypoint: the stable hostname when
    // mDNS is up, the IP URL otherwise.
    match QrCode::new(&info.viewer_url) {
      Ok(code) => println!("{}", code.render::<Dense1x2>().build()),
      Err(e) => tracing::warn!("could not render QR code: {e}"),
    }
  }
  println!("Waiting for devices... (Ctrl+C to stop)");
}

/// Terminal approval surface: prompt for each pending viewer. The admin
/// dashboard shares the same queue; whichever surface answers first wins.
fn spawn_approval_loop(queue: ApprovalQueue, shutdown: CancellationToken) {
  let prompt_lock = Arc::new(tokio::sync::Mutex::new(()));
  tokio::spawn(async move {
    loop {
      let waiter = queue.wait_for_change();
      tokio::pin!(waiter);
      waiter.as_mut().enable();
      if let Some(req) = queue.poll().into_iter().next() {
        let ip = req
          .peer
          .address
          .map_or_else(|| "unknown".to_owned(), |a| a.to_string());
        let ua = describe_user_agent(req.peer.user_agent.as_deref());
        let _guard = prompt_lock.lock().await;
        // The dashboard may have decided while this iteration was queued.
        if queue.poll().iter().all(|pending| pending.id != req.id) {
          continue;
        }
        println!("\nIncoming device:\n");
        println!("  IP:       {ip}");
        println!("  Browser:  {ua}");
        println!();
        let allowed = tokio::task::spawn_blocking(|| {
          dialoguer::Confirm::new()
            .with_prompt("Allow connection?")
            .default(true)
            .interact()
            .unwrap_or(false)
        })
        .await
        .unwrap_or(false);
        if queue.decide(
          req.id,
          if allowed {
            AuthDecision::Allow
          } else {
            AuthDecision::Deny
          },
        ) {
          println!(
            "{}",
            if allowed {
              "Connection allowed."
            } else {
              "Connection denied."
            }
          );
        } else {
          println!("Connection already handled from the host dashboard.");
        }
        continue;
      }
      tokio::select! {
          () = shutdown.cancelled() => break,
          () = waiter => {}
      }
    }
  });
}

#[expect(
  clippy::cast_precision_loss,
  reason = "displayed fps/latency stats; counter magnitudes are far below 2^53"
)]
fn spawn_stats_loop(
  stats: Arc<PipelineStats>,
  registry: Arc<PeerRegistry>,
  bitrate_label: String,
  dims: Dimensions,
  shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
  tokio::spawn(async move {
    let mut prev_captured = 0_u64;
    let mut prev_encoded = 0_u64;
    let mut prev_audio = 0_u64;
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
      tokio::select! {
          () = shutdown.cancelled() => break,
          _ = tick.tick() => {}
      }
      let captured = stats.captured.load(Ordering::Relaxed);
      let encoded = stats.encoded.load(Ordering::Relaxed);
      let audio_packets = stats.audio_encoded.load(Ordering::Relaxed);
      let latency_us = stats.encode_latency_us.load(Ordering::Relaxed);
      println!(
        "[stats] capture {:.1} fps | encoded {:.1} fps | dropped {} | \
                 audio {:.1} pkt/s | encode latency {:.1} ms | viewers {} | \
                 queue {}/{} | {}x{} @ {}",
        (captured - prev_captured) as f64 / 5.0,
        (encoded - prev_encoded) as f64 / 5.0,
        captured.saturating_sub(encoded),
        (audio_packets - prev_audio) as f64 / 5.0,
        latency_us as f64 / 1000.0,
        registry.count(),
        // Queue depth is not directly observable on broadcast::Sender;
        // subscribers show fan-out instead.
        stats.subscribers.load(Ordering::Relaxed),
        stats.fanout_capacity.load(Ordering::Relaxed),
        dims.width,
        dims.height,
        bitrate_label,
      );
      prev_captured = captured;
      prev_encoded = encoded;
      prev_audio = audio_packets;
    }
  })
}
