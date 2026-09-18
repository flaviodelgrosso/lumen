//! WebRTC peers serving one shared H.264 video stream with optional Opus
//! audio to browsers.
//!
//! Built on `webrtc` / `rtc` 0.20 (webrtc-rs). Each connected viewer gets
//! its own peer connection and local video track (plus an audio track when
//! available), while all peers consume the same encoder fan-out.
//!
//! Codecs: H.264 Constrained Baseline `profile-level-id=42e01f` and
//! `42001f`, `packetization-mode=1` (the configuration the webrtc-rs
//! browser examples validate against Chromium and Safari), plus Opus
//! 48 kHz stereo for system audio.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lumen_core::{EncodedAudioFrame, EncodedFrame};
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::{
  MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine,
};
use rtc::peer_connection::configuration::{RTCConfigurationBuilder, RTCIceServer};
use rtc::rtp_transceiver::rtp_sender::{
  RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
  RtpCodecKind,
};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::peer_connection::{
  PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCPeerConnectionIceEvent,
  RTCPeerConnectionState, RTCSessionDescription,
};
use webrtc::rtp_transceiver::RtpSender;

/// Errors from peer setup or media writes.
#[derive(Debug, Error)]
pub enum WebRtcError {
  /// The WebRTC stack failed.
  #[error("WebRTC error: {0}")]
  Rtc(String),
  /// No H.264 codec was negotiated with the browser.
  #[error("browser did not negotiate an H.264 codec")]
  NoH264Codec,
  /// The peer connection is not connected yet.
  #[error("peer is not connected")]
  NotConnected,
}

/// Events a peer emits toward the signaling layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerEvent {
  /// ICE/DTLS/SRTP established; media flows.
  Connected,
  /// Temporarily lost connectivity; the viewer may recover.
  Disconnected,
  /// Connection failed or closed.
  Failed,
  /// A host ICE candidate to relay to the browser.
  IceCandidate {
    /// Candidate attribute string.
    candidate: String,
    /// SDP media line index, when known.
    sdp_m_line_index: Option<u16>,
  },
}

/// Payload type used before video negotiation completes (the `42e01f` entry).
const PRIMARY_PAYLOAD_TYPE: u8 = 125;

/// Payload type registered for Opus (the webrtc-rs default).
const OPUS_PAYLOAD_TYPE: u8 = 111;

/// Opus frame duration produced by the encoder.
const OPUS_FRAME_DURATION: Duration = Duration::from_millis(20);

/// Register the codecs Lumen offers: H.264 mode 1 (Chromium + Safari) for
/// video and Opus 48 kHz stereo for audio.
fn lumen_media_engine() -> Result<MediaEngine, WebRtcError> {
  let feedback = || {
    [
      ("goog-remb", ""),
      ("ccm", "fir"),
      ("nack", ""),
      ("nack", "pli"),
    ]
    .into_iter()
    .map(
      |(typ, parameter)| rtc::rtp_transceiver::rtp_sender::RTCPFeedback {
        typ: typ.to_owned(),
        parameter: parameter.to_owned(),
      },
    )
    .collect()
  };
  let h264 = |payload_type: u8, profile: &str| RTCRtpCodecParameters {
    rtp_codec: RTCRtpCodec {
      mime_type: MIME_TYPE_H264.to_owned(),
      clock_rate: 90_000,
      channels: 0,
      sdp_fmtp_line: format!(
        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile}"
      ),
      rtcp_feedback: feedback(),
    },
    payload_type,
  };
  let rtx = |payload_type: u8, apt: u8| RTCRtpCodecParameters {
    rtp_codec: RTCRtpCodec {
      mime_type: "video/rtx".to_owned(),
      clock_rate: 90_000,
      channels: 0,
      sdp_fmtp_line: format!("apt={apt}"),
      rtcp_feedback: vec![],
    },
    payload_type,
  };

  let mut engine = MediaEngine::default();
  for codec in [
    h264(125, "42e01f"),
    rtx(105, 125),
    h264(102, "42001f"),
    rtx(103, 102),
  ] {
    engine
      .register_codec(codec, RtpCodecKind::Video)
      .map_err(|e| WebRtcError::Rtc(e.to_string()))?;
  }
  engine
    .register_codec(
      RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
          mime_type: MIME_TYPE_OPUS.to_owned(),
          clock_rate: 48_000,
          channels: 2,
          sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
          rtcp_feedback: vec![],
        },
        payload_type: OPUS_PAYLOAD_TYPE,
      },
      RtpCodecKind::Audio,
    )
    .map_err(|e| WebRtcError::Rtc(e.to_string()))?;
  Ok(engine)
}

