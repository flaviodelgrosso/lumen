//! Capture → encode → fan-out pipelines (video + optional audio).
//!
//! Video: one capture source and one encoder feed a bounded `broadcast`
//! channel; every WebRTC peer subscribes independently. The capture side
//! publishes into a `watch` channel so the encoder always sees the
//! **latest** frame: slow encoding drops stale frames instead of queueing
//! latency.
//!
//! Audio: capture and Opus encoding share one blocking task (encoding a
//! 20 ms packet costs far less than the packet's duration), feeding a
//! second fan-out. Every PCM buffer must reach the encoder — dropping
//! audio frames would glitch the stream — so there is no latest-only
//! shortcut here.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use lumen_capture::{AudioCaptureSource, CaptureError, CaptureSource};
use lumen_core::{EncodedAudioFrame, EncodedFrame, PipelineStats, RawFrame};
use lumen_encoder::{AudioEncoder, EncodeError, VideoEncoder};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

/// Broadcast capacity for the audio fan-out (20 ms packets → ~2 s).
const AUDIO_FANOUT: usize = 100;

/// Handle on a running pipeline.
pub struct PipelineHandle {
  /// Encoded-frame fan-out.
  pub frames: broadcast::Sender<Arc<EncodedFrame>>,
  /// Encoded-audio fan-out; `None` when audio is disabled.
  pub audio: Option<broadcast::Sender<Arc<EncodedAudioFrame>>>,
  /// Shared counters.
  pub stats: Arc<PipelineStats>,
  shutdown: CancellationToken,
  capture: Option<tokio::task::JoinHandle<()>>,
  encode: Option<tokio::task::JoinHandle<()>>,
  audio_task: Option<tokio::task::JoinHandle<()>>,
}

impl PipelineHandle {
  /// Stop capture and encoding, waiting (bounded) for all tasks to exit.
  pub async fn shutdown(mut self) {
    self.shutdown.cancel();
    for task in [&mut self.capture, &mut self.encode, &mut self.audio_task] {
      if let Some(handle) = task.take() {
        let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;
      }
    }
  }
}

/// Pipeline failure.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
  /// Capture failed before the first frame.
  #[error(transparent)]
  Capture(#[from] CaptureError),
}

/// Start the pipelines. `keyframe_interval_secs` drives a periodic IDR in
/// addition to the on-join requests delivered via `keyframe_requests`.
/// `audio` optionally adds the system-audio capture/encode pipeline.
///
/// # Errors
///
/// Returns [`PipelineError`] when the first frame cannot be captured.
pub fn start(
  mut source: Box<dyn CaptureSource>,
  mut encoder: Box<dyn VideoEncoder>,
  fps: u32,
  keyframe_interval_secs: u64,
  shutdown: CancellationToken,
  mut keyframe_requests: mpsc::Receiver<()>,
  audio: Option<(Box<dyn AudioCaptureSource>, Box<dyn AudioEncoder>)>,
) -> Result<PipelineHandle, PipelineError> {
  // Prime: fail fast if the display cannot deliver a frame.
  let first = source.next_frame()?;

  let stats = Arc::new(PipelineStats::default());
  let capacity = (fps.max(1) * 2).clamp(4, 120);
  stats.fanout_capacity.store(capacity, Ordering::Relaxed);
  let (frames_tx, _) = broadcast::channel::<Arc<EncodedFrame>>(capacity as usize);
  let (raw_tx, mut raw_rx) = watch::channel(Some(first));

  // ── Capture task (blocking thread: next_frame waits on the native engine) ──
  let capture_stats = Arc::clone(&stats);
  let capture_shutdown = shutdown.clone();
  let capture = tokio::task::spawn_blocking(move || {
    loop {
      if capture_shutdown.is_cancelled() || raw_tx.receiver_count() == 0 {
        break;
      }
      match source.next_frame() {
        Ok(frame) => {
          capture_stats.captured.fetch_add(1, Ordering::Relaxed);
          // The watch keeps only the latest frame: stale frames are
          // replaced, never queued (bounded memory, no latency debt).
          raw_tx.send_replace(Some(frame));
        }
        Err(CaptureError::Stopped) => break,
        Err(e) => {
          tracing::warn!("capture error: {e}");
          break;
        }
      }
    }
  });

  // ── Encode task ──
  let encode_stats = Arc::clone(&stats);
  let encode_shutdown = shutdown.clone();
  let encode_frames = frames_tx.clone();
  let encode = tokio::spawn(async move {
    let mut period = tokio::time::interval(Duration::from_secs(keyframe_interval_secs.max(1)));
    period.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    period.tick().await;
    loop {
      tokio::select! {
          () = encode_shutdown.cancelled() => break,
          _ = keyframe_requests.recv() => encoder.request_keyframe(),
          _ = period.tick() => encoder.request_keyframe(),
          changed = raw_rx.changed() => {
              if changed.is_err() {
                  break;
              }
              let Some(frame) = raw_rx.borrow_and_update().clone() else {
                  continue;
              };
              match encode_one(&mut encoder, &frame, &encode_frames, &encode_stats) {
                  Ok(()) => {}
                  Err(EncodeError::DimensionMismatch { .. }) => {
                      // The encoder was configured for the old size.
                      tracing::error!("display resolution changed; restart lumen");
                      break;
                  }
                  Err(e) => {
                      tracing::warn!("encode failed: {e}");
                  }
              }
          }
      }
    }
  });

  let (audio_tx, audio_task) = spawn_audio(audio, shutdown.clone(), &stats);

  tracing::debug!(capacity, audio = audio_tx.is_some(), "pipeline started");
  Ok(PipelineHandle {
    frames: frames_tx,
    audio: audio_tx,
    stats,
    shutdown,
    capture: Some(capture),
    encode: Some(encode),
    audio_task,
  })
}

