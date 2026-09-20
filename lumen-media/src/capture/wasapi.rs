//! Windows system-audio capture through WASAPI loopback.
//!
//! System audio is captured by opening the default *render* endpoint in
//! shared mode with `AUDCLNT_STREAMFLAGS_LOOPBACK` and draining device
//! packets through `IAudioCaptureClient`. Packets arrive in the device's
//! mix format — commonly 32-bit float, but at the device's own rate and
//! channel count (44.1 kHz is extremely common) — so every packet is
//! decoded to `f32`, downmixed to stereo, and resampled to 48 kHz by the
//! same [`LaneResampler`] the macOS path uses. The pipeline contract
//! ([`RawAudioFrame`] at 48 kHz stereo) never depends on the device.
//!
//! WASAPI is a COM API, so the whole lifecycle lives on one engine
//! thread: it initializes COM, activates the audio client, runs the
//! capture loop, and releases everything when it exits. The public
//! [`PlatformAudioCapture`] only polls a bounded channel, keeping
//! [`AudioCaptureSource::next_audio`] blocking and shutdown responsive.
//! Device invalidation and capture failures surface as
//! [`CaptureError::AudioDeviceLost`] / [`CaptureError::Stopped`] — never
//! as a panic.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use lumen_core::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, RawAudioFrame};
use windows::Win32::Media::Audio::{
  AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED,
  AUDCLNT_STREAMFLAGS_LOOPBACK, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
  MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
  CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::core::{Error as WindowsError, IUnknown};

use super::{
  AudioCaptureSource, POLL_INTERVAL, QUEUE_BUFFERS, deinterleave, interleave, mix_to_stereo,
};
use crate::capture::resample::LaneResampler;
use crate::capture::{CaptureError, FrameSender, SendOutcome};

/// How long the engine waits for the next audio packet before polling
/// again; short enough to keep shutdown responsive, long enough to idle
/// the thread between 10 ms device periods.
const PACKET_POLL: Duration = Duration::from_millis(10);

/// Requested loopback buffer duration in 100 ns units (10 ms). In shared
/// mode the audio engine sizes the buffer itself, but WASAPI requires a
/// positive value here.
const LOOPBACK_BUFFER_DURATION: i64 = 100_000;

/// `AUDCLNT_BUFFERFLAGS_SILENT`: the packet contents are undefined and
/// must be replaced by silence.
const SILENT_PACKET: u32 = AUDCLNT_BUFFERFLAGS_SILENT.0 as u32;

/// Plausible upper bound for a hardware mix format; rejects garbage
/// descriptors before they are used to size buffers.
const MAX_CHANNELS: usize = 32;

/// The PCM sample representation of a WASAPI mix format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SampleKind {
  /// IEEE 754 little-endian 32-bit float — what the audio engine mixes in.
  Float32,
  /// Little-endian signed 16-bit integer PCM.
  Int16,
  /// Little-endian signed 32-bit integer PCM (also covers 24-bit data
  /// packed into 32-bit containers).
  Int32,
}

impl SampleKind {
  fn bytes(self) -> usize {
    match self {
      Self::Float32 | Self::Int32 => 4,
      Self::Int16 => 2,
    }
  }
}

/// The parts of the device mix format that normalization depends on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceFormat {
  rate: u32,
  channels: usize,
  kind: SampleKind,
}

