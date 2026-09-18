/// Human-friendly browser/device description for terminal prompts.
///
/// Pure string classification — no external crate needed for this tiny
/// surface (the `ua-parser` ecosystem is heavyweight for a one-line label).
#[must_use]
pub fn describe_user_agent(ua: Option<&str>) -> String {
  let Some(ua) = ua else {
    return "Unknown browser".to_owned();
  };
  let browser = if ua.contains("Edg/") {
    "Edge"
  } else if ua.contains("OPR/") || ua.contains("Opera") {
    "Opera"
  } else if ua.contains("Firefox/") || ua.contains("FxiOS") {
    "Firefox"
  } else if ua.contains("CriOS/") || ua.contains("Chrome/") || ua.contains("Chromium/") {
    "Chrome"
  } else if ua.contains("Safari/") {
    "Safari"
  } else {
    "Unknown browser"
  };
  let device = if ua.contains("iPad") || (ua.contains("Macintosh") && ua.contains("Mobile")) {
    "iPad"
  } else if ua.contains("iPhone") || ua.contains("iPod") {
    "iPhone"
  } else if ua.contains("Android") {
    "Android"
  } else if ua.contains("Macintosh") || ua.contains("Mac OS X") {
    "macOS"
  } else if ua.contains("Windows") {
    "Windows"
  } else if ua.contains("Linux") || ua.contains("X11") {
    "Linux"
  } else if ua.contains("SmartTV") || ua.contains("Tizen") || ua.contains("Web0S") {
    "Smart TV"
  } else {
    "Unknown device"
  };
  format!("{browser} / {device}")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn safari_ipad() {
    let ua = "Mozilla/5.0 (iPad; CPU OS 17_0 like Mac OS X) AppleWebKit/605.1.15 \
                  (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";
    assert_eq!(describe_user_agent(Some(ua)), "Safari / iPad");
  }

  #[test]
  fn chrome_iphone_is_chrome() {
    let ua = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 \
                  (KHTML, like Gecko) CriOS/120.0 Mobile/15E148 Safari/604.1";
    assert_eq!(describe_user_agent(Some(ua)), "Chrome / iPhone");
  }

  #[test]
  fn chrome_android() {
    let ua = "Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/120.0 Mobile Safari/537.36";
    assert_eq!(describe_user_agent(Some(ua)), "Chrome / Android");
  }

  #[test]
  fn desktop_browsers() {
    let mac_safari = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 \
                          (KHTML, like Gecko) Version/17.0 Safari/605.1.15";
    assert_eq!(describe_user_agent(Some(mac_safari)), "Safari / macOS");
    let win_edge = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                        (KHTML, like Gecko) Chrome/120.0 Safari/537.36 Edg/120.0";
    assert_eq!(describe_user_agent(Some(win_edge)), "Edge / Windows");
    let linux_firefox = "Mozilla/5.0 (X11; Linux x86_64; rv:120.0) Gecko/20100101 Firefox/120.0";
    assert_eq!(describe_user_agent(Some(linux_firefox)), "Firefox / Linux");
  }

  #[test]
  fn unknown_ua() {
    assert_eq!(describe_user_agent(None), "Unknown browser");
    assert_eq!(
      describe_user_agent(Some("curl/8.0")),
      "Unknown browser / Unknown device"
    );
  }
}
