/* Lumen viewer — plain ES2020, no dependencies.
 *
 * Protocol (JSON over WebSocket, tagged `type`):
 *   host → viewer: waiting | offer | iceCandidate | disconnected | error
 *   viewer → host: answer | iceCandidate
 */
(() => {
  "use strict";

  const stage = document.getElementById("stage");
  const video = document.getElementById("screen");
  const overlay = document.getElementById("overlay");
  const overlayIcon = document.getElementById("overlay-icon");
  const glyphErr = document.getElementById("overlay-glyph-err");
  const overlayTitle = document.getElementById("overlay-title");
  const overlayMessage = document.getElementById("overlay-message");
  const hud = document.getElementById("hud");
  const statusDot = document.getElementById("status-dot");
  const statusText = document.getElementById("status-text");
  const btnFullscreen = document.getElementById("btn-fullscreen");
  const btnReconnect = document.getElementById("btn-reconnect");
  const btnRetry = document.getElementById("btn-retry");
  const btnMute = document.getElementById("btn-mute");
  const iconMuted = document.getElementById("icon-muted");
  const iconUnmuted = document.getElementById("icon-unmuted");
  const iconFsEnter = document.getElementById("icon-fs-enter");
  const iconFsExit = document.getElementById("icon-fs-exit");
  const btnSettings = document.getElementById("btn-settings");
  const settings = document.getElementById("settings");
  const statsLine = document.getElementById("stats-line");

  // `hidden` is an IDL property only on HTMLElement — SVG icons must toggle
  // the attribute itself, or the markup state never clears.
  const setHidden = (el, on) => el.toggleAttribute("hidden", on);

  const token = (location.pathname.split("/").filter(Boolean)[1] || "").trim();

  /* ---------------- strings ---------------- */

  const STRINGS = {
    connecting: "Connecting",
    live: "Live",
    retrying: "Retrying",
    waitingHost: "Waiting for host",
    reconnecting: "Reconnecting",
    offline: "Offline",
    blocked: "Blocked",
    connectingMsg: "Connecting…",
    waitingTitle: "Almost there",
    waitingMsg: "Waiting for the host to accept this device…",
    negotiatingMsg: "Establishing secure stream…",
    lostMsg: "Lost connection to the host. Retrying…",
    failedMsg: "Connection failed. Retrying…",
    negotiationMsg: "Negotiation failed. Retrying…",
    waitingServerMsg: "Waiting for the host…",
    endedTitle: "Session ended",
    endedMsg: "The host disconnected this device.",
    blockedTitle: "Not allowed",
    blockedMsg: "The host refused this connection.",
    badLinkTitle: "Bad link",
    badLinkMsg: "This URL is missing its session token.",
    mute: "Mute",
    unmute: "Unmute",
    fullscreen: "Toggle fullscreen",
    exitFullscreen: "Exit fullscreen",
    reconnect: "Reconnect",
    retry: "Try again",
    settings: "Settings",
    statsNoData: "No stats yet.",
  };

  const t = (key) => STRINGS[key] || key;

  /* ---------------- settings ---------------- */

  const settingsState = {
    fit: localStorage.getItem("lumen.viewer.fit") === "fill" ? "fill" : "fit",
    mirror: localStorage.getItem("lumen.viewer.mirror") === "1",
    stats: localStorage.getItem("lumen.viewer.stats") === "1",
  };

  const seg = {
    fit: document.getElementById("btn-fit"),
    fill: document.getElementById("btn-fill"),
    mirrorOff: document.getElementById("btn-mirror-off"),
    mirrorOn: document.getElementById("btn-mirror-on"),
    statsOff: document.getElementById("btn-stats-off"),
    statsOn: document.getElementById("btn-stats-on"),
  };

  function press(btn, on) {
    btn.setAttribute("aria-pressed", on ? "true" : "false");
  }

  function applySettings() {
    video.classList.toggle("fill", settingsState.fit === "fill");
    video.classList.toggle("mirror", settingsState.mirror);
    press(seg.fit, settingsState.fit === "fit");
    press(seg.fill, settingsState.fit === "fill");
    press(seg.mirrorOff, !settingsState.mirror);
    press(seg.mirrorOn, settingsState.mirror);
    press(seg.statsOff, !settingsState.stats);
    press(seg.statsOn, settingsState.stats);
    if (!settingsState.stats) statsLine.hidden = true;
  }

  seg.fit.addEventListener("click", () => {
    settingsState.fit = "fit";
    localStorage.setItem("lumen.viewer.fit", "fit");
    applySettings();
  });
  seg.fill.addEventListener("click", () => {
    settingsState.fit = "fill";
    localStorage.setItem("lumen.viewer.fit", "fill");
    applySettings();
  });
  seg.mirrorOff.addEventListener("click", () => {
    settingsState.mirror = false;
    localStorage.setItem("lumen.viewer.mirror", "0");
    applySettings();
  });
  seg.statsOff.addEventListener("click", () => {
    settingsState.stats = false;
    localStorage.setItem("lumen.viewer.stats", "0");
    applySettings();
  });
  seg.statsOn.addEventListener("click", () => {
    settingsState.stats = true;
    localStorage.setItem("lumen.viewer.stats", "1");
    applySettings();
    pollStats();
  });

  /* ---------------- settings panel ---------------- */

  function openSettings(open) {
    settings.hidden = !open;
    btnSettings.classList.toggle("active", open);
    btnSettings.setAttribute("aria-pressed", open ? "true" : "false");
    pokeHud();
    if (open) pollStats();
  }

  btnSettings.addEventListener("click", (event) => {
    event.stopPropagation();
    openSettings(settings.hidden);
  });
  document.addEventListener("click", (event) => {
    if (!settings.hidden && !settings.contains(event.target)) openSettings(false);
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !settings.hidden) openSettings(false);
  });

  /* ---------------- UI state ---------------- */

  let currentStatus = { key: "connecting", kind: "" };
  let currentOverlay = null; // {titleKey, msgKey, mode, rawMessage}

  function renderStatus() {
    statusText.textContent = t(currentStatus.key);
    statusDot.className =
      currentStatus.kind === "live" ? "live" : currentStatus.kind === "bad" ? "bad" : "";
  }

  function setStatus(key, kind) {
    currentStatus = { key, kind };
    renderStatus();
  }

  function renderOverlay() {
    if (!currentOverlay) return;
    overlayTitle.textContent = t(currentOverlay.titleKey);
    overlayMessage.textContent = currentOverlay.rawMessage || t(currentOverlay.msgKey);
    const mode = currentOverlay.mode;
    overlayIcon.className = mode === "error" ? "error" : "";
    setHidden(glyphErr, mode !== "error");
  }

  function showOverlay(titleKey, msgKey, mode, rawMessage) {
    overlay.hidden = false;
    hud.hidden = true;
    video.classList.remove("live");
    currentOverlay = { titleKey, msgKey, mode, rawMessage };
    renderOverlay();
    // Terminal states are the only dead end; offer retry (never for a
    // malformed link — there is nothing to reconnect to).
    setHidden(btnRetry, !terminal || !token);
  }

  function setStatusOnly(key, kind) {
    currentOverlay = null;
    setStatus(key, kind);
  }

  let liveState = false;

  function showLive() {
    overlay.hidden = true;
    hud.hidden = false;
    currentOverlay = null;
    video.classList.add("live");
    setStatus("live", "live");
    liveState = true;
    acquireWakeLock();
    pokeHud();
  }

  let hudTimer = null;
  function pokeHud() {
    hud.classList.remove("faded");
    clearTimeout(hudTimer);
    hudTimer = setTimeout(() => hud.classList.add("faded"), 3500);
  }
  ["pointerdown", "pointermove", "keydown"].forEach((e) =>
    document.addEventListener(e, pokeHud, { passive: true }),
  );

  /* ---------------- viewport ---------------- */

  // Keep the stage exactly as tall as the visible viewport. `100%`/`100vh`
  // go stale across rotation and toolbar collapse on iOS Safari, so drive
  // the height from the live viewport on every resize/orientation change.
  function fitViewport() {
    stage.style.height = `${window.innerHeight}px`;
  }

  let viewportRaf = 0;
  function onViewportChange() {
    cancelAnimationFrame(viewportRaf);
    viewportRaf = requestAnimationFrame(fitViewport);
    pokeHud();
  }

  window.addEventListener("resize", onViewportChange);
  window.addEventListener("orientationchange", () => {
    onViewportChange();
    // iOS settles the layout viewport only after the rotation animation.
    setTimeout(fitViewport, 250);
    setTimeout(fitViewport, 600);
  });
  fitViewport();

  /* ---------------- wake lock ---------------- */

  let wakeLock = null;

  async function acquireWakeLock() {
    if (!("wakeLock" in navigator)) return;
    try {
      wakeLock = await navigator.wakeLock.request("screen");
      wakeLock.addEventListener("release", () => {
        wakeLock = null;
      });
    } catch {
      /* denied or page hidden */
    }
  }

  function releaseWakeLock() {
    liveState = false;
    if (wakeLock) {
      const lock = wakeLock;
      wakeLock = null;
      lock.release().catch(() => {});
    }
  }

  /* ---------------- WebRTC stats ---------------- */

  async function pollStats() {
    if (!settingsState.stats || settings.hidden || !pc) {
      statsLine.hidden = true;
      return;
    }
    try {
      const report = await pc.getStats();
      let fps = null;
      let width = null;
      let height = null;
      let rtt = null;
      report.forEach((entry) => {
        if (entry.type === "inbound-rtp" && entry.kind === "video") {
          if (typeof entry.framesPerSecond === "number") fps = entry.framesPerSecond;
          width = entry.frameWidth || width;
          height = entry.frameHeight || height;
        } else if (
          entry.type === "candidate-pair" &&
          entry.nominated &&
          typeof entry.currentRoundTripTime === "number"
        ) {
          rtt = entry.currentRoundTripTime;
        }
      });
      const parts = [];
      if (typeof fps === "number") parts.push(`${Math.round(fps)} fps`);
      if (width && height) parts.push(`${width}×${height}`);
      if (typeof rtt === "number") parts.push(`RTT ${Math.round(rtt * 1000)} ms`);
      statsLine.textContent = parts.length ? parts.join(" · ") : t("statsNoData");
      statsLine.hidden = false;
    } catch {
      statsLine.hidden = true;
    }
  }

  setInterval(pollStats, 2000);

  /* ---------------- session ---------------- */

  let ws = null;
  let pc = null;
  let remoteStream = null;
  let reconnectTimer = null;
  let reconnectDelay = 1000;
  let terminal = false; // session gone: invalid token, declined, host ended

  async function validateSession() {
    try {
      const res = await fetch(`/api/session/${encodeURIComponent(token)}`, {
        cache: "no-store",
      });
      return res.ok;
    } catch {
      return false; // network hiccup: treat as temporarily invalid, retry
    }
  }

  function wsUrl() {
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    return `${scheme}://${location.host}/ws/${encodeURIComponent(token)}`;
  }

  function connect() {
    if (terminal) return;
    clearTimeout(reconnectTimer);
    setStatus("connecting", "");
    showOverlay("Lumen", "connectingMsg", "spinner");

    validateSession().then((ok) => {
      if (terminal) return;
      if (!ok) {
        // Could be server restarting; keep retrying unless we know it is gone.
        scheduleReconnect("waitingServerMsg");
        return;
      }
      openSocket();
    });
  }

  function openSocket() {
    closeSignaling();
    ws = new WebSocket(wsUrl());
    ws.onmessage = (event) => {
      let msg;
      try {
        msg = JSON.parse(event.data);
      } catch {
        return;
      }
      handleHostMessage(msg);
    };
    ws.onclose = () => {
      if (!terminal) scheduleReconnect("lostMsg");
    };
    ws.onerror = () => {
      try {
        ws.close();
      } catch {
        /* already closed */
      }
    };
  }

  function send(obj) {
    if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(obj));
  }

  function scheduleReconnect(msgKey) {
    teardownPeer();
    setStatus("retrying", "bad");
    showOverlay("Lumen", msgKey, "spinner");
    clearTimeout(reconnectTimer);
    reconnectTimer = setTimeout(connect, reconnectDelay);
    reconnectDelay = Math.min(reconnectDelay * 2, 8000);
  }

  function closeSignaling() {
    if (ws) {
      ws.onclose = null;
      ws.onmessage = null;
      try {
        ws.close();
      } catch {
        /* noop */
      }
      ws = null;
    }
  }

  /* ---------------- WebRTC ---------------- */

  function teardownPeer() {
    releaseWakeLock();
    if (pc) {
      pc.ontrack = null;
      pc.onicecandidate = null;
      pc.onconnectionstatechange = null;
      try {
        pc.close();
      } catch {
        /* noop */
      }
      pc = null;
    }
    if (remoteStream) {
      remoteStream.getTracks().forEach((track) => track.stop());
      remoteStream = null;
      video.srcObject = null;
    }
    btnMute.hidden = true;
    video.muted = true; // next autoplay window starts muted again
    statsLine.hidden = true;
  }

  function handleHostMessage(msg) {
    switch (msg.type) {
      case "waiting":
        setStatus("waitingHost", "");
        showOverlay("waitingTitle", "waitingMsg", "spinner");
        return;
      case "offer":
        startPeer(msg.sdp);
        return;
      case "iceCandidate":
        if (pc && msg.candidate) {
          pc.addIceCandidate({
            candidate: msg.candidate,
            sdpMLineIndex: msg.sdpMLineIndex ?? 0,
          }).catch(() => {
            /* ignore late candidates */
          });
        }
        return;
      case "connected":
        reconnectDelay = 1000;
        setStatusOnly("live", "live");
        return;
      case "disconnected":
        // The host ended the session; the run's token is dead forever, so
        // stop auto-retrying and hand the choice to the viewer.
        terminal = true;
        teardownPeer();
        closeSignaling();
        setStatusOnly("offline", "bad");
        showOverlay("endedTitle", "endedMsg", "error");
        return;
      case "error":
        terminal = true;
        teardownPeer();
        closeSignaling();
        setStatusOnly("blocked", "bad");
        showOverlay("blockedTitle", "blockedMsg", "error", msg.message);
        return;
      default:
        return;
    }
  }

  async function startPeer(offerSdp) {
    teardownPeer();
    pc = new RTCPeerConnection({ iceServers: [] }); // LAN-only: host candidates suffice

    pc.ontrack = (event) => {
      remoteStream = event.streams[0] || new MediaStream([event.track]);
      video.srcObject = remoteStream;
      if (event.track.kind === "audio") {
        btnMute.hidden = false; // autoplay starts muted; let the viewer unmute
        syncMuteButton();
      }
      video.play().catch(() => {
        /* autoplay is allowed: muted + playsinline */
      });
    };

    pc.onicecandidate = (event) => {
      if (event.candidate) {
        send({
          type: "iceCandidate",
          candidate: event.candidate.candidate,
          sdpMLineIndex: event.candidate.sdpMLineIndex ?? 0,
        });
      }
    };

    pc.onconnectionstatechange = () => {
      if (!pc) return;
      switch (pc.connectionState) {
        case "connected":
          reconnectDelay = 1000;
          showLive();
          break;
        case "connecting":
          setStatusOnly("connecting", "");
          break;
        case "disconnected":
          setStatusOnly("reconnecting", "bad");
          break;
        case "failed":
        case "closed":
          setStatusOnly("reconnecting", "bad");
          scheduleReconnect("failedMsg");
          break;
        default:
          break;
      }
    };

    try {
      await pc.setRemoteDescription({ type: "offer", sdp: offerSdp });
      const answer = await pc.createAnswer();
      await pc.setLocalDescription(answer);
      send({ type: "answer", sdp: pc.localDescription.sdp });
      setStatus("connecting", "");
      showOverlay("Lumen", "negotiatingMsg", "spinner");
    } catch (err) {
      console.error("WebRTC negotiation failed", err);
      scheduleReconnect("negotiationMsg");
    }
  }

  /* ---------------- controls ---------------- */

  function fullscreenTarget() {
    return (
      document.fullscreenElement ||
      document.webkitFullscreenElement ||
      (video.webkitDisplayingFullscreen ? video : null)
    );
  }

  function syncFullscreenButton() {
    const on = Boolean(fullscreenTarget());
    btnFullscreen.classList.toggle("active", on);
    btnFullscreen.setAttribute("aria-pressed", on ? "true" : "false");
    btnFullscreen.setAttribute("aria-label", on ? t("exitFullscreen") : t("fullscreen"));
    setHidden(iconFsEnter, on);
    setHidden(iconFsExit, !on);
    pokeHud();
  }

  function toggleFullscreen() {
    if (fullscreenTarget()) {
      if (video.webkitDisplayingFullscreen) {
        const exitLegacy = video.webkitExitFullscreen || video.webkitExitFullScreen;
        if (exitLegacy) exitLegacy.call(video);
        return;
      }
      const exit = document.exitFullscreen || document.webkitExitFullscreen;
      if (exit) {
        const result = exit.call(document);
        if (result && result.catch) result.catch(() => {});
      }
      return;
    }
    // Standard / WebKit element fullscreen: desktop, Android Chrome, iPadOS.
    for (const el of [stage, video]) {
      const req = el.requestFullscreen || el.webkitRequestFullscreen;
      if (req) {
        const promise = req.call(el);
        if (promise && promise.catch) promise.catch(() => {});
        return;
      }
    }
    // iPhone Safari: only <video> can go fullscreen, via its legacy API.
    const enter = video.webkitEnterFullscreen || video.webkitEnterFullScreen;
    if (enter) {
      try {
        enter.call(video);
      } catch {
        /* not ready yet */
      }
    }
  }

  ["fullscreenchange", "webkitfullscreenchange"].forEach((type) =>
    document.addEventListener(type, syncFullscreenButton),
  );
  // iOS fires the legacy pair on the video element instead.
  ["webkitbeginfullscreen", "webkitendfullscreen"].forEach((type) =>
    video.addEventListener(type, syncFullscreenButton),
  );

  btnFullscreen.addEventListener("click", toggleFullscreen);

  function syncMuteButton() {
    const muted = video.muted;
    setHidden(iconMuted, !muted);
    setHidden(iconUnmuted, muted);
    btnMute.setAttribute("aria-label", muted ? t("unmute") : t("mute"));
    btnMute.setAttribute("aria-pressed", muted ? "false" : "true");
  }

  btnMute.addEventListener("click", () => {
    video.muted = !video.muted;
    // Safari may pause when the muted flag flips mid-playback; nudge it.
    const resumed = video.play();
    if (resumed && resumed.catch) resumed.catch(() => {});
    syncMuteButton();
    pokeHud();
  });

  function retryNow() {
    reconnectDelay = 1000;
    terminal = false;
    connect();
  }
  btnReconnect.addEventListener("click", retryNow);
  btnRetry.addEventListener("click", retryNow);

  // Double-click / double-tap toggles fullscreen.
  let lastTap = 0;
  video.addEventListener("click", () => {
    const now = Date.now();
    if (now - lastTap < 320) toggleFullscreen();
    lastTap = now;
  });

  // Resume signaling when the tab becomes visible again after a hard drop,
  // and re-arm the wake lock the browser dropped with the visibility change.
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState !== "visible") return;
    if (!terminal && !ws) connect();
    if (liveState) acquireWakeLock();
  });

  /* ---------------- boot ---------------- */

  applySettings();
  btnSettings.setAttribute("aria-label", t("settings"));
  btnReconnect.setAttribute("aria-label", t("reconnect"));

  if (!token) {
    terminal = true;
    showOverlay("badLinkTitle", "badLinkMsg", "error");
  } else {
    connect();
  }
})();