impl SourceFormat {
  /// Interpret a `WAVEFORMATEX` header from `GetMixFormat`, accepting
  /// plain PCM / IEEE-float headers and `WAVEFORMATEXTENSIBLE` with a
  /// float or integer-PCM sub-format; anything else yields `None`.
  ///
  /// # Safety
  ///
  /// `wf` must be a valid `WAVEFORMATEX` — or a valid
  /// `WAVEFORMATEXTENSIBLE` when the tag says so, as guaranteed for the
  /// pointer returned by `IAudioClient::GetMixFormat`.
  unsafe fn from_wave_format(wf: *const WAVEFORMATEX) -> Option<Self> {
    let header = unsafe { &*wf };
    let channels = usize::from(header.nChannels);
    if !(1..=MAX_CHANNELS).contains(&channels) {
      return None;
    }
    let bits = usize::from(header.wBitsPerSample);
    let kind = match u32::from(header.wFormatTag) {
      WAVE_FORMAT_EXTENSIBLE => {
        let ext = unsafe { &*(wf.cast::<WAVEFORMATEXTENSIBLE>()) };
        // `WAVEFORMATEXTENSIBLE` is packed; copy the GUID out to compare.
        let sub_format = ext.SubFormat;
        if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
          SampleKind::Float32
        } else if sub_format == KSDATAFORMAT_SUBTYPE_PCM {
          int_kind(bits)?
        } else {
          return None;
        }
      }
      WAVE_FORMAT_IEEE_FLOAT => SampleKind::Float32,
      WAVE_FORMAT_PCM => int_kind(bits)?,
      _ => return None,
    };
    let rate = header.nSamplesPerSec;
    // `LaneResampler` is defined for 4k..=384k; hardware never exceeds it.
    if !(4_000..=384_000).contains(&rate) {
      return None;
    }
    // The header must describe whole, consistently sized frames.
    if bits != kind.bytes() * 8 || usize::from(header.nBlockAlign) != channels * (bits / 8) {
      return None;
    }
    Some(Self {
      rate,
      channels,
      kind,
    })
  }

  fn frame_bytes(self) -> usize {
    self.channels * self.kind.bytes()
  }
}

fn int_kind(bits: usize) -> Option<SampleKind> {
  match bits {
    16 => Some(SampleKind::Int16),
    32 => Some(SampleKind::Int32),
    _ => None,
  }
}

/// Normalizes raw WASAPI packets to the pipeline contract: decode to
/// `f32`, downmix to stereo, resample to [`AUDIO_SAMPLE_RATE`]. Pure
/// arithmetic — no COM — so it is fully tested without hardware.
struct PcmNormalizer {
  format: SourceFormat,
  /// Present exactly when the device rate differs from the output rate;
  /// one resampler per output lane keeps the filter state across packets.
  lanes: Option<(LaneResampler, LaneResampler)>,
}

impl PcmNormalizer {
  fn new(format: SourceFormat) -> Self {
    let lanes = (format.rate != AUDIO_SAMPLE_RATE).then(|| {
      (
        LaneResampler::new(format.rate, AUDIO_SAMPLE_RATE),
        LaneResampler::new(format.rate, AUDIO_SAMPLE_RATE),
      )
    });
    Self { format, lanes }
  }

  /// Convert one raw `GetBuffer` payload into a pipeline frame. Returns
  /// `None` for empty or malformed payloads, which the engine drops; the
  /// device itself keeps running.
  fn packet_to_frame(
    &mut self,
    data: &[u8],
    flags: u32,
    captured_at: Instant,
  ) -> Option<RawAudioFrame> {
    let frame_bytes = self.format.frame_bytes();
    if frame_bytes == 0 || data.len() % frame_bytes != 0 {
      return None;
    }
    let frames = data.len() / frame_bytes;
    if frames == 0 {
      return None;
    }
    let samples = if flags & SILENT_PACKET != 0 {
      // Silent packets carry undefined buffer contents; synthesize zeros
      // (through the resampler, so the filter state stays coherent).
      let zeros = vec![0.0_f32; frames];
      self.stereo_bytes(&zeros, &zeros)
    } else if self.format.kind == SampleKind::Float32
      && self.format.channels == usize::from(AUDIO_CHANNELS)
      && self.format.rate == AUDIO_SAMPLE_RATE
    {
      // Already exactly what the pipeline wants: interleaved f32 stereo.
      data.to_vec()
    } else {
      let planes = self.decode(data)?;
      let (left, right) = mix_to_stereo(&planes);
      self.stereo_bytes(&left, &right)
    };
    if samples.is_empty() {
      return None;
    }
    Some(RawAudioFrame {
      samples: Bytes::from(samples),
      sample_rate: AUDIO_SAMPLE_RATE,
      channels: AUDIO_CHANNELS,
      captured_at,
    })
  }

