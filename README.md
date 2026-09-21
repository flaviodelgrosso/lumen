<div align="center">

# 📽️ Lumen

**Share your screen and system audio to any browser on your local network.**

No viewer app. No account. No cloud.

[![CI](https://img.shields.io/github/actions/workflow/status/flaviodelgrosso/lumen/ci.yml?style=flat-square&label=CI)](https://github.com/flaviodelgrosso/lumen/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/flaviodelgrosso/lumen?style=flat-square)](https://github.com/flaviodelgrosso/lumen/releases)
![Rust](https://img.shields.io/badge/Rust-1.85%2B-dea584?style=flat-square&logo=rust&logoColor=black)
![Platform](https://img.shields.io/badge/host-macOS%20ARM64%20%7C%20Windows%20x64-666?style=flat-square)
[![License](https://img.shields.io/badge/license-MIT-yellow?style=flat-square)](LICENSE)

[Features](#features) • [Quick start](#quick-start) • [Usage](#usage) • [How it works](#how-it-works) • [Security](#security) • [Development](#development)

</div>

Lumen turns any modern browser on your LAN into a wireless display.

Run one command on your Mac or PC, scan the QR code from another device, approve it, and start watching. Video and system audio are captured natively, encoded locally and streamed directly over WebRTC.

```text
Screen / window ──► H.264 ──┐
                            ├──► WebRTC / SRTP ──► Browser
System audio ─────► Opus ───┘
```

## Features

- **Browser as the receiver** — phones, tablets, laptops and other devices only need a modern browser.
- **Native screen capture** — ScreenCaptureKit on macOS and Windows Graphics Capture on Windows.
- **System audio** — streamed as Opus alongside the video.
- **Hardware H.264 encoding** — VideoToolbox on macOS and Media Foundation on Windows, with OpenH264 fallback.
- **Multiple viewers** — one encoder feeds independent WebRTC connections without re-encoding per device.
- **Low-latency pipeline** — stale video frames are dropped instead of building up latency.
- **Simple discovery** — open `lumen.local:3131`, use the printed LAN address, or scan the QR code.
- **Controlled access** — approve viewers individually or use a short pairing code for unattended sessions.
- **Built-in host dashboard** — inspect the stream, approve requests and disconnect viewers.
- **Local by design** — no account, STUN, TURN or cloud service is required.

## Quick start

### Requirements

| Host        | Requirements                               |
| ----------- | ------------------------------------------ |
| macOS       | macOS 13+ on Apple Silicon                 |
| Windows     | Windows 10 1903+ or Windows 11, x64        |
| Build tools | Rust 1.85+ and CMake (source builds only)  |
| Viewer      | A modern browser on the same local network |

Download the archive for your platform from [GitHub Releases](https://github.com/flaviodelgrosso/lumen/releases) — no Rust, Cargo or CMake required:

| Platform            | Asset                                      |
| ------------------- | ------------------------------------------ |
| macOS Apple Silicon | `lumen-vX.Y.Z-aarch64-apple-darwin.tar.gz` |
| Windows x64         | `lumen-vX.Y.Z-x86_64-pc-windows-msvc.zip`  |

```bash
tar -xzf lumen-vX.Y.Z-aarch64-apple-darwin.tar.gz   # Windows: extract the .zip
./lumen                                             # Windows: lumen.exe
```

Each release also ships `SHA256SUMS` covering both archives — verify with `shasum -a 256 -c SHA256SUMS` (macOS) or `sha256sum -c SHA256SUMS` (Git Bash on Windows).

To build from source instead, using the build tools above:

```bash
git clone https://github.com/flaviodelgrosso/lumen.git
cd lumen
cargo install --path lumen-cli --locked
```

Then start sharing:

```bash
lumen
```

Lumen prints a viewer URL, an IP fallback, a QR code and the local host dashboard.

On another device:

1. Scan the QR code or open `http://lumen.local:3131`.
2. Approve the device from the host terminal or dashboard.
3. The stream starts directly in the browser.

> [!IMPORTANT]
> **macOS:** the terminal running Lumen needs **Screen Recording** permission. Enable it in **System Settings → Privacy & Security → Screen Recording**, restart the terminal, then run Lumen again.
>
> On macOS 15+, the terminal may also need **Local Network** permission for LAN discovery and Chromium-based viewers.

> [!NOTE]
> Windows may show a firewall prompt on first use. Allow Lumen on the current **Private** network so local discovery and WebRTC traffic can reach other devices.

## Usage

Running `lumen` is equivalent to `lumen serve` and shares the primary display at 60 FPS with system audio enabled.

```bash
# Share the primary display
lumen

# See available displays and windows
lumen displays
lumen windows

# Share a specific display
lumen serve --display 1

# Share one window
lumen serve --window 1234

# Name a session
lumen serve --name "Architecture Workshop"

# Prefer quality over bandwidth
lumen serve --quality high

# Video only
lumen serve --no-audio

# Skip manual approval and use a pairing code
lumen serve --auto-accept
```

Common options:

| Option                 | Description                                           |
| ---------------------- | ----------------------------------------------------- |
| `--display <id>`       | Capture a specific display                            |
| `--window <id>`        | Capture a specific window                             |
| `--fps <fps>`          | Target frame rate, default `60`                       |
| `--quality <preset>`   | `low`, `medium`, `high` or `auto`                     |
| `--encoder <backend>`  | `auto`, `software` or `hardware`                      |
| `--max-bitrate <rate>` | Bitrate ceiling such as `8000k` or `2M`               |
| `--name <name>`        | Name shown to viewers and on the dashboard            |
| `--bind <ip>`          | Select the LAN interface explicitly                   |
| `--port <port>`        | Viewer/signaling port, default `3131`                 |
| `--auto-accept`        | Replace manual approval with a six-digit pairing code |
| `--no-audio`           | Disable system audio                                  |
| `--no-qr`              | Do not print the terminal QR code                     |
| `--allow-lan-admin`    | Make the host dashboard reachable from the LAN        |
| `--verbose`            | Enable debug logging and pipeline statistics          |

Run `lumen serve --help` for the complete CLI reference.

### Viewer controls

The browser viewer includes fullscreen playback, mute/unmute, fit/fill modes, horizontal mirroring, reconnect controls and optional live WebRTC statistics.

Settings are stored locally in the viewer browser.

### Host dashboard

Each session includes a lightweight host dashboard showing:

- capture source and resolution;
- target and live frame rate;
- encoder, bitrate and audio status;
- QR code and viewer URL;
- pending connection requests;
- connected devices.

The dashboard is restricted to the host machine by default.

## How it works

### Capture

Lumen captures BGRA frames directly from the operating system:

- **macOS:** ScreenCaptureKit;
- **Windows:** Windows Graphics Capture.

System audio is captured separately using ScreenCaptureKit on macOS and WASAPI loopback on Windows.

No FFmpeg process is involved.

### Encode

Video is encoded as H.264.

With `--encoder auto`, Lumen prefers the native hardware encoder:

```text
macOS     → VideoToolbox
Windows   → Media Foundation
fallback  → OpenH264
```

System audio is normalized to 48 kHz stereo and encoded as Opus.

The video pipeline always keeps the latest captured frame. If encoding falls behind, stale frames are discarded instead of being queued, preventing latency from continuously increasing.

A keyframe is also requested whenever a new viewer joins so playback can start immediately.

### Stream

Each viewer gets its own WebRTC peer connection while all viewers share the same encoded media stream.

```text
                           ┌──► Browser A
Capture ─► Encode ─► Fan-out ├──► Browser B
                           ├──► Browser C
                           └──► Browser D
```

Media travels over SRTP. Signaling and the embedded web UI are served locally by Lumen using Axum.

There are no external signaling servers, STUN servers or TURN relays.

## Platform notes

### macOS

Lumen uses ScreenCaptureKit for both screen and system-audio capture and VideoToolbox for hardware H.264 encoding.

The supported macOS target is **Apple Silicon on macOS 13 or later**.

Screen Recording permission is required. If `lumen.local` cannot be reached, also verify **Local Network** permission, VPN settings and multicast filtering on the network.

### Windows

Lumen uses Windows Graphics Capture, WASAPI loopback and Media Foundation.

A non-elevated process cannot capture windows running as administrator. Run Lumen elevated if the target application is elevated.

Displays larger than the current H.264 encoder limit of **3840×2160** cannot be downscaled by the Windows capture backend and must use a smaller display mode or window capture.

> [!NOTE]
> Windows builds are covered by CI, but real screen capture, audio devices and hardware H.264 encoders ultimately depend on the physical machine and drivers.

## Security

Opening the viewer URL alone does not grant access to a stream.

By default, every viewer must be explicitly approved by the host. After approval, Lumen issues a random, short-lived and single-use signaling grant. Join requests and pairing attempts are bounded and rate-limited.

The host dashboard uses a separate random capability and is accessible only from localhost unless `--allow-lan-admin` is explicitly enabled.

Media is transported using WebRTC/SRTP and stays on the local network.

> [!WARNING]
> Lumen intentionally serves its viewer and signaling endpoints over plain HTTP on the LAN. Do not expose the Lumen port to the public internet and use it only on networks you trust.

> [!WARNING]
> System audio means **everything your computer is playing**. Notifications, calls and other application audio may be heard by connected viewers. Use `--no-audio` when needed.

## Troubleshooting

<details>
<summary><strong><code>lumen.local</code> does not open</strong></summary>

Use the IP fallback printed by Lumen first.

If the IP works but `lumen.local` does not, multicast DNS is being blocked. Check:

- Local Network permission on macOS;
- Windows Defender Firewall;
- VPN software;
- guest Wi-Fi or client isolation;
- multicast filtering on the router.

</details>

<details>
<summary><strong>A Chromium-based viewer never connects</strong></summary>

Chromium may advertise local WebRTC candidates as mDNS names. Lumen must be able to send multicast DNS queries to resolve them.

If multicast is unavailable, Safari or another browser exposing direct LAN candidates may still work.

</details>

<details>
<summary><strong>There is no audio</strong></summary>

Lumen falls back to video-only streaming when the system audio device cannot be captured.

On Windows, changing or unplugging the default output device during a session stops the current audio capture. Restart Lumen after selecting the intended output device.

Browser autoplay policies also require the viewer to explicitly unmute audio.

</details>

## Development

Lumen is a Rust workspace split by responsibility:

| Crate          | Purpose                                                   |
| -------------- | --------------------------------------------------------- |
| `lumen-core`   | Shared configuration, media types and statistics          |
| `lumen-media`  | Native capture, H.264 encoders and Opus audio             |
| `lumen-webrtc` | Per-viewer WebRTC connections and media tracks            |
| `lumen-server` | HTTP, signaling, approvals, dashboard and embedded viewer |
| `lumen-cli`    | CLI, LAN discovery and pipeline orchestration             |

Run the complete quality gate with:

```bash
make ci
```

Or individual tasks:

```bash
make fmt-check
make check
make clippy
make test
make build
```

The project is tested in CI on both macOS and Windows.
