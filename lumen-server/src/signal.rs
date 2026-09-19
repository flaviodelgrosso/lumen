//! Typed WebSocket signaling protocol.
//!
//! Tagged JSON via serde — never hand-built. The `type` tag uses
//! lowerCamelCase (`offer`, `answer`, `iceCandidate`, …).

use serde::{Deserialize, Serialize};

/// A host → viewer signaling message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum HostMessage {
  /// WebRTC connectivity established; media is flowing.
  Connected,
  /// WebRTC SDP offer from the host.
  Offer {
    /// SDP offer text.
    sdp: String,
  },
  /// Host ICE candidate.
  IceCandidate {
    /// Candidate string.
    candidate: String,
    /// SDP media line index, when present.
    #[serde(
      rename = "sdpMLineIndex",
      default,
      skip_serializing_if = "Option::is_none"
    )]
    sdp_m_line_index: Option<u16>,
  },
  /// Viewer was explicitly disconnected by the host.
  Disconnected,
  /// Terminal error; the viewer should stop.
  Error {
    /// Human-readable reason.
    message: String,
  },
}

/// A viewer → host signaling message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ViewerMessage {
  /// WebRTC SDP answer from the browser.
  Answer {
    /// SDP answer text.
    sdp: String,
  },
  /// Browser ICE candidate.
  IceCandidate {
    /// Candidate string.
    candidate: String,
    /// SDP media line index, when present.
    #[serde(
      rename = "sdpMLineIndex",
      default,
      skip_serializing_if = "Option::is_none"
    )]
    sdp_m_line_index: Option<u16>,
  },
}

/// Combined protocol message (both directions share the tag space).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SignalMessage {
  /// Message sent by the host.
  Host(HostMessage),
  /// Message sent by the viewer.
  Viewer(ViewerMessage),
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn offer_serializes_with_type_tag() {
    let msg = HostMessage::Offer {
      sdp: "v=0\r\n".to_owned(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"offer\""));
    assert!(json.contains("\"sdp\":\"v=0\\r\\n\""));
  }

  #[test]
  fn answer_roundtrips() {
    let msg = ViewerMessage::Answer {
      sdp: "v=0\r\nx".to_owned(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let back: ViewerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(back, msg);
  }

  #[test]
  fn ice_candidate_omits_absent_line_index() {
    let msg = HostMessage::IceCandidate {
      candidate: "candidate:1 1 udp 1 10.0.0.1 50000 typ host".to_owned(),
      sdp_m_line_index: None,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(!json.contains("sdpMLineIndex"));
    let back: HostMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(back, msg);
  }

  #[test]
  fn ice_candidate_with_line_index_roundtrips() {
    let msg = ViewerMessage::IceCandidate {
      candidate: "candidate:2".to_owned(),
      sdp_m_line_index: Some(0),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"sdpMLineIndex\":0"));
    let back: ViewerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(back, msg);
  }

  #[test]
  fn error_tag_roundtrips() {
    let err = HostMessage::Error {
      message: "declined".to_owned(),
    };
    let json = serde_json::to_string(&err).unwrap();
    assert!(json.contains("\"type\":\"error\""));
    let back: HostMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(back, err);
  }

  #[test]
  fn host_cannot_forgive_viewer_answer_and_vice_versa() {
    // Directional enums: a host message JSON must not parse as a viewer
    // message and vice versa, so the server can reject misrouted frames.
    let host = serde_json::to_string(&HostMessage::Connected).unwrap();
    assert!(serde_json::from_str::<ViewerMessage>(&host).is_err());
    let viewer = serde_json::to_string(&ViewerMessage::Answer {
      sdp: "x".to_owned(),
    })
    .unwrap();
    assert!(serde_json::from_str::<HostMessage>(&viewer).is_err());
  }

  #[test]
  fn unknown_type_is_rejected() {
    assert!(serde_json::from_str::<HostMessage>("{\"type\":\"pwn\"}").is_err());
  }
}