  /// Decode one payload into one `f32` plane per source channel.
  #[expect(
    clippy::cast_possible_truncation,
    reason = "integer samples divide by an exact power of two; the f32 \
              result is the closest approximation of the exact value"
  )]
  fn decode(&self, data: &[u8]) -> Option<Vec<Vec<f32>>> {
    let channels = self.format.channels;
    match self.format.kind {
      SampleKind::Float32 => deinterleave(data, channels),
      SampleKind::Int16 => {
        let stride = channels.checked_mul(2)?;
        if data.len() % stride != 0 {
          return None;
        }
        let mut planes = vec![Vec::with_capacity(data.len() / stride); channels];
        for chunk in data.chunks_exact(stride) {
          for (plane, sample) in planes.iter_mut().zip(chunk.chunks_exact(2)) {
            let value = i16::from_le_bytes(sample.try_into().expect("2 bytes"));
            plane.push((f64::from(value) / 32_768.0) as f32);
          }
        }
        Some(planes)
      }
      SampleKind::Int32 => {
        let stride = channels.checked_mul(4)?;
        if data.len() % stride != 0 {
          return None;
        }
        let mut planes = vec![Vec::with_capacity(data.len() / stride); channels];
        for chunk in data.chunks_exact(stride) {
          for (plane, sample) in planes.iter_mut().zip(chunk.chunks_exact(4)) {
            let value = i32::from_le_bytes(sample.try_into().expect("4 bytes"));
            plane.push((f64::from(value) / 2_147_483_648.0) as f32);
          }
        }
        Some(planes)
      }
    }
  }

  /// Downmixed stereo lanes to interleaved LE `f32` bytes, resampling to
  /// the output rate when the device differs from it.
  fn stereo_bytes(&mut self, left: &[f32], right: &[f32]) -> Vec<u8> {
    match &mut self.lanes {
      Some((lane_l, lane_r)) => {
        let mut out_l = Vec::with_capacity(left.len());
        let mut out_r = Vec::with_capacity(right.len());
        lane_l.push(left, &mut out_l);
        lane_r.push(right, &mut out_r);
        interleave(&out_l, &out_r)
      }
      None => interleave(left, right),
    }
  }
}

/// A live WASAPI capture session, owned by the engine thread.
struct Session {
  client: IAudioClient,
  capture: IAudioCaptureClient,
  normalizer: PcmNormalizer,
  block_bytes: usize,
}

impl Session {
  /// Drain packets until the consumer vanishes or `stop` is set. Every
  /// WASAPI failure maps to a [`CaptureError`]: the device simply cannot
  /// be captured further.
  fn pump(
    &mut self,
    bridge: &FrameSender<RawAudioFrame>,
    stop: &AtomicBool,
  ) -> Result<(), CaptureError> {
    while !stop.load(Ordering::Relaxed) {
      let packets = match unsafe { self.capture.GetNextPacketSize() } {
        Ok(0) => {
          std::thread::sleep(PACKET_POLL);
          continue;
        }
        Ok(packets) => packets,
        Err(error) => return Err(stopping_cause("packet poll", &error)),
      };
      for _ in 0..packets {
        let mut data: *mut u8 = core::ptr::null_mut();
        let mut frames = 0_u32;
        let mut flags = 0_u32;
        if let Err(error) = unsafe {
          self
            .capture
            .GetBuffer(&raw mut data, &raw mut frames, &raw mut flags, None, None)
        } {
          return Err(stopping_cause("packet read", &error));
        }
        // The payload length is derived from the device's own frame count,
        // so it can never exceed the buffer WASAPI handed us. A null
        // buffer with frames > 0 (silent packets from some drivers) is
        // synthesized as zeros — never exposed as uninitialized memory.
        let synthesized;
        let payload: &[u8] = match u64::from(frames)
          .checked_mul(u64::try_from(self.block_bytes).unwrap_or(u64::MAX))
          .and_then(|bytes| usize::try_from(bytes).ok())
        {
          Some(bytes) if !data.is_null() => unsafe { core::slice::from_raw_parts(data, bytes) },
          Some(bytes) => {
            synthesized = vec![0_u8; bytes];
            &synthesized
          }
          None => &[],
        };
        let frame = self
          .normalizer
          .packet_to_frame(payload, flags, Instant::now());
        if let Err(error) = unsafe { self.capture.ReleaseBuffer(frames) } {
          return Err(stopping_cause("packet release", &error));
        }
        let Some(frame) = frame else { continue };
        match bridge.send(frame) {
          SendOutcome::Sent | SendOutcome::Full => {}
          // The pipeline is gone; stop the loop, which releases the
          // device.
          SendOutcome::Closed => {
            stop.store(true, Ordering::Relaxed);
            break;
          }
        }
      }
    }
    let _ = unsafe { self.client.Stop() };
    Ok(())
  }
}