/// Bridges webrtc-rs callbacks to the signaling task.
struct PeerHandler {
  events: mpsc::UnboundedSender<PeerEvent>,
  gather_done: Mutex<Option<oneshot::Sender<()>>>,
  connected: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for PeerHandler {
  async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
    let Ok(init) = event.candidate.to_json() else {
      return;
    };
    if init.candidate.is_empty() {
      return; // end-of-candidates sentinel
    }
    let _ = self.events.send(PeerEvent::IceCandidate {
      candidate: init.candidate,
      sdp_m_line_index: init.sdp_mline_index,
    });
  }

  async fn on_ice_gathering_state_change(
    &self,
    state: webrtc::peer_connection::RTCIceGatheringState,
  ) {
    if state == webrtc::peer_connection::RTCIceGatheringState::Complete {
      if let Some(tx) = self.gather_done.lock().ok().and_then(|mut g| g.take()) {
        let _ = tx.send(());
      }
    }
  }

  async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
    let event = match state {
      RTCPeerConnectionState::Connected => {
        self
          .connected
          .store(true, std::sync::atomic::Ordering::SeqCst);
        PeerEvent::Connected
      }
      RTCPeerConnectionState::Disconnected => {
        self
          .connected
          .store(false, std::sync::atomic::Ordering::SeqCst);
        PeerEvent::Disconnected
      }
      RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
        self
          .connected
          .store(false, std::sync::atomic::Ordering::SeqCst);
        PeerEvent::Failed
      }
      _ => return,
    };
    let _ = self.events.send(event);
  }
}

/// One viewer's peer connection with an H.264 video track and, when enabled,
/// an Opus audio track fed from the shared encoder fan-out.
pub struct Peer {
  pc: Arc<dyn PeerConnection>,
  track: Arc<TrackLocalStaticSample>,
  sender: Arc<dyn RtpSender>,
  ssrc: u32,
  negotiated_pt: AsyncMutex<Option<u8>>,
  audio: Option<PeerAudio>,
  events: mpsc::UnboundedReceiver<PeerEvent>,
  /// Trickled candidates that arrived before the answer; webrtc-rs
  /// rejects `add_ice_candidate` without a remote description.
  pending_candidates: std::sync::Mutex<Vec<(String, Option<u16>)>>,
  remote_set: std::sync::atomic::AtomicBool,
  connected: std::sync::Arc<std::sync::atomic::AtomicBool>,
  /// SDP offer to send to the browser (complete, non-trickle).
  pub offer_sdp: String,
}

struct PeerAudio {
  track: Arc<TrackLocalStaticSample>,
  sender: Arc<dyn RtpSender>,
  ssrc: u32,
  negotiated_pt: AsyncMutex<Option<u8>>,
  last_sequence: AsyncMutex<Option<u64>>,
}

fn random_ssrc() -> Result<u32, WebRtcError> {
  let mut buf = [0_u8; 4];
  getrandom::fill(&mut buf).map_err(|e| WebRtcError::Rtc(e.to_string()))?;
  // Keep the high bit clear so the value stays a positive i32-friendly id.
  Ok(u32::from_be_bytes(buf) & 0x7fff_ffff)
}

