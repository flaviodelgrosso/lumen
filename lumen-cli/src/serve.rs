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

use crate::ServeArgs;
use crate::network::LanInterface;
use crate::pick_interface;
use crate::stream_config;
use lumen_core::StreamConfig;
use lumen_media::capture::{
  AudioCaptureSource, CaptureError, CaptureSource, PlatformAudioCapture, PlatformCapture,
  list_displays,
};
use lumen_media::encoder::{AudioEncoder, OpenH264Encoder, OpusAudioEncoder};
use lumen_server::{
  ApprovalQueue, AuthDecision, Authorizer, PairingCode, PeerRegistry, ServerConfig, ServerHandle,
  SessionToken, StreamInfo, describe_user_agent, spawn_server,
};

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
    Some(id) => PlatformCapture::for_window(id, cfg.fps)?,
    None => PlatformCapture::for_display(args.display, cfg.fps)?,
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
  let interfaces = crate::network::discover()?;
  let iface = match args.bind {
    Some(ip) => LanInterface {
      name: "manual".into(),
      ip,
    },
    None => pick_interface(&crate::network::select(&interfaces, None)?)?,
  };

  // Preflight: resolving the `.local` ICE candidates Chromium browsers
  // advertise requires sending IPv4 multicast. A failure here is fatal
  // for those viewers (Safari offers plain IPs and still works), so
  // name the usual cause instead of leaving only webrtc-rs errors.
  if let Err(source) = crate::network::probe_multicast() {
    tracing::warn!(
      "IPv4 multicast send failed ({source}); Chromium-based viewers \
             (mDNS-obfuscated ICE candidates) will not connect.\n{}",
      multicast_remediation()
    );
  }

  // ── session + services ──
  let admin_token = SessionToken::generate()?;
  let pairing_code = pairing_code(args.auto_accept)?;
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

  let port = args.port.unwrap_or(3131);

  let mdns = advertise_mdns(port, iface.ip);
  let viewer_url = crate::mdns::viewer_url(mdns.is_some(), iface.ip, port);
  let stream = build_stream_info(&args, &cfg, dims, &viewer_url, pipeline.audio.is_some());

  let server = spawn_server(ServerConfig {
    port,
    admin_token: admin_token.clone(),
    admin_allow_lan: args.allow_lan_admin,
    auto_accept_pairing: pairing_code.clone(),
    authorizer,
    approvals: approvals.clone(),
    registry: Arc::clone(&registry),
    frames: pipeline.frames.clone(),
    audio: pipeline.audio.clone(),
    keyframe_requests: kf_tx,
    shutdown: shutdown.clone(),
    webrtc_bind: Vec::new(),
    stream: stream.clone(),
    stats: Arc::clone(&pipeline.stats),
  })
  .await?;

  print_banner(
    &args,
    &iface,
    server.addr.port(),
    &admin_token,
    pairing_code.as_ref(),
    &stream,
    mdns.is_some(),
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

  // The terminal prompt is one approval surface; the host dashboard is
  // another. Without an interactive stdin the dashboard carries approvals.
  if !args.auto_accept && std::io::stdin().is_terminal() {
    spawn_approval_loop(approvals, shutdown.clone());
  }

  wait_shutdown(server, pipeline, stats_task, shutdown, mdns).await
}

/// Graceful shutdown: cancel, drain the server, stop the pipeline, reap the
/// stats loop.
async fn wait_shutdown(
  server: ServerHandle,
  pipeline: pipeline::PipelineHandle,
  stats_task: Option<tokio::task::JoinHandle<()>>,
  shutdown: CancellationToken,
  mdns: Option<crate::mdns::MdnsGuard>,
) -> anyhow::Result<()> {
  tokio::signal::ctrl_c()
    .await
    .context("failed to install Ctrl+C handler")?;
  println!("\nShutting down...");
  shutdown.cancel();

  server.join().await?;
  if let Some(guard) = mdns {
    guard.stop();
  }
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
fn source_label(args: &ServeArgs, dims: lumen_core::Dimensions) -> String {
  if let Some(id) = args.window {
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

fn advertise_mdns(port: u16, ip: std::net::IpAddr) -> Option<crate::mdns::MdnsGuard> {
  crate::mdns::advertise(port, ip).map_or_else(
    |error| {
      tracing::warn!(
        "could not announce {} via mDNS ({error}); viewers must use {}.\n{}",
        crate::mdns::HOSTNAME,
        crate::mdns::fallback_url(ip, port),
        multicast_remediation()
      );
      None
    },
    Some,
  )
}

fn pairing_code(auto_accept: bool) -> anyhow::Result<Option<PairingCode>> {
  auto_accept
    .then(PairingCode::generate)
    .transpose()
    .map_err(Into::into)
}

/// Assemble the dashboard/banner stream description from resolved config.
fn build_stream_info(
  args: &ServeArgs,
  cfg: &StreamConfig,
  dims: lumen_core::Dimensions,
  viewer_url: &str,
  audio_enabled: bool,
) -> StreamInfo {
  StreamInfo {
    source_label: source_label(args, dims),
    width: dims.width,
    height: dims.height,
    target_fps: cfg.fps,
    quality: cfg.quality.to_string(),
    bitrate_label: cfg.effective_bitrate(dims).to_string(),
    audio_label: audio_label(cfg, audio_enabled),
    viewer_url: viewer_url.to_owned(),
  }
}

fn print_banner(
  args: &ServeArgs,
  iface: &LanInterface,
  port: u16,
  admin_token: &SessionToken,
  pairing_code: Option<&PairingCode>,
  stream: &StreamInfo,
  mdns_ok: bool,
) {
  println!();
  println!("Lumen screen sharing server started.");
  println!();
  println!("Display:      {}", stream.source_label);
  println!(
    "Quality:      {} ({} fps, max {})",
    stream.quality, stream.target_fps, stream.bitrate_label
  );
  println!("Audio:        {}", stream.audio_label);
  println!("Listening on: {}:{}", iface.ip, port);
  println!();
  println!("Open on any device:");
  println!();
  println!("  {}", stream.viewer_url);
  println!();
  if let Some(pairing_code) = pairing_code {
    let code = pairing_code.to_string();
    println!("Pairing code:");
    println!();
    println!("  {} {}", &code[..3], &code[3..]);
    println!();
  }
  if mdns_ok {
    println!("IP fallback:");
    println!();
    println!("  {}", crate::mdns::fallback_url(iface.ip, port));
    println!();
  }
  println!("Host dashboard:");
  println!("http://127.0.0.1:{port}/admin/{admin_token}");
  if args.allow_lan_admin {
    println!("LAN admin access enabled (--allow-lan-admin):");
    println!("http://{}:{port}/admin/{admin_token}", iface.ip);
  }
  println!();
  if !args.no_qr {
    // The QR carries the canonical entrypoint: the stable hostname when
    // mDNS is up, the IP URL otherwise.
    match QrCode::new(&stream.viewer_url) {
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
  stats: Arc<lumen_core::PipelineStats>,
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