/// Map a mid-stream WASAPI failure to the [`CaptureError`] the consumer
/// sees, logging the raw COM error.
fn stopping_cause(context: &str, error: &WindowsError) -> CaptureError {
  tracing::warn!("WASAPI {context} failed: {error}");
  if error.code() == AUDCLNT_E_DEVICE_INVALIDATED {
    CaptureError::AudioDeviceLost
  } else {
    CaptureError::Stopped
  }
}

/// Activate the default render endpoint and start shared loopback
/// capture. COM must already be initialized on this thread.
fn initialize() -> Result<Session, CaptureError> {
  let enumerator: IMMDeviceEnumerator =
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL) }.map_err(
      |error| {
        tracing::debug!("WASAPI device enumeration unavailable: {error}");
        CaptureError::AudioNotSupported
      },
    )?;
  let device =
    unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.map_err(|error| {
      tracing::debug!("no default audio render endpoint: {error}");
      CaptureError::AudioStart("no default audio output device".to_owned())
    })?;
  let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|error| {
    CaptureError::AudioStart(format!("could not open the WASAPI audio client ({error})"))
  })?;
  let mix = unsafe { client.GetMixFormat() }.map_err(|error| {
    CaptureError::AudioStart(format!("could not read the mix format ({error})"))
  })?;
  if mix.is_null() {
    return Err(CaptureError::AudioStart(
      "the audio mix format was null".to_owned(),
    ));
  }
  let Some(format) = (unsafe { SourceFormat::from_wave_format(mix) }) else {
    // `WAVEFORMATEX` is packed; copy the fields out before formatting.
    let header = unsafe { &*mix };
    let (tag, channels, rate, bits) = (
      header.wFormatTag,
      header.nChannels,
      header.nSamplesPerSec,
      header.wBitsPerSample,
    );
    unsafe { CoTaskMemFree(Some(mix.cast_const().cast())) };
    return Err(CaptureError::AudioStart(format!(
      "unsupported audio mix format (tag {tag}, {channels} ch, {rate} Hz, {bits} bit)"
    )));
  };
  let started = unsafe {
    client.Initialize(
      AUDCLNT_SHAREMODE_SHARED,
      AUDCLNT_STREAMFLAGS_LOOPBACK,
      LOOPBACK_BUFFER_DURATION,
      0,
      mix,
      None,
    )
  };
  unsafe { CoTaskMemFree(Some(mix.cast_const().cast())) };
  started.map_err(|error| {
    CaptureError::AudioStart(format!(
      "WASAPI rejected the mix format for loopback ({error})"
    ))
  })?;
  let capture: IAudioCaptureClient = unsafe { client.GetService() }.map_err(|error| {
    CaptureError::AudioStart(format!("could not get the capture client ({error})"))
  })?;
  unsafe { client.Start() }
    .map_err(|error| CaptureError::AudioStart(format!("could not start capture ({error})")))?;
  tracing::debug!(
    rate = format.rate,
    channels = format.channels,
    "started WASAPI loopback capture"
  );
  Ok(Session {
    client,
    capture,
    normalizer: PcmNormalizer::new(format),
    block_bytes: format.frame_bytes(),
  })
}

