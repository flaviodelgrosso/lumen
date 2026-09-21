//! `lumen` — stream your display to browsers on the LAN.

mod mdns;
mod network;
mod serve;

use std::io::IsTerminal;
use std::net::IpAddr;

use clap::{Parser, Subcommand};
use lumen_core::{EncoderPreference, Quality, StreamConfig, normalize_session_name};
use lumen_media::capture::{list_displays, list_windows};

#[derive(Parser)]
#[command(
  name = "lumen",
  version,
  about = "Stream your display to any browser on the local network",
  long_about = "Lumen captures a display (or window), encodes it as H.264 video \
                  plus Opus system audio and streams both to browsers over WebRTC. \
                  No viewer software required."
)]
struct Cli {
  #[command(subcommand)]
  command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
  /// Start the streaming server (default when no subcommand is given)
  Serve(ServeArgs),
  /// List capturable displays
  Displays,
  /// List capturable windows
  Windows,
}

#[derive(Parser)]
#[expect(
  clippy::struct_excessive_bools,
  reason = "clap flag struct: every bool is an independent on/off CLI flag"
)]
struct ServeArgs {
  /// Display id to capture (see `lumen displays`; default: primary display)
  #[arg(long, value_name = "id")]
  display: Option<u32>,

  /// Window id to capture (see `lumen windows`)
  #[arg(long, value_name = "id")]
  window: Option<u32>,

  /// LAN address to bind (default: auto-selected)
  #[arg(long, value_name = "ip")]
  bind: Option<IpAddr>,

  /// TCP port for the viewer/signaling server (default: 3131)
  #[arg(long, value_name = "port")]
  port: Option<u16>,

  /// Human-readable session name shown to viewers and on the host
  /// dashboard (display metadata only; max 64 characters)
  #[arg(long, value_name = "session-name")]
  name: Option<String>,

  /// Target capture/encode frame rate (default: 60)
  #[arg(long, value_name = "fps")]
  fps: Option<u32>,

  /// Video encoder backend: auto | software | hardware (default: auto)
  #[arg(long, value_name = "backend")]
  encoder: Option<String>,

  /// Encoding quality preset: low | medium | high | auto (default: auto)
  #[arg(long, value_name = "preset")]
  quality: Option<String>,

  /// Bitrate ceiling, e.g. 8000k or 2M (overrides the quality preset)
  #[arg(long, value_name = "rate")]
  max_bitrate: Option<String>,

  /// Admit viewers without a terminal prompt
  #[arg(long)]
  auto_accept: bool,

  /// Allow non-localhost clients to reach the /admin host dashboard
  #[arg(long)]
  allow_lan_admin: bool,

  /// Stream video only (no system audio)
  #[arg(long)]
  no_audio: bool,

  /// Do not print a QR code
  #[arg(long)]
  no_qr: bool,

  /// Debug logging plus periodic pipeline statistics
  #[arg(long)]
  verbose: bool,
}

fn main() -> anyhow::Result<()> {
  let cli = Cli::parse();
  match cli.command.unwrap_or(Command::Serve(ServeArgs {
    display: None,
    window: None,
    bind: None,
    port: None,
    fps: None,
    encoder: None,
    quality: None,
    max_bitrate: None,
    name: None,
    auto_accept: false,
    allow_lan_admin: false,
    no_audio: false,
    no_qr: false,
    verbose: false,
  })) {
    Command::Serve(args) => serve::run(args),
    Command::Displays => {
      let displays = list_displays()?;
      if displays.is_empty() {
        println!("No displays found.");
      }
      for (idx, d) in displays.iter().enumerate() {
        let primary = if idx == 0 { " (primary)" } else { "" };
        println!(
          "{}  {}{}  {}x{}",
          d.id, d.title, primary, d.dimensions.width, d.dimensions.height
        );
      }
      Ok(())
    }
    Command::Windows => {
      let windows = list_windows()?;
      if windows.is_empty() {
        println!("No capturable windows found.");
      }
      for w in windows {
        println!("{}  {}", w.id, w.title);
      }
      Ok(())
    }
  }
}

