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
  const overlayTitle = document.getElementById("overlay-title");
  const overlayMessage = document.getElementById("overlay-message");
  const hud = document.getElementById("hud");
  const statusDot = document.getElementById("status-dot");
  const statusText = document.getElementById("status-text");
  const btnFullscreen = document.getElementById("btn-fullscreen");
  const btnReconnect = document.getElementById("btn-reconnect");
  const btnMute = document.getElementById("btn-mute");

  const token = (location.pathname.split("/").filter(Boolean)[1] || "").trim();

  let ws = null;
  let pc = null;
  let remoteStream = null;
  let reconnectTimer = null;
  let reconnectDelay = 1000;
  let terminal = false; // session gone: invalid token, declined, host ended

  /* ---------------- UI helpers ---------------- */

  function showOverlay(title, message, mode) {
    overlay.hidden = false;
    hud.hidden = true;
    video.classList.remove("live");
    overlayTitle.textContent = title;
    overlayMessage.textContent = message;
    overlayIcon.className = mode === "spinner" ? "" : mode === "error" ? "error" : "done";
    overlayIcon.textContent = mode === "spinner" ? "" : mode === "error" ? "✕" : "✓";
  }

  function setStatus(text, kind) {
    statusText.textContent = text;
    statusDot.className = kind === "live" ? "live" : kind === "bad" ? "bad" : "";
  }

  function showLive() {
    overlay.hidden = true;
    hud.hidden = false;
    video.classList.add("live");
    setStatus("Live", "live");
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

  /* ---------------- session ---------------- */

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
    setStatus("Connecting", "");
    showOverlay("Lumen", "Connecting…", "spinner");

    validateSession().then((ok) => {
      if (terminal) return;
      if (!ok) {
        // Could be server restarting; keep retrying unless we know it is gone.
        scheduleReconnect("Waiting for the host…");
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
      if (!terminal) scheduleReconnect("Lost connection to the host. Retrying…");
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

  function scheduleReconnect(message) {
    teardownPeer();
    setStatus("Retrying", "bad");
    if (!overlay.hidden) showOverlay("Lumen", message, "spinner");
    overlay.hidden = false;
    overlayIcon.className = "";
    overlayIcon.textContent = "";
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
      remoteStream.getTracks().forEach((t) => t.stop());
      remoteStream = null;
      video.srcObject = null;
    }
    btnMute.hidden = true;
    video.muted = true; // next autoplay window starts muted again
  }

  function handleHostMessage(msg) {
    switch (msg.type) {
      case "waiting":
        setStatus("Waiting for host", "");
        showOverlay("Almost there", "Waiting for the host to accept this device…", "spinner");
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
        setStatus("Live", "live");
        return;
      case "disconnected":
        teardownPeer();
        closeSignaling();
        setStatus("Offline", "bad");
        showOverlay("Session ended", "The host disconnected this device.", "error");
        return;
      case "error":
        terminal = true;
        teardownPeer();
        closeSignaling();
        setStatus("Blocked", "bad");
        showOverlay("Not allowed", msg.message || "The host refused this connection.", "error");
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
          setStatus("Connecting", "");
          break;
        case "disconnected":
          setStatus("Reconnecting", "bad");
          break;
        case "failed":
        case "closed":
          setStatus("Reconnecting", "bad");
          scheduleReconnect("Connection failed. Retrying…");
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
      setStatus("Connecting", "");
      showOverlay("Lumen", "Establishing secure stream…", "spinner");
    } catch (err) {
      console.error("WebRTC negotiation failed", err);
      scheduleReconnect("Negotiation failed. Retrying…");
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
    btnFullscreen.setAttribute("aria-label", on ? "Exit fullscreen" : "Toggle fullscreen");
    pokeHud();
  }

  function toggleFullscreen() {
    if (fullscreenTarget()) {
      if (video.webkitDisplayingFullscreen) {
        const exit = video.webkitExitFullscreen || video.webkitExitFullScreen;
        if (exit) exit.call(video);
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
  syncFullscreenButton();

  btnFullscreen.addEventListener("click", toggleFullscreen);

  function syncMuteButton() {
    btnMute.textContent = video.muted ? "🔇" : "🔊";
    btnMute.setAttribute("aria-label", video.muted ? "Unmute" : "Mute");
    btnMute.setAttribute("aria-pressed", video.muted ? "false" : "true");
  }

  btnMute.addEventListener("click", () => {
    video.muted = !video.muted;
    // Safari may pause when the muted flag flips mid-playback; nudge it.
    const resumed = video.play();
    if (resumed && resumed.catch) resumed.catch(() => {});
    syncMuteButton();
    pokeHud();
  });
  btnReconnect.addEventListener("click", () => {
    reconnectDelay = 1000;
    connect();
  });

  // Double-click / double-tap toggles fullscreen.
  let lastTap = 0;
  video.addEventListener("click", () => {
    const now = Date.now();
    if (now - lastTap < 320) toggleFullscreen();
    lastTap = now;
  });

  // Resume signaling when the tab becomes visible again after a hard drop.
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible" && !terminal && !ws) connect();
  });

  if (!token) {
    terminal = true;
    showOverlay("Bad link", "This URL is missing its session token.", "error");
  } else {
    connect();
  }
})();