/// Engine-thread body: COM lifetime, the startup handshake, the capture
/// loop, and the cleanup that runs when the thread's locals drop.
fn engine(
  bridge: &FrameSender<RawAudioFrame>,
  stop: &AtomicBool,
  failure: &Mutex<Option<CaptureError>>,
  ready: &Sender<Result<(), CaptureError>>,
) {
  // The capture thread owns the COM apartment: device activation, the
  // capture loop, and the final releases all run on this thread.
  let coinit = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
  if coinit.ok().is_err() {
    let _ = ready.send(Err(CaptureError::AudioStart(format!(
      "could not initialize COM for audio capture ({coinit})"
    ))));
    bridge.close();
    return;
  }
  match initialize() {
    Err(error) => {
      let _ = ready.send(Err(error));
    }
    Ok(mut session) => {
      let _ = ready.send(Ok(()));
      if let Err(error) = session.pump(bridge, stop) {
        tracing::warn!("system audio capture stopped: {error}");
        if let Ok(mut cause) = failure.lock() {
          *cause = Some(error);
        }
      }
    }
  }
  // The session drops here, releasing the WASAPI interfaces, before the
  // apartment is left; closing the bridge then wakes `next_audio` with
  // the recorded cause.
  unsafe { CoUninitialize() }
  bridge.close();
}

/// Windows system-audio capture: WASAPI loopback of the default render
/// endpoint, normalized to 48 kHz stereo.
pub struct PlatformAudioCapture {
  rx: Receiver<RawAudioFrame>,
  stop: Arc<AtomicBool>,
  engine: Option<JoinHandle<()>>,
  /// Why the engine thread died (if it did); reported once by
  /// [`AudioCaptureSource::next_audio`].
  failure: Arc<Mutex<Option<CaptureError>>>,
}

impl PlatformAudioCapture {
  /// Activate the default render endpoint and start the capture engine.
  ///
  /// # Errors
  ///
  /// See [`CaptureError`]: [`CaptureError::AudioNotSupported`] when the
  /// audio endpoint subsystem is unavailable, [`CaptureError::AudioStart`]
  /// when there is no default output device or it rejects loopback
  /// capture.
  pub fn new() -> Result<Self, CaptureError> {
    let (bridge, rx) = FrameSender::bounded(QUEUE_BUFFERS);
    let bridge = Arc::new(bridge);
    let stop = Arc::new(AtomicBool::new(false));
    let failure = Arc::new(Mutex::new(None));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let engine = std::thread::Builder::new()
      .name("lumen-wasapi-audio".to_owned())
      .spawn({
        let bridge = Arc::clone(&bridge);
        let stop = Arc::clone(&stop);
        let failure = Arc::clone(&failure);
        move || engine(&bridge, &stop, &failure, &ready_tx)
      })
      .map_err(|error| {
        CaptureError::AudioStart(format!("could not spawn the audio engine ({error})"))
      })?;
    // Wait for the startup handshake so `lumen serve` can fall back to
    // video-only synchronously.
    match ready_rx.recv() {
      Ok(Ok(())) => Ok(Self {
        rx,
        stop,
        engine: Some(engine),
        failure,
      }),
      Ok(Err(error)) => {
        stop.store(true, Ordering::Relaxed);
        Err(error)
      }
      Err(_) => Err(CaptureError::AudioStart(
        "the audio engine died before reporting readiness".to_owned(),
      )),
    }
  }
}

impl AudioCaptureSource for PlatformAudioCapture {
  fn next_audio(&mut self) -> Result<Option<RawAudioFrame>, CaptureError> {
    match self.rx.recv_timeout(POLL_INTERVAL) {
      Ok(frame) => Ok(Some(frame)),
      Err(RecvTimeoutError::Timeout) => Ok(None),
      Err(RecvTimeoutError::Disconnected) => Err(
        self
          .failure
          .lock()
          .ok()
          .and_then(|mut cause| cause.take())
          .unwrap_or(CaptureError::Stopped),
      ),
    }
  }
}

