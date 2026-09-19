<!-- prettier-ignore -->
<div align="center">

# 📽️ Lumen

**Stream your screen — with system audio on macOS — to any browser on your LAN.**

[![CI](https://img.shields.io/github/actions/workflow/status/flaviodelgrosso/lumen/ci.yml?style=flat-square&label=CI)](https://github.com/flaviodelgrosso/lumen/actions/workflows/ci.yml)
![Rust](https://img.shields.io/badge/rust-1.85%2B-dea584?style=flat-square&logo=rust&logoColor=black)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows-666?style=flat-square)
[![License: MIT](https://img.shields.io/badge/license-MIT-yellow?style=flat-square)](LICENSE)

⭐ If you like this project, star it on GitHub!

[Features](#features) • [Quick start](#quick-start) • [Usage](#usage) • [How it works](#how-it-works) • [Security](#security) • [Troubleshooting](#troubleshooting) • [Development](#development)

</div>

No viewer app. No FFmpeg. No account. Run one command, scan a QR code, watch and listen — approvals, device management and stream stats live in a built-in host dashboard.

```
capture (SCK / WGC)  →  openh264 (H.264) ┐
                                         ├→ WebRTC over SRTP → browser <video>
system audio (SCK)   →  libopus  (Opus)  ┘
```

Works on **macOS 13+** (ScreenCaptureKit, video + system audio) and **Windows 10 1903+ / 11** (Windows.Graphics.Capture, video only — see [Windows](#windows)).

## Features

- **Zero-setup viewer** — any modern browser on the LAN; nothing to install on the viewing device
- **Stable entry URL** — `http://lumen.local:3131`, advertised via mDNS/Bonjour; no secrets in the URL, LAN IP fallback printed alongside
- **Native capture** — ScreenCaptureKit on macOS, Windows.Graphics.Capture on Windows, no FFmpeg
- **System audio on macOS** — dedicated ScreenCaptureKit audio stream, Opus 48 kHz stereo
- **Instant mid-session joins** — forced keyframe every 2 s and on every new viewer
- **Host dashboard** — a lightweight `/admin` page (no frameworks): QR code, copy-link, live FPS/quality/audio, pending devices with Allow/Deny, connected devices with Disconnect
- **Two approval surfaces** — confirm each viewer in the terminal (browser + device detected) or from the dashboard; kick anyone at any time from either
- **LAN-only by design** — no STUN, no TURN, no internet; media is SRTP-encrypted end to end
- **Single binary** — capture, encode, fan-out, HTTP, signaling and viewer in one `lumen` command

## Quick start

### Requirements

| Requirement                     | Notes                                                                                               |
| ------------------------------- | --------------------------------------------------------------------------------------------------- |
| **macOS 13+**                   | Apple Silicon or Intel. Capture via ScreenCaptureKit; system audio via a dedicated audio stream.    |
| **Windows 10 1903+ / 11**       | x64. Display and window capture via Windows.Graphics.Capture. Video only — see [Windows](#windows). |
| **Rust 1.85+**                  | Edition 2024. Needed to build.                                                                      |
| **CMake**                       | The `opus` crate compiles a bundled `libopus` at build time (`brew install cmake`).                 |
| **Screen Recording permission** | macOS only — see below.                                                                             |

### Install

```sh
make install
```

Or build in-tree: `cargo build --release` (→ `target/release/lumen`).

### Run

```sh
lumen
```

`lumen` prints the viewer entry URL, a QR code and a **host dashboard** URL. Open `http://lumen.local:3131` (or scan the QR) on any device on the same network, then approve the device in the terminal or on the dashboard unless `--auto-accept` is set. `lumen.local` is advertised via mDNS and resolves on any OS with local discovery (macOS, iOS, Android, Windows, most Smart-TV browsers); the banner also prints the **IP fallback** (`http://<lan-ip>:3131`) for networks where `.local` does not resolve — same page, same flow. If mDNS registration fails, `lumen` warns and keeps serving; nothing else changes. The dashboard (`http://127.0.0.1:3131/admin/<admin-token>`) is reachable **from the host machine only** unless you pass `--allow-lan-admin`.

> [!IMPORTANT]
> **macOS:** the terminal app you run `lumen` from (Terminal, iTerm, Ghostty, …) must be allowed under **System Settings → Privacy & Security → Screen Recording**. The first run registers the request — toggle your terminal on, then **restart it** and run again. Without the permission, `lumen` exits with a clear message instead of crashing.

### Windows

Windows needs **no per-app screen-recording permission** — Windows.Graphics.Capture grants capture to any ordinary process. The realistic blockers are different:

- **Elevated windows** — a non-elevated `lumen` cannot capture windows running as administrator. Close the elevated target or run `lumen` from an elevated terminal.
- **Firewall / group policy** — allow `lumen` through Windows Defender Firewall on the current (Private) network profile; enterprise policy can disable screen capture entirely.
- **No system audio** — WGC captures video only. `lumen serve` detects the missing backend, logs `system audio is not available here; streaming video only`, and starts normally. `--no-audio` is never required on Windows.

> [!NOTE]
> The Windows build is fully gated in CI (`check`/`test`/`clippy`/`build` on `windows-latest`), but CI cannot exercise screen capture. The WGC path drives `windows-capture` directly but **not yet validated on hardware** — run the checklist below on a real machine before trusting it.

<details>
<summary><b>Windows hardware verification checklist</b></summary>

1. `lumen displays` lists monitors and `lumen windows` lists app windows; printed ids round-trip into `--display` / `--window` (display ids are the position in the `lumen displays` listing; window ids are the `HWND` truncated to `u32`).
2. `lumen serve` captures primary and non-primary displays at 100%, 125% and 150% scaling (WGC sizes frames by effective DPI; padded rows are repacked by the encoder).
3. On a display larger than **3840×2160** (5K/6K/8K), `lumen serve` exits at startup with `display <W>x<H> exceeds the 3840x2160 encoder limit…`. The WGC backend cannot scale — this is expected, not a bug.
4. Window capture of a normal (non-elevated) window, including a window on a secondary monitor with different DPI.
5. The banner shows `Audio: unavailable — video only`, and viewers play video with no audio track.
6. A Chromium viewer on another device connects over mDNS (accept the firewall prompt on the **Private** profile first).
7. Ctrl+C shuts the server down cleanly.

If an item fails, the fix belongs in the platform-gated capture code in `lumen-media` or upstream in `windows-capture`; the macOS path is unaffected either way.

</details>

## Usage

```
lumen serve [flags]    # capture + stream (default command)
lumen displays         # list capturable displays
lumen windows          # list capturable windows
```

| Flag                   | Default | Meaning                                                              |
| ---------------------- | ------- | -------------------------------------------------------------------- |
| `--display <id>`       | primary | Display to capture (`lumen displays` for ids)                        |
| `--window <id>`        | —       | Capture a single window instead                                      |
| `--bind <ip>`          | auto    | LAN address to bind (interactive menu when several interfaces exist) |
| `--port <port>`        | `3131`  | HTTP/signaling port                                                  |
| `--fps <fps>`          | `60`    | Capture/encode frame rate                                            |
| `--quality <preset>`   | `auto`  | `low` \| `medium` \| `high` \| `auto`                                |
| `--max-bitrate <rate>` | preset  | Ceiling, e.g. `8000k` or `2M`                                        |
| `--auto-accept`        | off     | Admit viewers without prompting                                      |
| `--allow-lan-admin`    | off     | Let non-localhost clients reach the host dashboard                   |
| `--no-audio`           | off     | Stream video only (no system audio)                                  |
| `--no-qr`              | off     | Skip the QR code                                                     |
| `--verbose`            | off     | Debug logs + periodic `[stats]` line                                 |

> [!TIP]
> Unless `--max-bitrate` is set, the target bitrate is derived from resolution × fps × the quality preset and clamped to 0.8–20 Mbps. `auto` currently follows `medium`.

```sh
lumen serve --window 42 --fps 60 --quality high   # one window, high motion
lumen serve --no-audio --auto-accept              # quick, unattended video-only stream
```

## How it works

### Capture

Native backends grab BGRA frames — `ScreenCaptureKit` via the `screencapturekit` crate on macOS, `Windows.Graphics.Capture` via the `windows-capture` crate on Windows (engine on a dedicated thread, frames over a bounded channel). The pipeline keeps only the latest frame, so a slow encoder never builds a queue. On macOS, targets outside the encoder's bounds are floored to even and scaled to the nearest encodable size — aspect ratio preserved — by ScreenCaptureKit itself, zero CPU cost. WGC cannot scale: oversized displays fail at startup, and frames whose D3D11 row pitch exceeds the logical width keep their logical size; the encoder repacks the padded rows.

### Encode

`openh264` (Cisco's royalty-free binary codec, loaded at runtime) encodes H.264 constrained-baseline-ish Annex B, with a forced IDR every 2 seconds and on every new viewer join — mid-GOP joiners start instantly.

### Audio

A dedicated audio-only ScreenCaptureKit stream captures system audio on macOS and normalizes it to 48 kHz stereo `f32` PCM for Opus. `libopus` (bundled via the `opus` crate) encodes 20 ms packets at 128 kbps, riding a second WebRTC track in the same `MediaStream`. Browsers block autoplaying sound, so the viewer starts **muted** — use the Unmute button in the HUD to enable sound. On Windows there is no system-audio backend; `lumen` falls back to a video-only `MediaStream` automatically.

### Stream

One `webrtc-rs` peer connection per viewer (negotiated `recvonly` answer from the browser's offer, SRTP, LAN-only ICE). A single encoder feeds all viewers through a bounded `tokio::sync::broadcast` per track: lagging viewers drop video frames and resync at the next keyframe, while audio resumes at the next packet — the Opus decoder conceals the gap.

### Serve

`axum` serves the embedded viewer at `/`, the host dashboard and a WebSocket signaling channel per viewer: `join → approval → grant → offer → answer → ice`. Opening the page only creates a **pending join request** (`POST /api/join`); once the host approves, the poll endpoint hands the viewer an **ephemeral single-use grant** (OS-random, short TTL, constant-time checked) that authenticates exactly one signaling socket — no long-lived secret ever appears in a URL. The viewer adds auto-reconnect (a fresh join re-asks the host), double-tap fullscreen, mute, fit/fill, mirror, screen wake lock, optional WebRTC stats (`RTCPeerConnection.getStats()`) and a retry action on terminal states — all plain HTML/CSS/JS, no frameworks, no vendor detection. The dashboard polls a single admin API (`/api/admin/state`) and drives approval/disconnect through `/api/admin/...`. Join requests are bounded (saturated queue answers `429`), so a cheap repeat-POST cannot pile up state.

## Security

- The viewer page at `/` is **LAN-discoverable by design** — reaching it grants nothing. Stream access requires the host's **approval** and an **ephemeral session grant**: after approval the server issues a fresh **256-bit** (URL-safe base64, OS CSPRNG) grant with a short TTL, redeemable exactly once by the signaling socket and checked in constant time; expired, reused or forged grants answer `404`. No secret ever lives in the viewer URL. The **admin token** is a separate 256-bit secret carried by the dashboard URL: admin routes answer `404` to missing or wrong admin tokens (fail closed, no enumeration) and `403` when the client is not on localhost. A viewer grant never reaches an admin endpoint, and the admin token never authenticates signaling.
- The admin surface (`/admin/<admin-token>`, `/api/admin/*`) is **localhost-only by default**; `--allow-lan-admin` extends it to the LAN. Admin clients can see the pending queue, **Allow/Deny** viewers (`POST /api/admin/pending/<id>/allow|deny`), and **kick** connected ones (`POST /api/admin/peers/<id>/disconnect`).
- New viewers are approved on the **terminal prompt or the dashboard** (browser name and device are parsed from the user agent) — first responder wins — unless `--auto-accept` is set. Approval is fail-closed: a saturated pending queue denies immediately, and non-interactive runs without a dashboard decision never leak a peer through.
- Traffic is **LAN-only**: ICE is restricted to LAN addresses, mDNS `.local` candidates are resolved in query mode, and a loopback socket is bound only for a viewer that is itself on loopback. The HTTP server is plain `http://` (no TLS certificate warnings), but the media itself — video and audio — is SRTP-encrypted end to end. Do not expose the port beyond your LAN.

> [!WARNING]
> **Audio is everything your computer plays** (macOS) — approved viewers hear notifications, calls and music. Use `--no-audio` for video-only; Windows is video-only already.

## Troubleshooting

### Viewers on other devices never connect

Logs show:

```
Failed to write packet to 224.0.0.251:5353 … No route to host
mDNS Query … timed out
```

**Cause:** the host cannot send IPv4 multicast. Chromium browsers obfuscate their host ICE candidates as `mDNS .local` names, and `lumen` resolves them by querying `224.0.0.251:5353`. If every such send fails, the browser's candidate is dropped, no ICE pair ever forms, and LAN viewers never connect. (`lumen serve` probes this at startup and warns.)

Likely causes, in order of likelihood (labels note the OS):

<details>
<summary><b>1. The terminal app lacks the Local Network permission (macOS 15+)</b></summary>

macOS gates multicast (and direct LAN connections) behind a privacy permission owned by the app responsible for the process — for a CLI binary, that's your **terminal app** (Terminal, Ghostty, iTerm, …), not `lumen` itself. Binaries run from a terminal frequently never trigger the prompt, so grant it manually:

> **System Settings → Privacy & Security → Local Network → enable your terminal**

Then quit and reopen the terminal and run `lumen` again.

</details>

<details>
<summary><b>1b. Windows Defender Firewall blocks the app (Windows)</b></summary>

Windows shows a prompt the first time `lumen` sends UDP; dismissing it leaves outbound multicast blocked. Allow the binary on the current (Private) network profile:

```powershell
New-NetFirewallRule -DisplayName "lumen screen sharing" `
  -Direction Outbound -Program "$PWD\target\release\lumen.exe" `
  -Action Allow -Profile Private
```

Repeat after each rebuild — the rule keys on the file path.

</details>

<details>
<summary><b>2. The host's NIC cannot multicast</b></summary>

Check without `lumen`:

```sh
python3 -c "
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.bind(('0.0.0.0', 0))
try:
    s.sendto(b'x'*12, ('224.0.0.251', 5353)); print('multicast ok')
except OSError as e:
    print('multicast FAILED:', e)
"
```

Run this **after** granting the permission — from a terminal it inherits the same gate, so it fails identically while the permission is denied. A genuine `FAILED` after granting means the multicast route is broken (common on VMs with synthetic NICs and some VPN setups). `lumen` cannot fix that — instead:

- Make the viewer offer plain IP candidates: Chrome flag `--disable-features=WebRtcHideLocalIpsWithMdns`, or
- View from a browser that doesn't obfuscate by default (Safari).

</details>

<details>
<summary><b>3. The macOS Application Firewall drops UDP</b></summary>

A firewall block looks similar in the logs, but the check above distinguishes it: the firewall drops packets _silently_ (`sendto` succeeds, the answer never arrives), whereas a missing permission or a broken route fails the `sendto` itself.

If `sendto` succeeds but queries still time out, allow the binary:

```sh
sudo /usr/libexec/ApplicationFirewall/socketfilterfw \
  --add "$(pwd)/target/release/lumen"
sudo /usr/libexec/ApplicationFirewall/socketfilterfw \
  --unblockapp "$(pwd)/target/release/lumen"
```

> Repeat after each rebuild — the firewall keys on the file path. Released/notarized builds prompt instead.

A browser on the **same Mac** connects over loopback regardless, which is why local tests can pass while LAN devices fail.

</details>

### Other errors

**`Failed to write packet to <lan-ip>:<port> from 127.0.0.1:<port>: Can't assign requested address (os error 49)`**
A loopback-bound ICE socket is being used to reach a LAN peer. `lumen` binds `127.0.0.1:0` only for a viewer that is itself on loopback, so this shouldn't appear — if it does, the viewer is likely being mis-detected (e.g. behind a proxy that makes the connection look local).

**`display size … is not encodable`**
A capture larger than 3840×2160 that Windows Graphics Capture cannot scale (see [Capture](#capture)). Pick a smaller `--display` target or a window.

## Development

```
lumen-core      shared types, config, errors
lumen-media     native video (SCK / WGC) + SCK system-audio + openh264/opus wrappers + fake sources/encoders (tests)
lumen-webrtc    per-viewer peer connection (video + audio tracks)
lumen-server    axum HTTP + WebSocket signaling + fan-out + token/approval/user-agent parsing + embedded viewer
lumen-cli       `lumen` binary (serve/displays/windows), LAN interface discovery
```

**Gates**, via the `Makefile`:

```sh
make fmt-check   # formatting
make check       # compile check
make clippy      # warnings as errors
make test        # test suite
make build       # release build
make ci          # all of the above (minus build)
make install     # install the release binary
```

**Windows.** The full gate runs on a `windows-latest` CI runner (check/test/clippy/release build). Off-Windows you can still type-check the pure-Rust half of the workspace for the MSVC target:

```sh
rustup target add x86_64-pc-windows-msvc
cargo check -p lumen-core --all-targets --locked --target x86_64-pc-windows-msvc
```

The rest of the workspace sits behind C toolchains (`lumen-media`'s bundled `opus`/`openh264`, `lumen-webrtc`'s `ring`) and can't cross-compile from macOS; it is validated on the `windows-latest` runner only.

> [!NOTE]
>
> - `screencapturekit` ships a Swift bridge; its `libswift_Concurrency.dylib` dependency is resolved through the `/usr/lib/swift` rpath that `.cargo/config.toml` adds for macOS targets (the crate's own build script only bakes it into its own targets, not downstream binaries).
> - `openh264` loads a prebuilt Cisco library at runtime; the `openh264` crate vendors it under a BSD-2-style license.
