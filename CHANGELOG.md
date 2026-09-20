# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.3](https://github.com/flaviodelgrosso/lumen/compare/v0.1.2...v0.1.3) - 2026-09-20

### Added

- *(media)* hardware H.264 encoding via VideoToolbox with encoder selection

### Fixed

- gate AVCC helpers outside production targets

## [0.1.2](https://github.com/flaviodelgrosso/lumen/compare/v0.1.1...v0.1.2) - 2026-09-20

### Added

- *(media)* capture Windows system audio via WASAPI loopback ([#19](https://github.com/flaviodelgrosso/lumen/pull/19))

## [0.1.1](https://github.com/flaviodelgrosso/lumen/compare/v0.1.0...v0.1.1) - 2026-09-19

### Fixed

- harden the new stable-URL viewer authentication flow

### Other

- viewers can connect using a stable, device-agnostic LAN URL

## [0.1.0](https://github.com/flaviodelgrosso/lumen/releases/tag/v0.1.0) - 2026-09-18

### Added

- add admin web dashboard and improve viewer one
- first-class Windows support (WGC capture, video-only audio, Windows CI) ([#1](https://github.com/flaviodelgrosso/lumen/pull/1))

### Fixed

- repair remaining iOS video freeze after exiting native fullscreen/maximized mode
- prevent WebRTC viewer video from freezing after iOS native fullscreen transitions
- resume viewer playback after fullscreen, rotation and window zoom
- change default fps to 60
- remove scap and drive native capture backends directly
- change default fps to 60
- remove scap and drives native capture backends directly

### Other

- improve crates organization
- initial commit