impl Drop for PlatformAudioCapture {
  fn drop(&mut self) {
    // Ask the engine to stop, then join: the pump checks the flag every
    // poll tick, and the thread releases the audio client, the capture
    // client, and the COM apartment as it exits.
    self.stop.store(true, Ordering::Relaxed);
    if let Some(engine) = self.engine.take() {
      if engine.join().is_err() {
        tracing::warn!("audio capture thread panicked during shutdown");
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn wave_format(tag: u16, channels: u16, rate: u32, bits: u16) -> WAVEFORMATEX {
    WAVEFORMATEX {
      wFormatTag: tag,
      nChannels: channels,
      nSamplesPerSec: rate,
      nAvgBytesPerSec: rate * u32::from(channels) * u32::from(bits) / 8,
      nBlockAlign: channels * (bits / 8),
      wBitsPerSample: bits,
      cbSize: 0,
    }
  }

  fn f32_bytes(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|v| v.to_le_bytes()).collect()
  }

  fn frame_lanes(samples: &Bytes) -> Vec<f32> {
    samples
      .chunks_exact(8)
      .flat_map(|pair| {
        [
          f32::from_le_bytes(pair[..4].try_into().expect("L")),
          f32::from_le_bytes(pair[4..].try_into().expect("R")),
        ]
      })
      .collect()
  }

  #[test]
  fn parses_plain_float_and_pcm_headers() {
    let float = wave_format(
      u16::try_from(WAVE_FORMAT_IEEE_FLOAT).expect("fits u16"),
      2,
      48_000,
      32,
    );
    assert_eq!(
      unsafe { SourceFormat::from_wave_format(&raw const float) },
      Some(SourceFormat {
        rate: 48_000,
        channels: 2,
        kind: SampleKind::Float32
      })
    );
    let pcm = wave_format(
      u16::try_from(WAVE_FORMAT_PCM).expect("fits u16"),
      2,
      44_100,
      16,
    );
    assert_eq!(
      unsafe { SourceFormat::from_wave_format(&raw const pcm) },
      Some(SourceFormat {
        rate: 44_100,
        channels: 2,
        kind: SampleKind::Int16
      })
    );
    let pcm32 = wave_format(
      u16::try_from(WAVE_FORMAT_PCM).expect("fits u16"),
      1,
      48_000,
      32,
    );
    assert_eq!(
      unsafe { SourceFormat::from_wave_format(&raw const pcm32) },
      Some(SourceFormat {
        rate: 48_000,
        channels: 1,
        kind: SampleKind::Int32
      })
    );
  }

  #[test]
  fn parses_extensible_subformats() {
    let float = WAVEFORMATEXTENSIBLE {
      Format: wave_format(
        u16::try_from(WAVE_FORMAT_EXTENSIBLE).expect("fits u16"),
        2,
        44_100,
        32,
      ),
      SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
      ..Default::default()
    };
    assert_eq!(
      unsafe { SourceFormat::from_wave_format(&raw const float.Format) },
      Some(SourceFormat {
        rate: 44_100,
        channels: 2,
        kind: SampleKind::Float32
      })
    );
    let pcm = WAVEFORMATEXTENSIBLE {
      Format: wave_format(
        u16::try_from(WAVE_FORMAT_EXTENSIBLE).expect("fits u16"),
        2,
        44_100,
        16,
      ),
      SubFormat: KSDATAFORMAT_SUBTYPE_PCM,
      ..Default::default()
    };
    assert_eq!(
      unsafe { SourceFormat::from_wave_format(&raw const pcm.Format) },
      Some(SourceFormat {
        rate: 44_100,
        channels: 2,
        kind: SampleKind::Int16
      })
    );
  }

  #[test]
  fn rejects_bogus_formats() {
    // Unknown format tag.
    let bogus = wave_format(0x9999, 2, 48_000, 32);
    assert_eq!(
      unsafe { SourceFormat::from_wave_format(&raw const bogus) },
      None
    );
    // Extensible header with a sub-format that is neither float nor PCM.
    let ext = WAVEFORMATEXTENSIBLE {
      Format: wave_format(
        u16::try_from(WAVE_FORMAT_EXTENSIBLE).expect("fits u16"),
        2,
        48_000,
        32,
      ),
      SubFormat: windows::core::GUID::zeroed(),
      ..Default::default()
    };
    assert_eq!(
      unsafe { SourceFormat::from_wave_format(&raw const ext.Format) },
      None
    );
    // Integer PCM with a bit depth that is not a native sample width.
    let weird = wave_format(
      u16::try_from(WAVE_FORMAT_PCM).expect("fits u16"),
      2,
      48_000,
      12,
    );
    assert_eq!(
      unsafe { SourceFormat::from_wave_format(&raw const weird) },
      None
    );
    // Block align inconsistent with the channel/bit layout.
    let mut broken = wave_format(
      u16::try_from(WAVE_FORMAT_IEEE_FLOAT).expect("fits u16"),
      2,
      48_000,
      32,
    );
    broken.nBlockAlign = 4;
    assert_eq!(
      unsafe { SourceFormat::from_wave_format(&raw const broken) },
      None
    );
  }

  #[test]
  fn native_48k_stereo_float32_passes_through() {
    let mut normalizer = PcmNormalizer::new(SourceFormat {
      rate: 48_000,
      channels: 2,
      kind: SampleKind::Float32,
    });
    let data = interleave(&[1.0, -1.0], &[0.5, -0.5]);
    let frame = normalizer
      .packet_to_frame(&data, 0, Instant::now())
      .expect("native packet");
    assert_eq!(frame.samples.as_ref(), data.as_slice());
    assert_eq!(frame.sample_rate, AUDIO_SAMPLE_RATE);
    assert_eq!(frame.channels, AUDIO_CHANNELS);
  }

  #[test]
  fn resamples_44_1k_to_48k() {
    // A steady 440 Hz tone fed as two packets, like a real device.
    const TONE_FRAMES: usize = 1024;
    let mut normalizer = PcmNormalizer::new(SourceFormat {
      rate: 44_100,
      channels: 2,
      kind: SampleKind::Float32,
    });
    let mut total = 0_usize;
    let mut peak = 0.0_f32;
    for packet in 0..2_u32 {
      let mut lane = Vec::with_capacity(TONE_FRAMES);
      for n in 0..u32::try_from(TONE_FRAMES).expect("small") {
        let index = packet * u32::try_from(TONE_FRAMES).expect("small") + n;
        let phase = f64::from(index) * 440.0 * 2.0 * std::f64::consts::PI / 44_100.0;
        #[expect(
          clippy::cast_possible_truncation,
          reason = "tone amplitude is well within f32"
        )]
        lane.push(phase.sin() as f32);
      }
      let data = interleave(&lane, &lane);
      let frame = normalizer
        .packet_to_frame(&data, 0, Instant::now())
        .expect("resampled frame");
      assert_eq!(frame.sample_rate, AUDIO_SAMPLE_RATE);
      assert_eq!(frame.channels, AUDIO_CHANNELS);
      let samples = frame_lanes(&frame.samples);
      assert!(samples.iter().all(|s| s.is_finite()), "NaN in resampled");
      peak = samples.iter().fold(peak, |m, s| m.max(s.abs()));
      total += samples.len() / 2;
    }
    // 2048 input frames scale to ~2229 output frames; the streaming
    // filter holds ~32 input samples of history, so allow a small band.
    let expected = f64::from(u32::try_from(TONE_FRAMES).expect("small") * 2) * 48_000.0 / 44_100.0;
    let delta = (f64::from(u32::try_from(total).expect("small")) - expected).abs();
    assert!(
      delta < 64.0,
      "{total} output frames, expected ~{expected:.0}"
    );
    // A band-limited tone keeps its amplitude through the resampler.
    assert!((0.5..=1.05).contains(&peak), "peak {peak}");
  }

