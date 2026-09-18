//! `lumen serve`: capture → encode → WebRTC fan-out, with prompts and QR.

use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::ServeArgs;
use crate::pick_interface;
use crate::stream_config;
use lumen_capture::{
  AudioCaptureSource, CaptureError, CaptureSource, ScapAudioCapture, ScapCapture, list_displays,
};
use lumen_core::StreamConfig;
use lumen_encoder::{AudioEncoder, OpenH264Encoder, OpusAudioEncoder};
use lumen_network::LanInterface;
use lumen_server::{ServerConfig, spawn_server};
use lumen_session::{AuthDecision, Authorizer, PeerRegistry, SessionToken, describe_user_agent};

use lumen_cli::pipeline;

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

  // ── capture ──
  let mut capture = match args.window {
    Some(id) => ScapCapture::for_window(id, cfg.fps)?,
    None => ScapCapture::for_display(args.display, cfg.fps)?,
  };
  let dims = capture.dimensions();
  capture.start()?;
  let source: Box<dyn CaptureSource> = Box::new(capture);

  // ── encoder ──
  let bitrate = cfg.effective_bitrate(dims);
  let keyframe_frames = cfg
    .fps
    .saturating_mul(u32::try_from(cfg.keyframe_interval_secs).unwrap_or(1))
    .max(1);
  let encoder = OpenH264Encoder::new(dims, cfg.fps, bitrate, keyframe_frames)?;

  // ── network ──
  let interfaces = lumen_network::discover()?;
  let iface = match args.bind {
    Some(ip) => LanInterface {
      name: "manual".into(),
      ip,
    },
    None => pick_interface(&lumen_network::select(&interfaces, None)?)?,
  };

  // Preflight: resolving the `.local` ICE candidates Chromium browsers
  // advertise requires sending IPv4 multicast. A failure here is fatal
  // for those viewers (Safari offers plain IPs and still works), so
  // name the usual cause instead of leaving only webrtc-rs errors.
  if let Err(source) = lumen_network::probe_multicast() {
    tracing::warn!(
      "IPv4 multicast send failed ({source}); Chromium-based viewers \
             (mDNS-obfuscated ICE candidates) will not connect.\n{}",
      multicast_remediation()
    );
  }

  // ── session + services ──
  let token = SessionToken::generate()?;
  let (authorizer, approvals) = Authorizer::channel(16);
  let registry = Arc::new(PeerRegistry::new());
  let (kf_tx, kf_rx) = mpsc::channel(64);
  let shutdown = CancellationToken::new();
  let audio = build_audio(&cfg);

  let pipeline = pipeline::start(
    source,
    Box::new(encoder),
    cfg.fps,
    cfg.keyframe_interval_secs,
    shutdown.clone(),
    kf_rx,
    audio,
  )?;

  let server = spawn_server(ServerConfig {
    port: args.port.unwrap_or(3131),
    token: token.clone(),
    authorizer,
    registry: Arc::clone(&registry),
    frames: pipeline.frames.clone(),
    audio: pipeline.audio.clone(),
    keyframe_requests: kf_tx,
    shutdown: shutdown.clone(),
    webrtc_bind: Vec::new(),
  })
  .await?;

  let url = Url::parse(&format!("http://{}:{}", iface.ip, server.addr.port()))
    .expect("valid url")
    .join(&format!("/s/{token}"))
    .expect("valid path");

  print_banner(
    &args,
    &cfg,
    pipeline.audio.is_some(),
    &iface,
    server.addr.port(),
    dims,
    &url,
  );

  let stats_task = args.verbose.then(|| {
    spawn_stats_loop(
      Arc::clone(&pipeline.stats),
      Arc::clone(&registry),
      cfg,
      dims,
      shutdown.clone(),
    )
  });

  spawn_approval_loop(approvals, args.auto_accept, shutdown.clone());

  tokio::signal::ctrl_c()
    .await
    .context("failed to install Ctrl+C handler")?;
  println!("\nShutting down...");
  shutdown.cancel();

  server.join().await?;
  pipeline.shutdown().await;
  if let Some(task) = stats_task {
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
  }
  println!("Stopped.");
  Ok(())
}