/// Spawn the audio capture + Opus encode pipeline on one blocking thread.
///
/// Every PCM buffer must reach the encoder (dropped buffers glitch audio),
/// so this task owns both stages; a `None` from the source is a poll
/// timeout that keeps shutdown responsive. The task deliberately keeps
/// running with zero viewers so the first viewer and reconnecting viewers
/// can receive audio without restarting capture.
fn spawn_audio(
  audio: Option<(Box<dyn AudioCaptureSource>, Box<dyn AudioEncoder>)>,
  shutdown: CancellationToken,
  stats: &Arc<PipelineStats>,
) -> (
  Option<broadcast::Sender<Arc<EncodedAudioFrame>>>,
  Option<tokio::task::JoinHandle<()>>,
) {
  let Some((mut audio_source, mut audio_encoder)) = audio else {
    return (None, None);
  };
  let (audio_frames_tx, _) = broadcast::channel::<Arc<EncodedAudioFrame>>(AUDIO_FANOUT);
  let audio_stats = Arc::clone(stats);
  let audio_frames = audio_frames_tx.clone();
  let handle = tokio::task::spawn_blocking(move || {
    loop {
      if shutdown.is_cancelled() {
        break;
      }
      match audio_source.next_audio() {
        Ok(None) => {}
        Ok(Some(frame)) => {
          audio_stats.audio_captured.fetch_add(1, Ordering::Relaxed);
          match audio_encoder.encode_audio(&frame) {
            Ok(packets) => {
              audio_stats
                .audio_encoded
                .fetch_add(packets.len() as u64, Ordering::Relaxed);
              for packet in packets {
                let _ = audio_frames.send(Arc::new(packet));
              }
            }
            Err(EncodeError::UnsupportedAudio { .. }) => {
              tracing::error!("audio format not encodable; audio stopped");
              break;
            }
            Err(e) => tracing::warn!("audio encode failed: {e}"),
          }
        }
        Err(CaptureError::Stopped) => break,
        Err(e) => {
          tracing::warn!("audio capture error: {e}");
          break;
        }
      }
    }
  });
  (Some(audio_frames_tx), Some(handle))
}

#[expect(
  clippy::cast_possible_truncation,
  reason = "elapsed is clamped to u32::MAX microseconds before casting"
)]
fn encode_one(
  encoder: &mut Box<dyn VideoEncoder>,
  frame: &RawFrame,
  frames_tx: &broadcast::Sender<Arc<EncodedFrame>>,
  stats: &Arc<PipelineStats>,
) -> Result<(), EncodeError> {
  let started = Instant::now();
  let Some(unit) = encoder.encode(frame)? else {
    return Ok(());
  };
  let micros = started.elapsed().as_micros().min(u128::from(u32::MAX)) as u64;
  // Exponential moving average, α = 1/8.
  let prev = stats.encode_latency_us.load(Ordering::Relaxed);
  let ema = if prev == 0 {
    micros
  } else {
    (prev * 7 + micros) / 8
  };
  stats.encode_latency_us.store(ema, Ordering::Relaxed);
  stats.encoded.fetch_add(1, Ordering::Relaxed);
  stats
    .subscribers
    .store(frames_tx.receiver_count(), Ordering::Relaxed);
  let _ = frames_tx.send(Arc::new(unit));
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn audio_survives_without_viewers_and_after_disconnect() {
    let shutdown = CancellationToken::new();
    let stats = Arc::new(PipelineStats::default());
    let (sender, task) = spawn_audio(
      Some((
        Box::new(lumen_capture::FakeAudioCapture::new()),
        Box::new(lumen_encoder::OpusAudioEncoder::new(lumen_core::Bitrate(128_000)).unwrap()),
      )),
      shutdown.clone(),
      &stats,
    );
    let sender = sender.unwrap();
    let received = tokio::time::timeout(Duration::from_secs(3), async {
      while stats.audio_encoded.load(Ordering::Relaxed) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
      }
      let mut first_viewer = sender.subscribe();
      let first = first_viewer.recv().await.unwrap();
      drop(first_viewer);
      let disconnected_at = stats.audio_encoded.load(Ordering::Relaxed);
      while stats.audio_encoded.load(Ordering::Relaxed) == disconnected_at {
        tokio::time::sleep(Duration::from_millis(5)).await;
      }
      let mut next_viewer = sender.subscribe();
      let next = next_viewer.recv().await.unwrap();
      assert!(next.sequence > first.sequence);
    })
    .await;
    shutdown.cancel();
    task.unwrap().await.unwrap();
    received.expect("audio must reach late and returning viewers");
  }
}