  #[test]
  fn mono_duplicates_into_stereo() {
    let mut normalizer = PcmNormalizer::new(SourceFormat {
      rate: 48_000,
      channels: 1,
      kind: SampleKind::Float32,
    });
    let data = f32_bytes(&[0.25, -0.5]);
    let frame = normalizer
      .packet_to_frame(&data, 0, Instant::now())
      .expect("mono frame");
    assert_eq!(
      frame.samples.as_ref(),
      interleave(&[0.25, -0.5], &[0.25, -0.5]).as_slice()
    );
  }

  #[test]
  fn surround_downmixes_to_stereo() {
    let mut normalizer = PcmNormalizer::new(SourceFormat {
      rate: 48_000,
      channels: 4,
      kind: SampleKind::Float32,
    });
    // One 4-channel frame: L/F/C average to 1.0, R/LFE average to 0.0.
    let data = f32_bytes(&[1.0, 0.0, 1.0, 0.0]);
    let frame = normalizer
      .packet_to_frame(&data, 0, Instant::now())
      .expect("surround frame");
    assert_eq!(
      frame.samples.as_ref(),
      interleave(&[1.0], &[0.0]).as_slice()
    );
  }

  #[test]
  fn silent_packets_become_silence() {
    // Native format: garbage contents must never reach the output.
    let mut native = PcmNormalizer::new(SourceFormat {
      rate: 48_000,
      channels: 2,
      kind: SampleKind::Float32,
    });
    let garbage = vec![0xAA_u8; 1024 * 8];
    let frame = native
      .packet_to_frame(&garbage, SILENT_PACKET, Instant::now())
      .expect("silent native frame");
    assert_eq!(frame.samples.len(), 1024 * 8);
    assert!(frame.samples.iter().all(|&b| b == 0));
    // Resampled path: silence stays silence through the filter.
    let mut resampled = PcmNormalizer::new(SourceFormat {
      rate: 44_100,
      channels: 2,
      kind: SampleKind::Int16,
    });
    let garbage = vec![0xAA_u8; 1024 * 4];
    let frame = resampled
      .packet_to_frame(&garbage, SILENT_PACKET, Instant::now())
      .expect("silent resampled frame");
    assert!(frame.samples.iter().all(|&b| b == 0));
  }