/// Parse quality/bitrate/fps/name flags into a validated [`StreamConfig`].
fn stream_config(args: &ServeArgs) -> anyhow::Result<StreamConfig> {
  let mut cfg = StreamConfig::default();
  if let Some(q) = &args.quality {
    cfg.quality = q.parse::<Quality>().map_err(|e| anyhow::anyhow!("{e}"))?;
  }
  if let Some(b) = &args.max_bitrate {
    cfg.max_bitrate = Some(
      b.parse()
        .map_err(|e: lumen_core::ConfigError| anyhow::anyhow!("{e}"))?,
    );
  }
  if let Some(f) = args.fps {
    cfg.fps = f;
  }
  if let Some(e) = &args.encoder {
    cfg.encoder = e
      .parse::<EncoderPreference>()
      .map_err(|e: lumen_core::ConfigError| anyhow::anyhow!("{e}"))?;
  }
  if let Some(n) = &args.name {
    cfg.session_name = normalize_session_name(n).map_err(|e| anyhow::anyhow!("{e}"))?;
  }
  cfg.audio = !args.no_audio;
  cfg.validate().map_err(|e| anyhow::anyhow!("{e}"))?;
  Ok(cfg)
}

/// Ask the user to pick between several plausible LAN interfaces.
fn pick_interface(
  interfaces: &[crate::network::LanInterface],
) -> anyhow::Result<crate::network::LanInterface> {
  if interfaces.len() == 1 {
    return Ok(interfaces[0].clone());
  }
  if !std::io::stdin().is_terminal() {
    anyhow::bail!(
      "multiple network interfaces found; choose one with --bind ({}); \
             non-interactive mode cannot prompt",
      interfaces
        .iter()
        .map(|i| i.ip.to_string())
        .collect::<Vec<_>>()
        .join(", ")
    );
  }
  let items: Vec<String> = interfaces
    .iter()
    .map(|i| format!("{}  ({})", i.ip, i.name))
    .collect();
  let idx = dialoguer::Select::new()
    .with_prompt("Which network should Lumen use?")
    .items(&items)
    .default(0)
    .interact()?;
  Ok(interfaces[idx].clone())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn serve_args_with(name: Option<&str>) -> ServeArgs {
    ServeArgs {
      display: None,
      window: None,
      bind: None,
      port: None,
      name: name.map(str::to_owned),
      fps: None,
      encoder: None,
      quality: None,
      max_bitrate: None,
      auto_accept: false,
      allow_lan_admin: false,
      no_audio: false,
      no_qr: false,
      verbose: false,
    }
  }

  #[test]
  fn cli_accepts_the_name_flag() {
    let cli = Cli::try_parse_from(["lumen", "serve", "--name", "Architecture Workshop"])
      .expect("--name must parse");
    let Some(Command::Serve(args)) = cli.command else {
      panic!("expected a serve command");
    };
    assert_eq!(args.name.as_deref(), Some("Architecture Workshop"));
  }

  #[test]
  fn name_flag_is_optional() {
    let cli = Cli::try_parse_from(["lumen", "serve"]).expect("serve must parse");
    let Some(Command::Serve(args)) = cli.command else {
      panic!("expected a serve command");
    };
    assert_eq!(args.name, None);
  }

  #[test]
  fn name_flows_into_shared_config() {
    let cfg =
      stream_config(&serve_args_with(Some("  Architecture  Workshop "))).expect("valid name");
    assert_eq!(cfg.session_name.as_deref(), Some("Architecture Workshop"));
  }

  #[test]
  fn no_name_keeps_the_default_config_unchanged() {
    assert_eq!(
      stream_config(&serve_args_with(None)).expect("default config"),
      StreamConfig::default()
    );
  }

  #[test]
  fn control_characters_never_reach_the_config() {
    let cfg = stream_config(&serve_args_with(Some("Team\u{a}\u{1b}[2J Briefing")))
      .expect("control characters are sanitized, not rejected");
    assert_eq!(cfg.session_name.as_deref(), Some("Team [2J Briefing"));
  }

  #[test]
  fn overlong_name_is_rejected_with_guidance() {
    let long = "x".repeat(lumen_core::MAX_SESSION_NAME_LEN + 1);
    let err =
      stream_config(&serve_args_with(Some(&long))).expect_err("overlong names must be rejected");
    assert!(err.to_string().contains("at most 64 characters"));
  }
}