fn dropped_audio_packets(previous: Option<u64>, sequence: u64) -> u16 {
  previous.map_or(0, |previous| {
    u16::try_from(sequence.saturating_sub(previous).saturating_sub(1)).unwrap_or(u16::MAX)
  })
}

/// The H.264 video track (Constrained Baseline, mode 1) for one peer.
fn video_track(ssrc: u32) -> MediaStreamTrack {
  MediaStreamTrack::new(
    "lumen-stream".to_owned(),
    "lumen-video".to_owned(),
    "Lumen display".to_owned(),
    RtpCodecKind::Video,
    vec![RTCRtpEncodingParameters {
      rtp_coding_parameters: RTCRtpCodingParameters {
        ssrc: Some(ssrc),
        ..Default::default()
      },
      codec: RTCRtpCodec {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: 90_000,
        channels: 0,
        sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
          .to_owned(),
        rtcp_feedback: vec![],
      },
      ..Default::default()
    }],
  )
}

/// The Opus audio track (48 kHz stereo) for one peer.
fn audio_track(ssrc: u32) -> MediaStreamTrack {
  MediaStreamTrack::new(
    "lumen-stream".to_owned(),
    "lumen-audio".to_owned(),
    "Lumen system audio".to_owned(),
    RtpCodecKind::Audio,
    vec![RTCRtpEncodingParameters {
      rtp_coding_parameters: RTCRtpCodingParameters {
        ssrc: Some(ssrc),
        ..Default::default()
      },
      codec: RTCRtpCodec {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48_000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
        rtcp_feedback: vec![],
      },
      ..Default::default()
    }],
  )
}

impl Peer {
  /// Create a peer with video and audio tracks and produce a complete offer.
  ///
  /// Waits for ICE gathering (bounded) so the offer carries all host
  /// candidates; LAN-only, no STUN/TURN.
  ///
  /// # Errors
  ///
  /// See [`WebRtcError`].
  pub async fn new() -> Result<Self, WebRtcError> {
    Self::with_bind(vec!["0.0.0.0:0".to_owned()], true).await
  }

  /// Create a peer bound to specific local UDP addresses.
  ///
  /// When `audio_enabled` is false, no audio track or SDP media line is
  /// created. Each address is bound per interface as documented by
  /// `PeerConnectionBuilder::with_udp_addrs`; use `127.0.0.1:0` to
  /// restrict ICE to loopback (local testing) or a concrete LAN IP to
  /// pin one interface.
  ///
  /// # Errors
  ///
  /// See [`WebRtcError`].
  pub async fn with_bind(udp_addrs: Vec<String>, audio_enabled: bool) -> Result<Self, WebRtcError> {
    // Box the build future: webrtc-rs builder state makes it ~21 KB,
    // too large to inline into every caller's stack frame.
    Box::pin(Self::build(udp_addrs, audio_enabled)).await
  }

