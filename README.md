# 🔴 Lumen

**Stream your screen — with system audio on macOS — to any browser on your LAN.**

No viewer app. No FFmpeg. No account. Run one command, scan a QR code, watch and listen.

Works on **macOS 13+** (ScreenCaptureKit) and **Windows 10 1903+** (Windows.Graphics.Capture, video-only — see [Windows](#windows)).

```
scap (video)    → openh264 (H.264 encode) ┐
                                           ├→ webrtc-rs (SRTP) → browser <video>
SCK audio (PCM) → libopus  (Opus encode)  ┘
```

---

## Table of Contents

- [Requirements](#requirements)
- [Install](#install)
- [Usage](#usage)
- [How It Works](#how-it-works)
- [Security](#security)
- [Troubleshooting](#troubleshooting)
- [Development](#development)
- [License](#license)

---

## Requirements

| Requirement                     | Notes                                                                                                                                         |
| ------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| **macOS 13+**                   | Apple Silicon or Intel. Capture uses the native ScreenCaptureKit API via `scap`; system audio uses a dedicated ScreenCaptureKit audio stream. |
| **Windows 10 1903+ / 11**       | x64. Display and window capture use Windows.Graphics.Capture (WGC) via `scap`. No system audio — `lumen` streams video only. See [Windows](#windows). |
| **Rust 1.85+**                  | Edition 2024, needed to build.                                                                                                                |
| **CMake**                       | The `opus` crate compiles a bundled `libopus` at build time. Install with `brew install cmake` if missing (macOS).                            |
| **Screen Recording permission** | macOS only — see [below](#macos-screen-recording-permission). Windows needs no permission grant; see [Windows](#windows).                     |

### macOS Screen Recording permission

`lumen` captures the screen, so macOS requires the terminal app you run it from (Terminal, iTerm, Ghostty, …) to be allowed under:

> **System Settings → Privacy & Security → Screen Recording**

The first `lumen serve` / `lumen displays` run registers the request — toggle your terminal on, then **restart it** and run again. Without the permission, `lumen` exits with a clear message instead of crashing.

### Windows

Windows needs **no per-app screen-recording permission**: Windows.Graphics.Capture grants capture to any ordinary process. The realistic blockers are different:

- **Elevated windows** — a non-elevated `lumen` cannot capture windows running as administrator (Task Manager, elevated terminals, …). Close the elevated target or run `lumen` from an elevated terminal.
- **Group policy / Windows Defender Firewall** — enterprise policy can disable screen capture entirely; the firewall gates the LAN/multicast traffic like any server app (allow `lumen` on the current network profile).
- **No system audio** — WGC captures video only. `lumen serve` detects the missing backend, logs `system audio is not available here; streaming video only`, prints `Audio: unavailable — video only` in the banner, and starts normally. `--no-audio` is never required on Windows.

#### Runtime verification status (manual checklist for a real Windows host)

The Windows build is verified in CI (`cargo check`/`test`/`clippy`/`build` on a `windows-latest` runner). CI cannot exercise screen capture, so these behaviors are wired to the `scap`/`windows-capture` (WGC) backend but **not yet validated on hardware** — confirm each on a real Windows 10/11 machine before trusting them:

1. `lumen displays` lists monitors and `lumen windows` lists app windows; ids round-trip into `--display` / `--window` (scap derives target ids from `HMONITOR`/`HWND` truncated to `u32` — verify the printed id selects the intended target).
2. `lumen serve` captures the primary display and a non-primary display at **100%, 125% and 150% display scaling** (scap sizes WGC output from `DEVMODE`/`GetWindowRect` × effective DPI; the frame stride may also exceed the logical width — Lumen keeps the frame's logical size and repacks padded rows, but the crop math is scap's).
3. Window capture of a normal (non-elevated) window, incl. a window on a secondary monitor with different DPI.
4. A capture start failure (e.g. the window vanishes mid-start) surfaces as `capture failed to start: …` — scap's Windows engine still unwraps internally, so a panic there would be an upstream bug to report, not a Lumen error path.
5. The banner shows `Audio: unavailable — video only`, and viewers play video with no audio track.
6. Chromium viewers connect over mDNS: on first run Windows asks to allow `lumen` through the firewall; accept it on the **Private** network profile, then verify a Chrome/Edge viewer on another device connects (a VPN/virtual adapter without multicast routing fails the same way as on macOS — see [Troubleshooting](#troubleshooting)).
7. Ctrl+C shuts the server down cleanly (no orphaned `lumen.exe`).

If any item fails on hardware, the fix belongs either in the platform-gated code in `lumen-capture` or upstream in `scap-vc`; the macOS implementation is unaffected either way.

---

## Install

```sh
cargo install --path lumen-cli --locked
```

Or build in-tree:

```sh
cargo build --release   # → target/release/lumen
```

---

## Usage

```sh
lumen serve            # capture primary display, stream on the LAN
lumen --help
```

`serve` prints a viewer URL and a QR code. Open the URL (or scan the QR) on any device on the same network. The first viewer must be approved in the terminal unless `--auto-accept` is set.

### Flags

| Flag                   | Default | Meaning                                                              |
| ---------------------- | ------- | -------------------------------------------------------------------- |
| `--display <id>`       | primary | Display to capture (`lumen displays` for ids)                        |
| `--window <id>`        | —       | Capture a single window instead                                      |
| `--bind <ip>`          | auto    | LAN address to bind (interactive menu when several interfaces exist) |
| `--port <port>`        | `3131`  | HTTP/signaling port                                                  |
| `--fps <fps>`          | `30`    | Capture/encode frame rate                                            |
| `--quality <preset>`   | `auto`  | `low` \| `medium` \| `high` \| `auto`                                |
| `--max-bitrate <rate>` | preset  | Ceiling, e.g. `8000k` or `2M`                                        |
| `--auto-accept`        | off     | Admit viewers without prompting                                      |
| `--no-audio`           | off     | Stream video only (no system audio)                                  |
| `--no-qr`              | off     | Skip the QR code                                                     |
| `--verbose`            | off     | Debug logs + periodic `[stats]` line                                 |

`lumen displays` and `lumen windows` list capturable targets.

---

## How It Works

### 🎥 Capture

`scap` grabs BGRA frames at the target FPS — from ScreenCaptureKit on macOS and from Windows.Graphics.Capture on Windows. The pipeline keeps only the latest frame — a slow encoder never builds a queue. Displays larger than the encoder's 3840×2160 ceiling (e.g. a 3456×2234 Retina panel) are scaled down by ScreenCaptureKit itself — zero CPU cost — to the nearest encodable size, aspect ratio preserved. On Windows, frames whose D3D11 row pitch exceeds the logical width keep their logical size; the encoder repacks padded rows.

### 🎞️ Encode

`openh264` (Cisco's royalty-free binary codec, loaded at runtime) encodes H.264 constrained-baseline-ish Annex B, with a forced IDR every 2 seconds and on every new viewer join — so mid-GOP joiners start instantly.

### 🔊 Audio

A dedicated audio-only ScreenCaptureKit stream captures system audio on macOS and normalizes it to 48 kHz stereo `f32` PCM for Opus. `libopus` (bundled via the `opus` crate) encodes 20 ms packets at 128 kbps, riding a second WebRTC track in the same `MediaStream`. Browsers block autoplaying sound, so the viewer starts **muted** — use the Unmute button in the HUD to enable sound. On Windows there is no system-audio backend; `lumen` falls back to a video-only `MediaStream` automatically.

### 📡 Stream

One `webrtc-rs` peer connection per viewer (negotiated `recvonly` answer from the browser's offer, SRTP, LAN-only ICE — no STUN, no TURN, no internet). A single encoder feeds all viewers through a bounded `tokio::sync::broadcast` per track; lagging viewers drop video frames and resync at the next keyframe, while audio resumes at the next packet (the Opus decoder conceals the gap).

### 🌐 Serve

`axum` serves the embedded viewer (`web/`) and a WebSocket signaling channel (`waiting → approval → offer → answer → ice`).

---

## Security

- 🔑 Every `serve` run generates a fresh **256-bit session token**. The viewer URL carries it, and every request (HTTP + WebSocket) must present it. Unknown tokens get `404` — fail closed, no enumeration.
- ✅ New viewers are **approved interactively** in the terminal (browser name + device detected from the user agent), unless `--auto-accept` is set.
- 👢 The host can list (`GET /api/peers?token=…`) and **kick** (`POST /api/peers/<id>/disconnect?token=…`) viewers at any time.
- 🌐 Traffic is **LAN-only**. The HTTP server is plain `http://` (so no TLS certificate warnings), but the media itself — video and audio — is SRTP-encrypted end to end. Do not expose the port beyond your LAN.
- 🔈 **Audio is everything your computer plays** (macOS) — approved viewers hear system audio (notifications, calls, music). Use `--no-audio` for video-only; Windows is video-only already.
- 🧭 ICE candidates are restricted to LAN; mDNS `.local` candidates are resolved in query mode. A loopback socket is bound only for a viewer that is itself on loopback, so a LAN viewer is never pinged from `127.0.0.1`.

---

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
A capture larger than 3840×2160 that scap cannot scale (see [How It Works](#how-it-works)). Pick a smaller `--display` target or window.

---

## Development

```
lumen-core      shared types, config, errors
lumen-capture   scap video (SCK / WGC) + SCK system-audio + fake sources (tests)
lumen-encoder   openh264 + opus wrappers + fake encoders (tests)
lumen-webrtc    per-viewer peer connection (video + audio tracks)
lumen-session   token, approval, user-agent parsing
lumen-network   LAN interface discovery
lumen-server    axum HTTP + WebSocket signaling + fan-out
lumen-cli       `lumen` binary (serve/displays/windows)
```

**Gates**, via the `Makefile`:

```sh
make fmt-check   # formatting
make check       # compile check
make clippy      # warnings as errors
make test        # test suite
make ci          # all of the above
make install     # install the release binary
```

**Windows.** The full gate runs on a `windows-latest` CI runner (check/test/clippy/release build). Off-Windows you can still type-check the pure-Rust half of the workspace for the MSVC target:

```sh
rustup target add x86_64-pc-windows-msvc
cargo check -p lumen-core -p lumen-capture -p lumen-session -p lumen-network \
  --all-targets --locked --target x86_64-pc-windows-msvc
```

The crates behind C toolchains (`lumen-encoder`'s `opus`, `lumen-webrtc`'s `ring`) can't cross-compile from macOS and are validated on the runner only. `windows-capture` (scap's WGC backend) is pinned to `1.4.4` in `Cargo.lock` — its `Settings::new` signature changed incompatibly in 1.5; keep the pin on `cargo update` (see the comment in `Cargo.toml`).

> **Note:** `cargo clippy` may report a future-incompatibility warning for `block v0.1.6` (a transitive dependency of `scap`'s `objc` usage). It's upstream, harmless today, and not actionable in this repo.
>
> `openh264` loads a prebuilt Cisco library at runtime; the `openh264` crate vendors it under a BSD-2-style license (see below).

---

## License

`lumen` is **MIT** (see `LICENSE`). Bundled/third-party components:

| Component                                | License                                 |
| ---------------------------------------- | --------------------------------------- |
| `openh264` (Cisco)                       | BSD-2-Clause-style, with a patent grant |
| `libopus` (bundled via the `opus` crate) | BSD-3-Clause                            |
| `webrtc-rs`                              | MIT                                     |
| Everything else via crates.io            | MIT/Apache-2.0 compatible               |
