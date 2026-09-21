/* Lumen viewer — plain ES2020, no dependencies.
 *
 * Entry: POST /api/join (no secret), poll GET /api/join/<id> until the
 * host approves, then open the signaling socket with the ephemeral grant.
 *
 * Protocol (JSON over WebSocket, tagged `type`):
 *   host → viewer: connected | offer | iceCandidate | disconnected | error
 *   viewer → host: answer | iceCandidate
 */
(() => {
  "use strict";

  const stage = document.getElementById("stage");
  const video = document.getElementById("screen");
  const audioEl = document.getElementById("screen-audio");
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
  const pairingForm = document.getElementById("pairing-form");
  const pairingInput = document.getElementById("pairing-code");

  // `hidden` is an IDL property only on HTMLElement — SVG icons must toggle
  // the attribute itself, or the markup state never clears.
  const setHidden = (el, on) => el.toggleAttribute("hidden", on);



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
    unsupportedTitle: "Not supported",
    unsupportedMsg: "This browser cannot stream: WebRTC is unavailable.",
    busyMsg: "The host is busy with too many devices. Retrying…",
    mute: "Mute",
    unmute: "Unmute",
    fullscreen: "Toggle fullscreen",
    exitFullscreen: "Exit fullscreen",
    reconnect: "Reconnect",
    retry: "Try again",
    settings: "Settings",
    statsNoData: "No stats yet.",
    pairingTitle: "Pair this device",
    pairingMsg: "Enter the code shown on the host.",
    pairingInvalid: "The pairing code was not accepted. Check the code and try again.",
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
    if (!settings.hidden && !settings.contains(event.target))
      openSettings(false);
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
      currentStatus.kind === "live"
        ? "live"
        : currentStatus.kind === "bad"
          ? "bad"
          : "";
  }

  function setStatus(key, kind) {
    currentStatus = { key, kind };
    renderStatus();
  }

  function renderOverlay() {
    if (!currentOverlay) return;
    // The session name replaces the generic title while the viewer is
    // still waiting for the host; error states keep their own title so
    // the status stays legible. textContent keeps the host-provided name
    // inert display text, never markup.
    overlayTitle.textContent =
      sessionName && currentOverlay.mode !== "error"
        ? sessionName
        : t(currentOverlay.titleKey);
    overlayMessage.textContent =
      currentOverlay.rawMessage || t(currentOverlay.msgKey);
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
    // Terminal states are the only dead end; offer retry — a fresh
    // join is always possible, the host decides again.
    setHidden(btnRetry, !terminal);
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

  let resizeRecoverTimer = 0;
  window.addEventListener("resize", () => {
    onViewportChange();
    // Maximize/restore and window drags fire resize bursts; recover once
    // the layout has settled instead of hammering play() on every pixel.
    clearTimeout(resizeRecoverTimer);
    resizeRecoverTimer = setTimeout(schedulePlaybackRecovery, 300);
  });
  window.addEventListener("orientationchange", () => {
    onViewportChange();
    schedulePlaybackRecovery();
    // iOS settles the layout viewport only after the rotation animation.
    setTimeout(fitViewport, 250);
    setTimeout(fitViewport, 600);
  });
  fitViewport();

  /* ---------------- playback recovery ---------------- */

  // iOS owns the fullscreen <video>: the native player takes over on
  // enter, and on exit WebKit fires `webkitendfullscreen` BEFORE its
  // restore animation finishes, then pauses the element once the inline
  // hand-back completes (WebKit deliberately pauses on exiting native
  // fullscreen). A play() issued inside the exit handler therefore runs
  // mid-transition and is swallowed: the element reports playing, the OS
  // pauses it afterwards, and nothing reacted to that late pause — the
  // freeze that only ended when the next tap called recoverPlayback.
  // The connection, tracks and audio never failed.
  //
  // The viewer stays split in two: this video element carries the video
  // track ONLY and is permanently muted, so programmatic play() is
  // always allowed; audio rides `audioEl`, which never enters fullscreen
  // and keeps playing across the round-trip.
  //
  // Recovery is keyed off the real media lifecycle instead of fullscreen
  // timers:
  //  - Exiting/entering presentation mode opens a short recovery window
  //    that is re-armed on every pause. Inside it, the `pause` event
  //    itself is the trigger: the element is answered with play() the
  //    moment the OS pauses it. A settle check re-answers a play() that
  //    the still-settling transition swallowed without pausing the
  //    element a second time. Repeated pauses escalate once to a
  //    pipeline rebuild, then stop chasing.
  //  - The window is scoped to the presentation transition and closes on
  //    its own, so any later pause remains the app's business.
  //  - A frozen render layer (element claims playing, no frames) is
  //    detected by the frame probe and rebuilt with reattachStream().

  // Field diagnostics: `localStorage.setItem("lumen.viewer.debug", "1")`
  // logs the full media lifecycle — fullscreen/presentation transitions,
  // play/pause/waiting/stalled/suspend/loadedmetadata, rejected play()
  // promises and rVFC frame ticks — for on-device tracing.
  const debugMedia = localStorage.getItem("lumen.viewer.debug") === "1";

  function debugLog(label, el) {
    if (!debugMedia) return;
    console.log(
      `lumen: ${label} paused=${el.paused} ready=${el.readyState} net=${el.networkState} t=${el.currentTime.toFixed(2)}`,
    );
  }

  function liveVideoTrack() {
    const track = videoStream && videoStream.getVideoTracks()[0];
    return track && track.readyState === "live" ? track : null;
  }

  // Attempt playback and surface the rejection reason instead of
  // swallowing it: a refused play() (autoplay policy) is a different
  // failure from a frozen render layer, and hiding that distinction is
  // what made the fullscreen freeze undebuggable.
  function playMedia(el) {
    const resumed = el.play();
    if (resumed && resumed.catch) {
      resumed.catch((err) => {
        console.warn(
          `lumen: play() refused on #${el.id}:`,
          err && `${err.name}: ${err.message}`,
        );
      });
    }
  }

  // Idempotent: no-op unless the document is visible and a live remote
  // video track is attached; play() on a running element is itself a
  // no-op. The audio element is nudged along whenever it is attached.
  function recoverPlayback() {
    if (document.visibilityState !== "visible" || !liveVideoTrack()) return;
    playMedia(video);
    const audioTrack =
      audioEl.srcObject && audioEl.srcObject.getAudioTracks()[0];
    if (audioTrack && audioTrack.readyState === "live") playMedia(audioEl);
  }

  // Rebuild the element's media pipeline from the still-live stream: the
  // manual equivalent of what re-entering fullscreen does. The WebRTC
  // track itself is never touched, so the connection stays intact.
  function reattachStream() {
    if (document.visibilityState !== "visible" || !liveVideoTrack()) return;
    video.srcObject = null;
    video.srcObject = videoStream;
    playMedia(video);
  }

  // Detect a frozen render layer: the host pipeline encodes continuously,
  // so no decoded frame within the probe window means the element's media
  // pipeline stalled regardless of what `paused` claims. Rebuild is
  // bounded and re-probed, so a pathological stream cannot loop here.
  let frameProbeTimer = 0;
  let frameReattachTries = 0;
  function probeFrameProgress() {
    if (!("requestVideoFrameCallback" in video) || !liveVideoTrack()) return;
    let advanced = false;
    video.requestVideoFrameCallback(() => {
      advanced = true;
      if (debugMedia) debugLog("video rVFC frame", video);
    });
    clearTimeout(frameProbeTimer);
    frameProbeTimer = setTimeout(() => {
      if (advanced) {
        frameReattachTries = 0;
        return;
      }
      // A paused element is not a frozen render layer: rebuilding it
      // would auto-resume a legitimate pause. Only the presentation
      // window resumes pauses; the probe rebuilds a claimed-playing
      // pipeline that is emitting no frames.
      if (video.paused) return;
      if (frameReattachTries >= 2) return; // pathological: stop hammering
      frameReattachTries++;
      reattachStream();
      probeFrameProgress();
    }, 600);
  }

  // Presentation recovery window: opened by fullscreen/presentation
  // transitions, closed when playback settles or the bounded escalation
  // is exhausted. `pause` events outside it are never auto-resumed.
  let presentationRecovery = null; // { resumes, timer }

  function endPresentationRecovery() {
    if (!presentationRecovery) return;
    clearTimeout(presentationRecovery.timer);
    presentationRecovery = null;
  }

  function armPresentationWindow(state, delay) {
    clearTimeout(state.timer);
    state.timer = setTimeout(() => {
      if (presentationRecovery !== state) return;
      if (video.paused) {
        // A play() swallowed by the still-settling transition never
        // produces a second pause event; the settle check re-answers it.
        presentationResume();
      } else {
        endPresentationRecovery();
        probeFrameProgress();
      }
    }, delay);
  }

  function presentationResume() {
    const state = presentationRecovery;
    if (!state || !liveVideoTrack()) return;
    if (state.resumes >= 4) {
      // The OS keeps re-pausing through play() and rebuild alike: stop
      // chasing; the gesture safety net remains.
      endPresentationRecovery();
      return;
    }
    state.resumes += 1;
    if (state.resumes >= 3) {
      // Plain play() keeps getting re-paused: rebuild the render
      // pipeline from the still-live stream (never touches the peer).
      reattachStream();
    } else {
      playMedia(video);
    }
    armPresentationWindow(state, 1500);
  }

  function beginPresentationRecovery() {
    if (document.visibilityState !== "visible" || !liveVideoTrack()) return;
    endPresentationRecovery();
    presentationRecovery = { resumes: 0, timer: 0 };
    frameReattachTries = 0;
    recoverPlayback();
    armPresentationWindow(presentationRecovery, 1500);
  }

  // The OS pause that follows a fullscreen exit lands AFTER the exit
  // event has fired; inside the scoped window the pause event itself is
  // the reliable "hand-back complete" signal — answer it immediately.
  video.addEventListener("pause", () => {
    if (presentationRecovery) presentationResume();
  });

  // Non-fullscreen paths (rotation, tab return, layout settle): nudge and
  // verify frames; the presentation window owns fullscreen round-trips.
  function schedulePlaybackRecovery() {
    if (document.visibilityState !== "visible" || !liveVideoTrack()) return;
    recoverPlayback();
    probeFrameProgress();
  }

  // Safety net, not the mechanism: with the video element permanently
  // muted recovery is deterministic, but any later gesture still re-arms
  // the idempotent resume for whatever edge case paused the OS pipeline.
  document.addEventListener("pointerdown", recoverPlayback, {
    passive: true,
  });

  if (debugMedia) {
    ["play", "playing", "pause", "waiting", "stalled", "suspend", "loadedmetadata", "webkitbeginfullscreen", "webkitendfullscreen"].forEach(
      (type) =>
        video.addEventListener(type, () => debugLog(`video ${type}`, video)),
    );
    ["fullscreenchange", "webkitfullscreenchange"].forEach((type) =>
      document.addEventListener(type, () =>
        debugLog(`document ${type}`, video),
      ),
    );
  }

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
          if (typeof entry.framesPerSecond === "number")
            fps = entry.framesPerSecond;
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
      if (typeof rtt === "number")
        parts.push(`RTT ${Math.round(rtt * 1000)} ms`);
      statsLine.textContent = parts.length
        ? parts.join(" · ")
        : t("statsNoData");
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
  let videoStream = null; // video-track-only view; what the <video> plays
  let reconnectTimer = null;
  let reconnectDelay = 1000;
  let joinTimer = null;
  let joinId = null;
  let joinToken = null;
  let pairingRequired = false;
  let sessionName = null; // public display metadata from /api/join/config
  let pairingCode = null;
  let terminal = false; // gone: denied, host ended, unsupported browser

  // Join flow: POST /api/join (no secret, no URL token) → poll status →
  // the host approves → the server hands out an ephemeral single-use
  // grant → the signaling socket opens with it. A dropped socket means a
  // fresh join (the host is asked again), matching the per-connection
  // approval semantics of the tokenized flow this replaces.
  function showPairing(messageKey = "pairingMsg") {
    clearTimeout(reconnectTimer);
    clearTimeout(joinTimer);
    closeSignaling();
    pairingForm.hidden = false;
    overlay.hidden = false;
    hud.hidden = true;
    setHidden(overlayIcon, true);
    currentOverlay = {
      titleKey: "pairingTitle",
      msgKey: messageKey,
      mode: "spinner",
    };
    renderOverlay();
    pairingInput.focus();
  }

  // Join flow: POST /api/join → poll with its per-request JoinToken → host
  // approval → ephemeral ViewerGrant → signaling socket. The pairing code is
  // sent only in the POST body for unattended runs and never enters a URL.
  async function connect() {
    if (terminal) return;
    if (pairingRequired && !pairingCode) {
      showPairing();
      return;
    }
    clearTimeout(reconnectTimer);
    clearTimeout(joinTimer);
    joinId = null;
    joinToken = null;
    pairingForm.hidden = true;
    setHidden(overlayIcon, false);
    setStatus("connecting", "");
    showOverlay("Lumen", "connectingMsg", "spinner");
    try {
      const init = { method: "POST", cache: "no-store" };
      if (pairingRequired) {
        init.headers = { "Content-Type": "application/json" };
        init.body = JSON.stringify({ pairingCode });
      }
      const res = await fetch("/api/join", init);
      if (terminal) return;
      if (res.status === 403 || (res.status === 429 && pairingRequired)) {
        pairingCode = null;
        showPairing("pairingInvalid");
        return;
      }
      if (res.status === 429) {
        scheduleReconnect("busyMsg");
        return;
      }
      if (!res.ok) {
        scheduleReconnect("waitingServerMsg");
        return;
      }
      const data = await res.json();
      if (terminal) return;
      if (!data || !data.requestId || !data.joinToken) {
        scheduleReconnect("failedMsg");
        return;
      }
      joinId = data.requestId;
      joinToken = data.joinToken;
      pollJoin();
    } catch {
      if (!terminal) scheduleReconnect("failedMsg");
    }
  }

  function pollJoin() {
    setStatus("waitingHost", "");
    showOverlay("waitingTitle", "waitingMsg", "spinner");
    const step = async () => {
      if (terminal || !joinId || !joinToken) return;
      let data = null;
      let gone = false;
      let lost = false;
      try {
        const res = await fetch(`/api/join/${encodeURIComponent(joinId)}`, {
          cache: "no-store",
          headers: { Authorization: `Bearer ${joinToken}` },
        });
        if (res.status === 404) gone = true;
        else if (!res.ok) lost = true;
        else data = await res.json();
      } catch {
        lost = true;
      }
      if (terminal) return;
      if (gone) {
        // The request expired (host restarted, abandoned poll): start over.
        scheduleReconnect("waitingServerMsg");
        return;
      }
      if (lost) {
        scheduleReconnect("lostMsg");
        return;
      }
      switch (data.status) {
        case "approved":
          openSocket(data.grant);
          return;
        case "denied":
          terminal = true;
          teardownPeer();
          closeSignaling();
          setStatusOnly("blocked", "bad");
          showOverlay("blockedTitle", "blockedMsg", "error");
          return;
        default:
          joinTimer = setTimeout(step, 1000);
      }
    };
    step();
  }

  function openSocket(grant) {
    closeSignaling();
    setStatus("connecting", "");
    showOverlay("Lumen", "negotiatingMsg", "spinner");
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    ws = new WebSocket(
      `${scheme}://${location.host}/ws/${encodeURIComponent(grant)}`,
    );
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
    endPresentationRecovery();
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
      videoStream = null;
      audioEl.srcObject = null;
    }
    btnMute.hidden = true;
    video.muted = true; // the fullscreen element stays permanently muted
    audioEl.muted = true; // next autoplay window starts muted again
    statsLine.hidden = true;
  }

  function handleHostMessage(msg) {
    switch (msg.type) {
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
        // The host ended the session; stop auto-retrying and hand the
        // choice (rejoin from scratch) to the viewer.
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
      if (event.track.kind === "video") {
        // Video-only on the fullscreen element: an audible audio track
        // here is what makes iOS refuse play() after the native
        // fullscreen exit until a fresh user gesture arrives.
        videoStream = new MediaStream([event.track]);
        video.srcObject = videoStream;
        video.muted = true; // permanently: audio output is audioEl's job
        playMedia(video);
      } else {
        // Audio on its own element, which never enters fullscreen and so
        // never loses its playback allowance mid-session. It starts muted
        // (autoplay policy); the mute button unmutes it inside a gesture.
        audioEl.srcObject = new MediaStream([event.track]);
        audioEl.muted = true;
        btnMute.hidden = false; // autoplay starts muted; let the viewer unmute
        syncMuteButton();
        playMedia(audioEl);
      }
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
    btnFullscreen.setAttribute(
      "aria-label",
      on ? t("exitFullscreen") : t("fullscreen"),
    );
    setHidden(iconFsEnter, on);
    setHidden(iconFsExit, !on);
    pokeHud();
  }

  function toggleFullscreen() {
    if (fullscreenTarget()) {
      if (video.webkitDisplayingFullscreen) {
        const exitLegacy =
          video.webkitExitFullscreen || video.webkitExitFullScreen;
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

  // Fullscreen transitions also re-fit the viewport (iOS may not fire
  // resize when the inline player comes back) and open the scoped
  // presentation-recovery window: iOS pauses the element asynchronously
  // as it hands playback back from the native player.
  function onFullscreenTransition() {
    syncFullscreenButton();
    onViewportChange();
    beginPresentationRecovery();
  }

  ["fullscreenchange", "webkitfullscreenchange"].forEach((type) =>
    document.addEventListener(type, onFullscreenTransition),
  );
  // iOS fires the legacy pair on the video element instead.

  pairingInput.addEventListener("input", () => {
    pairingInput.value = pairingInput.value.replace(/\D/g, "").slice(0, 6);
  });
  pairingForm.addEventListener("submit", (event) => {
    event.preventDefault();
    const code = pairingInput.value.replace(/\D/g, "");
    if (code.length !== 6) {
      pairingInput.focus();
      return;
    }
    pairingCode = code;
    connect();
  });
  ["webkitbeginfullscreen", "webkitendfullscreen"].forEach((type) =>
    video.addEventListener(type, onFullscreenTransition),
  );
  // iOS also reports the native player's mode on the element; `inline`
  // is the cleanest "hand-back to the page complete" signal. Feature-
  // detection, not browser sniffing: only engines exposing it subscribe.
  if ("webkitPresentationMode" in video) {
    video.addEventListener("webkitpresentationmodechanged", () => {
      if (debugMedia)
        debugLog(`video presentationMode=${video.webkitPresentationMode}`, video);
      if (video.webkitPresentationMode === "inline") beginPresentationRecovery();
    });
  }

  btnFullscreen.addEventListener("click", toggleFullscreen);

  function syncMuteButton() {
    const muted = audioEl.muted; // the video element is never unmuted
    setHidden(iconMuted, !muted);
    setHidden(iconUnmuted, muted);
    btnMute.setAttribute("aria-label", muted ? t("unmute") : t("mute"));
    btnMute.setAttribute("aria-pressed", muted ? "false" : "true");
  }

  btnMute.addEventListener("click", () => {
    audioEl.muted = !audioEl.muted;
    // Safari may pause when the muted flag flips mid-playback; nudge it.
    // Flipping to audible inside this gesture is also what admits the
    // autoplay policy to unmuted playback.
    playMedia(audioEl);
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
  // re-arm the wake lock the browser dropped with the visibility change,
  // and resume playback the OS paused while the page was hidden.
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState !== "visible") return;
    if (!terminal && !ws) connect();
    if (liveState) acquireWakeLock();
    schedulePlaybackRecovery();
  });

  /* ---------------- boot ---------------- */

  async function boot() {
    applySettings();
    btnSettings.setAttribute("aria-label", t("settings"));
    btnReconnect.setAttribute("aria-label", t("reconnect"));

    // Capability detection, not vendor detection: any browser with
    // RTCPeerConnection streams; everything else gets a clear message.
    if (!("RTCPeerConnection" in window)) {
      terminal = true;
      showOverlay("unsupportedTitle", "unsupportedMsg", "error");
      return;
    }
    try {
      const response = await fetch("/api/join/config", { cache: "no-store" });
      const config = response.ok ? await response.json() : null;
      pairingRequired = Boolean(config && config.pairingRequired);
      sessionName =
        config && typeof config.sessionName === "string" && config.sessionName
          ? config.sessionName
          : null;
    } catch {
      // The normal join path reports a server outage and retries.
    }
    if (pairingRequired) showPairing();
    else connect();
  }

  boot();
})();