  #[expect(
    clippy::large_futures,
    reason = "webrtc-rs builder types carry large state; the public API boxes this"
  )]
  async fn build(udp_addrs: Vec<String>, audio_enabled: bool) -> Result<Self, WebRtcError> {
    let ssrc = random_ssrc()?;
    let audio_ssrc = audio_enabled.then(random_ssrc).transpose()?;
    let mut media_engine = lumen_media_engine()?;
    let registry =
      rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors(
        rtc::interceptor::Registry::new(),
        &mut media_engine,
      )
      .map_err(|e| WebRtcError::Rtc(e.to_string()))?;

    let config = RTCConfigurationBuilder::new()
            // LAN-only: host candidates suffice; no external STUN/TURN.
            .with_ice_servers(Vec::<RTCIceServer>::new())
            .build();

    // Browsers obfuscate their host candidates as mDNS `.local` names;
    // query mode resolves them so ICE pairs can form on the LAN.
    let mut setting_engine = webrtc::peer_connection::SettingEngine::default();
    setting_engine.set_multicast_dns_mode(rtc::ice::mdns::MulticastDnsMode::QueryOnly);
    setting_engine.set_multicast_dns_timeout(Some(Duration::from_secs(5)));

    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (gather_tx, gather_rx) = oneshot::channel();
    let connected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handler = Arc::new(PeerHandler {
      events: events_tx,
      gather_done: Mutex::new(Some(gather_tx)),
      connected: std::sync::Arc::clone(&connected),
    });

    let pc = PeerConnectionBuilder::new()
      .with_configuration(config)
      .with_media_engine(media_engine)
      .with_interceptor_registry(registry)
      .with_setting_engine(setting_engine)
      .with_handler(handler)
      .with_udp_addrs(udp_addrs)
      .build()
      .await
      .map_err(|e| WebRtcError::Rtc(e.to_string()))?;
    let pc: Arc<dyn PeerConnection> = Arc::new(pc);

    let video = Arc::new(
      TrackLocalStaticSample::new(video_track(ssrc))
        .map_err(|e| WebRtcError::Rtc(e.to_string()))?,
    );
    let sender = pc
      .add_track(Arc::clone(&video) as Arc<dyn TrackLocal>)
      .await
      .map_err(|e| WebRtcError::Rtc(e.to_string()))?;

    let audio = if let Some(audio_ssrc) = audio_ssrc {
      let track = Arc::new(
        TrackLocalStaticSample::new(audio_track(audio_ssrc))
          .map_err(|e| WebRtcError::Rtc(e.to_string()))?,
      );
      let sender = pc
        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
        .await
        .map_err(|e| WebRtcError::Rtc(e.to_string()))?;
      Some(PeerAudio {
        track,
        sender,
        ssrc: audio_ssrc,
        negotiated_pt: AsyncMutex::new(None),
        last_sequence: AsyncMutex::new(None),
      })
    } else {
      None
    };

    let offer = pc
      .create_offer(None)
      .await
      .map_err(|e| WebRtcError::Rtc(e.to_string()))?;
    pc.set_local_description(offer)
      .await
      .map_err(|e| WebRtcError::Rtc(e.to_string()))?;

    // Non-trickle: wait for gathering so the offer embeds all host
    // candidates. Bounded — a stalled gather still yields a usable
    // (candidate-less) offer.
    let _ = tokio::time::timeout(Duration::from_secs(3), gather_rx).await;
    let local = pc
      .local_description()
      .await
      .ok_or_else(|| WebRtcError::Rtc("no local description after offer".into()))?;

    Ok(Self {
      pc,
      track: video,
      sender,
      ssrc,
      negotiated_pt: AsyncMutex::new(None),
      audio,
      events: events_rx,
      pending_candidates: std::sync::Mutex::new(Vec::new()),
      remote_set: std::sync::atomic::AtomicBool::new(false),
      connected,
      offer_sdp: local.sdp,
    })
  }

  /// Take the event stream for signaling relay (call once).
  pub fn take_events(&mut self) -> mpsc::UnboundedReceiver<PeerEvent> {
    std::mem::replace(&mut self.events, mpsc::unbounded_channel().1)
  }

  /// Apply the browser's SDP answer and lock the negotiated payload type.
  ///
  /// # Errors
  ///
  /// See [`WebRtcError`].
  pub async fn apply_answer(&self, sdp: &str) -> Result<(), WebRtcError> {
    let answer =
      RTCSessionDescription::answer(sdp.to_owned()).map_err(|e| WebRtcError::Rtc(e.to_string()))?;
    self
      .pc
      .set_remote_description(answer)
      .await
      .map_err(|e| WebRtcError::Rtc(e.to_string()))?;

    // Flush trickled candidates that raced ahead of the answer.
    self
      .remote_set
      .store(true, std::sync::atomic::Ordering::SeqCst);
    let pending = {
      let mut guard = self
        .pending_candidates
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
      std::mem::take(&mut *guard)
    };
    for (candidate, index) in pending {
      self.add_ice_candidate_now(&candidate, index).await?;
    }

    let params = self
      .sender
      .get_parameters()
      .await
      .map_err(|e| WebRtcError::Rtc(e.to_string()))?;
    let pt = params
      .rtp_parameters
      .codecs
      .iter()
      .find(|c| c.rtp_codec.mime_type == MIME_TYPE_H264)
      .map_or(PRIMARY_PAYLOAD_TYPE, |c| c.payload_type);
    *self.negotiated_pt.lock().await = Some(pt);
    tracing::debug!(pt, ssrc = self.ssrc, "H.264 negotiated");

    if let Some(audio) = &self.audio {
      let audio_params = audio
        .sender
        .get_parameters()
        .await
        .map_err(|e| WebRtcError::Rtc(e.to_string()))?;
      let audio_pt = audio_params
        .rtp_parameters
        .codecs
        .iter()
        .find(|c| c.rtp_codec.mime_type == MIME_TYPE_OPUS)
        .map_or(OPUS_PAYLOAD_TYPE, |c| c.payload_type);
      *audio.negotiated_pt.lock().await = Some(audio_pt);
      tracing::debug!(audio_pt, ssrc = audio.ssrc, "Opus negotiated");
    }
    Ok(())
  }

  /// Relay a browser ICE candidate into the peer connection.
  ///
  /// # Errors
  ///
  /// See [`WebRtcError`].
  pub async fn add_ice_candidate(
    &self,
    candidate: &str,
    sdp_m_line_index: Option<u16>,
  ) -> Result<(), WebRtcError> {
    if self.remote_set.load(std::sync::atomic::Ordering::SeqCst) {
      return self
        .add_ice_candidate_now(candidate, sdp_m_line_index)
        .await;
    }
    self
      .pending_candidates
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner)
      .push((candidate.to_owned(), sdp_m_line_index));
    Ok(())
  }

  async fn add_ice_candidate_now(
    &self,
    candidate: &str,
    sdp_m_line_index: Option<u16>,
  ) -> Result<(), WebRtcError> {
    let init = webrtc::peer_connection::RTCIceCandidateInit {
      candidate: candidate.to_owned(),
      sdp_mline_index: sdp_m_line_index,
      ..Default::default()
    };
    self
      .pc
      .add_ice_candidate(init)
      .await
      .map_err(|e| WebRtcError::Rtc(e.to_string()))?;
    Ok(())
  }

  /// Whether ICE/DTLS/SRTP are established (media can flow).
  #[must_use]
  pub fn is_connected(&self) -> bool {
    self.connected.load(std::sync::atomic::Ordering::SeqCst)
  }

  /// Write one encoded access unit to the video track.
  ///
  /// `duration` is the elapsed capture time since the previous frame and
  /// drives the 90 kHz RTP clock. Frames are dropped until the peer is
  /// connected (writing earlier would trip the SRTP interceptor before
  /// key material exists; they would be stale anyway).
  ///
  /// # Errors
  ///
  /// See [`WebRtcError`].
  pub async fn write_frame(
    &self,
    frame: &EncodedFrame,
    duration: Duration,
  ) -> Result<(), WebRtcError> {
    if !self.is_connected() {
      return Ok(());
    }
    let Some(pt) = *self.negotiated_pt.lock().await else {
      return Ok(()); // not negotiated yet
    };
    let duration = duration.clamp(Duration::from_millis(1), Duration::from_millis(500));
    let sample = Sample {
      data: frame.data.clone(),
      duration,
      ..Default::default()
    };
    self
      .track
      .sample_writer(self.ssrc, pt)
      .write_sample(&sample)
      .await
      .map_err(|e| WebRtcError::Rtc(e.to_string()))?;
    Ok(())
  }

  /// Write one Opus packet to the audio track, when present.
  ///
  /// Packets are fixed 20 ms and drive the 48 kHz RTP clock. Sequence gaps
  /// advance both the RTP sequence number and timestamp so receiver loss
  /// concealment observes the real missing duration. Like video, packets
  /// are dropped until the peer is connected.
  ///
  /// # Errors
  ///
  /// See [`WebRtcError`].
  pub async fn write_audio(&self, frame: &EncodedAudioFrame) -> Result<(), WebRtcError> {
    if !self.is_connected() {
      return Ok(());
    }
    let Some(audio) = &self.audio else {
      return Ok(());
    };
    let Some(pt) = *audio.negotiated_pt.lock().await else {
      return Ok(()); // not negotiated yet
    };
    let mut last_sequence = audio.last_sequence.lock().await;
    let prev_dropped_packets = dropped_audio_packets(*last_sequence, frame.sequence);
    let sample = Sample {
      data: frame.data.clone(),
      duration: OPUS_FRAME_DURATION,
      prev_dropped_packets,
      ..Default::default()
    };
    audio
      .track
      .sample_writer(audio.ssrc, pt)
      .write_sample(&sample)
      .await
      .map_err(|e| WebRtcError::Rtc(e.to_string()))?;
    *last_sequence = Some(frame.sequence);
    Ok(())
  }

  /// Close the peer connection.
  pub async fn close(&self) {
    if let Err(e) = self.pc.close().await {
      tracing::debug!("peer close: {e}");
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn media_engine_registers_h264_and_opus() {
    // Construction must succeed and be deterministic per peer.
    let _engine = lumen_media_engine().expect("media engine");
  }

  #[test]
  fn ssrc_is_random() {
    let a = random_ssrc().unwrap();
    let b = random_ssrc().unwrap();
    assert_ne!(a, b);
    assert!(a < 0x8000_0000);
  }

  #[test]
  fn audio_sequence_gaps_count_missing_opus_packets() {
    assert_eq!(dropped_audio_packets(None, 900), 0);
    assert_eq!(dropped_audio_packets(Some(900), 901), 0);
    assert_eq!(dropped_audio_packets(Some(901), 905), 3);
  }

  #[tokio::test]
  async fn peer_generates_video_and_audio_offer() {
    let peer = Peer::new().await.expect("peer creation");
    assert!(peer.offer_sdp.contains("v=0"));
    assert!(
      peer.offer_sdp.to_uppercase().contains("H264"),
      "offer must advertise H264"
    );

    assert!(
      peer.offer_sdp.to_uppercase().contains("OPUS"),
      "offer must advertise Opus"
    );
    assert!(
      peer.offer_sdp.contains("m=audio"),
      "offer needs an audio m-line"
    );
    assert!(
      peer.offer_sdp.contains("m=video"),
      "offer needs a video m-line"
    );
    assert!(
      !peer.offer_sdp.to_uppercase().contains("VP8"),
      "offer must not advertise VP8"
    );
    peer.close().await;
  }

  #[tokio::test]
  async fn peer_generates_video_only_offer_when_audio_is_disabled() {
    let peer = Peer::with_bind(vec!["0.0.0.0:0".to_owned()], false)
      .await
      .expect("peer creation");
    assert!(peer.offer_sdp.contains("m=video"));
    assert!(
      !peer.offer_sdp.contains("m=audio"),
      "offer must not advertise an audio m-line"
    );
    assert!(
      !peer.offer_sdp.to_uppercase().contains("OPUS"),
      "offer must not advertise Opus"
    );
    peer.close().await;
  }

  #[tokio::test]
  async fn frames_before_negotiation_are_dropped() {
    let peer = Peer::new().await.expect("peer creation");
    let frame = EncodedFrame::new(
      bytes::Bytes::from_static(&[0, 0, 0, 1, 0x65]),
      true,
      0,
      std::time::Instant::now(),
    );
    // No answer applied → silently dropped, no error.
    peer
      .write_frame(&frame, Duration::from_millis(33))
      .await
      .expect("pre-negotiation write is a no-op");
    let audio = EncodedAudioFrame::new(
      bytes::Bytes::from_static(&[0xF8, 0x01]),
      0,
      std::time::Instant::now(),
    );
    peer
      .write_audio(&audio)
      .await
      .expect("pre-negotiation audio write is a no-op");
    peer.close().await;
  }
}