/// Build the system-audio capture + encoder pair, or `None` (with a warning)
/// when audio is disabled or unavailable — the stream then carries video only.
fn build_audio(cfg: &StreamConfig) -> Option<(Box<dyn AudioCaptureSource>, Box<dyn AudioEncoder>)> {
  if !cfg.audio {
    return None;
  }
  let source = match ScapAudioCapture::new() {
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

fn print_banner(
  args: &ServeArgs,
  cfg: &StreamConfig,
  audio_enabled: bool,
  iface: &LanInterface,
  port: u16,
  dims: lumen_core::Dimensions,
  url: &Url,
) {
  let display_label = if let Some(id) = args.window {
    format!("Window {id} — {}x{}", dims.width, dims.height)
  } else {
    let name = args.display.map_or_else(
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
  };
  println!();
  println!("Lumen screen sharing server started.");
  println!();
  println!("Display:      {display_label}");
  println!(
    "Quality:      {} ({} fps, max {})",
    cfg.quality,
    cfg.fps,
    cfg.effective_bitrate(dims)
  );
  println!(
    "Audio:        {}",
    if audio_enabled {
      format!("system audio — Opus, {} stereo", cfg.audio_bitrate)
    } else if cfg.audio {
      "unavailable — video only".to_owned()
    } else {
      "off (--no-audio)".to_owned()
    }
  );
  println!("Listening on: {}:{}", iface.ip, port);
  println!();
  println!("Open:");
  println!("{url}");
  println!();
  if !args.no_qr {
    match QrCode::new(url.as_str()) {
      Ok(code) => println!("{}", code.render::<Dense1x2>().build()),
      Err(e) => tracing::warn!("could not render QR code: {e}"),
    }
  }
  println!("Waiting for devices... (Ctrl+C to stop)");
}

fn spawn_approval_loop(
  mut approvals: mpsc::Receiver<lumen_session::AuthRequest>,
  auto_accept: bool,
  shutdown: CancellationToken,
) {
  let prompt_lock = Arc::new(tokio::sync::Mutex::new(()));
  tokio::spawn(async move {
    loop {
      let req = tokio::select! {
          () = shutdown.cancelled() => break,
          got = approvals.recv() => match got { Some(r) => r, None => break },
      };
      let ip = req
        .peer
        .address
        .map_or_else(|| "unknown".to_owned(), |a| a.to_string());
      let ua = describe_user_agent(req.peer.user_agent.as_deref());
      if auto_accept {
        println!("Viewer joined: {ip} ({ua}) [auto-accepted]");
        let _ = req.respond.send(AuthDecision::Allow);
        continue;
      }
      if !std::io::stdin().is_terminal() {
        println!(
          "Connection from {ip} ({ua}) denied: terminal is not interactive \
                     (run with --auto-accept to skip prompts)"
        );
        let _ = req.respond.send(AuthDecision::Deny);
        continue;
      }
      let _guard = prompt_lock.lock().await;
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
      let _ = req.respond.send(if allowed {
        AuthDecision::Allow
      } else {
        AuthDecision::Deny
      });
      println!(
        "{}",
        if allowed {
          "Connection allowed."
        } else {
          "Connection denied."
        }
      );
    }
  });
}

#[expect(
  clippy::cast_precision_loss,
  reason = "displayed fps/latency stats; counter magnitudes are far below 2^53"
)]
fn spawn_stats_loop(
  stats: Arc<pipeline::PipelineStats>,
  registry: Arc<PeerRegistry>,
  cfg: StreamConfig,
  dims: lumen_core::Dimensions,
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
        cfg.effective_bitrate(dims),
      );
      prev_captured = captured;
      prev_encoded = encoded;
      prev_audio = audio_packets;
    }
  })
}
