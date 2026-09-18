/* Lumen host dashboard — plain ES2020, no dependencies.
 *
 * Polls /api/admin/state and drives Allow/Deny/Disconnect through the
 * admin API. The admin token lives in the page path (/admin/<token>).
 */
(() => {
  "use strict";

  const token = (location.pathname.split("/").filter(Boolean)[1] || "").trim();

  /* ---------------- strings ---------------- */

  const STRINGS = {
    streaming: "Streaming",
    offline: "Offline",
    copy: "Copy",
    copied: "Copied.",
    copyFailed: "Copy failed — select the text instead.",
    fpsValue: "{capture} / {target} fps",
    fpsPending: "measuring…",
    waited: "waiting {t}",
    connectedFor: "connected {t}",
    noAddress: "unknown address",
    allow: "Allow",
    deny: "Deny",
    disconnect: "Disconnect",
    allowFailed: "Could not allow this device.",
    denyFailed: "Could not deny this device.",
    kickFailed: "Could not disconnect this device.",
  };

  const t = (key, vars) => {
    let s = STRINGS[key] || key;
    if (vars) {
      for (const [k, v] of Object.entries(vars)) s = s.replace(`{${k}}`, v);
    }
    return s;
  };

  function fmtDur(secs) {
    const s = Math.max(0, Math.round(secs));
    if (s < 60) return `${s}s`;
    if (s < 3600) return `${Math.floor(s / 60)}m ${(s % 60).toString().padStart(2, "0")}s`;
    return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  }

  /* ---------------- elements ---------------- */

  const $ = (id) => document.getElementById(id);
  const statusDot = $("status-dot");
  const statusText = $("status-text");
  const sSource = $("s-source");
  const sRes = $("s-res");
  const sFps = $("s-fps");
  const sQuality = $("s-quality");
  const sBitrate = $("s-bitrate");
  const sAudio = $("s-audio");
  const qr = $("qr");
  const urlInput = $("viewer-url");
  const btnCopy = $("btn-copy");
  const copyHint = $("copy-hint");
  const pendingList = $("pending-list");
  const pendingCount = $("pending-count");
  const pendingEmpty = $("pending-empty");
  const connectedList = $("connected-list");
  const connectedCount = $("connected-count");
  const connectedEmpty = $("connected-empty");

  qr.src = `/api/admin/qr?token=${encodeURIComponent(token)}`;

  /* ---------------- api ---------------- */

  async function api(path, method = "GET") {
    const res = await fetch(`${path}${path.includes("?") ? "&" : "?"}token=${encodeURIComponent(token)}`, {
      method,
      cache: "no-store",
    });
    return res;
  }

  async function poll() {
    if (!token) return;
    try {
      const res = await api("/api/admin/state");
      if (!res.ok) throw new Error(String(res.status));
      render(await res.json());
      statusDot.classList.add("live");
      statusText.textContent = t("streaming");
    } catch {
      statusDot.classList.remove("live");
      statusText.textContent = t("offline");
    }
  }

  /* ---------------- render ---------------- */

  function render(state) {
    const src = state.source || {};
    sSource.textContent = src.label || "—";
    sRes.textContent = src.width ? `${src.width}×${src.height}` : "—";
    const fps = state.fps || {};
    sFps.textContent =
      typeof fps.capture === "number"
        ? t("fpsValue", { capture: fps.capture.toFixed(1), target: fps.target })
        : `${t("fpsPending")} (${fps.target ?? "—"})`;
    sQuality.textContent = state.quality || "—";
    sBitrate.textContent = state.bitrate || "—";
    sAudio.textContent = state.audio || "—";
    urlInput.value = state.viewerUrl || "";

    renderPending(state.pending || []);
    renderConnected(state.peers || []);
  }

  function deviceRow(entry, actionSpecs) {
    const li = document.createElement("li");

    const main = document.createElement("div");
    main.className = "device-main";
    const name = document.createElement("div");
    name.className = "device-name";
    name.textContent = entry.device;
    const meta = document.createElement("div");
    meta.className = "device-meta";
    meta.textContent = [entry.address || t("noAddress"), entry._metaLabel].filter(Boolean).join(" · ");
    main.append(name, meta);

    const actions = document.createElement("div");
    actions.className = "device-actions";
    for (const spec of actionSpecs) {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = spec.className;
      btn.textContent = spec.label;
      btn.addEventListener("click", async () => {
        btn.disabled = true;
        try {
          const res = await api(spec.path, "POST");
          if (!res.ok) throw new Error(String(res.status));
        } catch {
          window.alert(spec.failText);
        }
        poll();
      });
      actions.appendChild(btn);
    }

    li.append(main, actions);
    return li;
  }

  function renderPending(items) {
    pendingCount.textContent = String(items.length);
    pendingList.textContent = "";
    pendingEmpty.hidden = items.length > 0;
    pendingList.hidden = items.length === 0;
    for (const item of items) {
      item._metaLabel = t("waited", { t: fmtDur(item.waited_secs) });
      pendingList.appendChild(
        deviceRow(item, [
          {
            className: "allow",
            label: t("allow"),
            path: `/api/admin/pending/${encodeURIComponent(item.id)}/allow`,
            failText: t("allowFailed"),
          },
          {
            className: "deny",
            label: t("deny"),
            path: `/api/admin/pending/${encodeURIComponent(item.id)}/deny`,
            failText: t("denyFailed"),
          },
        ]),
      );
    }
  }

  function renderConnected(items) {
    connectedCount.textContent = String(items.length);
    connectedList.textContent = "";
    connectedEmpty.hidden = items.length > 0;
    connectedList.hidden = items.length === 0;
    for (const item of items) {
      item._metaLabel = t("connectedFor", { t: fmtDur(item.since_secs) });
      connectedList.appendChild(
        deviceRow(item, [
          {
            className: "kick",
            label: t("disconnect"),
            path: `/api/admin/peers/${encodeURIComponent(item.id)}/disconnect`,
            failText: t("kickFailed"),
          },
        ]),
      );
    }
  }

  /* ---------------- copy ---------------- */

  let hintTimer = null;
  function showHint(text) {
    copyHint.textContent = text;
    copyHint.hidden = false;
    clearTimeout(hintTimer);
    hintTimer = setTimeout(() => {
      copyHint.hidden = true;
    }, 2500);
  }

  btnCopy.addEventListener("click", async () => {
    const url = urlInput.value;
    try {
      await navigator.clipboard.writeText(url);
      showHint(t("copied"));
    } catch {
      urlInput.select();
      let ok = false;
      try {
        ok = document.execCommand("copy");
      } catch {
        ok = false;
      }
      showHint(ok ? t("copied") : t("copyFailed"));
    }
  });

  /* ---------------- loop ---------------- */

  poll();
  setInterval(() => {
    if (!document.hidden) poll();
  }, 2000);
})();