  #[test]
  fn malformed_and_empty_packets_are_dropped() {
    let mut normalizer = PcmNormalizer::new(SourceFormat {
      rate: 48_000,
      channels: 2,
      kind: SampleKind::Float32,
    });
    // A partial frame cannot be decoded.
    assert!(
      normalizer
        .packet_to_frame(&[0; 7], 0, Instant::now())
        .is_none()
    );
    assert!(normalizer.packet_to_frame(&[], 0, Instant::now()).is_none());
  }

  #[test]
  fn integer_formats_scale_to_float() {
    let mut pcm16 = PcmNormalizer::new(SourceFormat {
      rate: 48_000,
      channels: 2,
      kind: SampleKind::Int16,
    });
    let data: Vec<u8> = [i16::MIN, i16::MAX, 0, 8_192]
      .iter()
      .flat_map(|v| v.to_le_bytes())
      .collect();
    let frame = pcm16
      .packet_to_frame(&data, 0, Instant::now())
      .expect("int16 frame");
    let samples = frame_lanes(&frame.samples);
    assert_eq!(samples, vec![-1.0, 1.0 - 1.0 / 32_768.0, 0.0, 0.25]);
    // 32-bit integer (and 24-bit packed into 32 bits) scales to [-1, 1].
    let mut pcm32 = PcmNormalizer::new(SourceFormat {
      rate: 48_000,
      channels: 1,
      kind: SampleKind::Int32,
    });
    let data: Vec<u8> = [i32::MIN, 1 << 30]
      .iter()
      .flat_map(|v| v.to_le_bytes())
      .collect();
    let frame = pcm32
      .packet_to_frame(&data, 0, Instant::now())
      .expect("int32 frame");
    assert_eq!(
      frame.samples.as_ref(),
      interleave(&[-1.0, 0.5], &[-1.0, 0.5]).as_slice()
    );
  }
}
