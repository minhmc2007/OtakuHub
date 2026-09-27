(() => {
  "use strict";

  const toast = document.getElementById("oh-toast");
  const toastMsg = document.getElementById("oh-toast-msg");
  let toastTimer = 0;

  function say(message) {
    if (!toast || !message) return;
    if (toastMsg) {
      toastMsg.textContent = message;
    } else {
      toast.textContent = message;
    }
    toast.classList.add("oh-toast-on");
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => {
      toast.classList.remove("oh-toast-on");
    }, 2800);
  }

  document.body.addEventListener("htmx:beforeSwap", (e) => {
    const status = e.detail?.xhr?.status ?? 0;
    // htmx drops error responses by default, which would swallow the alert markup.
    if (status >= 400) {
      e.detail.shouldSwap = true;
      e.detail.isError = false;
    }
  });

  document.body.addEventListener("htmx:afterSwap", (e) => {
    const msg = e.detail?.xhr?.getResponseHeader("oh-message");
    if (msg) say(msg);

    const status = e.detail?.xhr?.status ?? 0;
    if (status >= 400) {
      const box = e.detail.target;
      if (box) {
        box.setAttribute("role", "alert");
        box.setAttribute("aria-live", "assertive");
      }
    }

    const el = e.detail?.target?.querySelector("#oh-video") || e.detail?.target;
    if (el && el.id === "oh-video") loadPlaylist(el);
  });

  document.body.addEventListener("htmx:afterRequest", (e) => {
    const el = e.detail?.elt;
    if (!el) return;
    if (e.detail.successful) {
      const btn = el.tagName === "BUTTON" ? el : el.querySelector("button[type=submit]");
      if (btn) {
        btn.classList.add("oh-down");
        setTimeout(() => btn.classList.remove("oh-down"), 160);
      }
    }
  });

  const video = document.getElementById("oh-video");

  function loadPlaylist(el) {
    const url = el.dataset.playlist;
    if (!url) return;

    const autoplay = el.dataset.autoplay !== "0";
    const canNative = el.canPlayType("application/vnd.apple.mpegurl") !== "";

    if (window.Hls && window.Hls.isSupported()) {
      if (el._hls) el._hls.destroy();
      const hls = new window.Hls({
        lowLatencyMode: false,
        manifestLoadingMaxRetry: 5,
        manifestLoadingRetryDelay: 600,
        levelLoadingMaxRetry: 6,
        levelLoadingRetryDelay: 800,
        fragLoadingMaxRetry: 8,
        fragLoadingRetryDelay: 700,
      });

      hls.on(window.Hls.Events.ERROR, (_e, data) => {
        if (!data.fatal) return;
        if (data.type === window.Hls.ErrorTypes.NETWORK_ERROR) {
          hls.startLoad();
        } else if (data.type === window.Hls.ErrorTypes.MEDIA_ERROR) {
          hls.recoverMediaError();
        } else {
          say("Playback paused: The video stream did not respond.");
        }
      });

      hls.loadSource(url);
      hls.attachMedia(el);
      el._hls = hls;
    } else if (canNative) {
      el.src = url;
    } else {
      say("Your browser does not support HLS video playback.");
    }

    if (autoplay) {
      // A rejection here is the browser refusing autoplay, which is not worth reporting.
      el.play().catch(() => {});
    }
  }

  if (video) loadPlaylist(video);

  const resumeBox = document.getElementById("oh-resume");

  function seekTo(secs) {
    const apply = () => {
      video.currentTime = Math.min(secs, Math.max((video.duration || 0) - 5, 0));
    };
    if (video.readyState >= 1) apply();
    else video.addEventListener("loadedmetadata", apply, { once: true });
  }

  function closeResume() {
    if (resumeBox) resumeBox.remove();
  }

  if (resumeBox && video) {
    const go = document.getElementById("oh-resume-go");
    if (go) {
      go.addEventListener("click", () => {
        seekTo(Number(video.dataset.resume || 0));
        closeResume();
        video.play().catch(() => {});
      });
    }
    const restart = document.getElementById("oh-resume-restart");
    if (restart) {
      // The server drops the stored position, so the next visit offers nothing.
      restart.addEventListener("click", () => {
        delete video.dataset.resume;
        closeResume();
      });
    }
    const dismiss = document.getElementById("oh-resume-dismiss");
    if (dismiss) {
      dismiss.addEventListener("click", closeResume);
    }
    // The video does not seek on its own, so the prompt has to be answered or dismissed
    // before it gets in the way.
    video.addEventListener("play", closeResume, { once: true });
  }

  if (video) {
    const animeId = video.dataset.anime || "";
    const epId = video.dataset.epId || "";
    const epNum = video.dataset.epNum || "";
    let lastReported = -1;

    // The position goes to the server, which is what marks the episode watched and what a
    // return visit offers to resume from. Nothing is kept in the browser.
    const report = (secs, keepalive) => {
      if (!animeId || !epId) return;
      const body = new URLSearchParams({
        anime: animeId,
        ep_id: epId,
        ep: epNum,
        t: String(Math.floor(secs)),
        d: String(Math.floor(video.duration || 0)),
      });
      if (keepalive && navigator.sendBeacon) {
        navigator.sendBeacon("/api/history/position", body);
      } else {
        fetch("/api/history/position", {
          method: "POST",
          body,
          headers: { "content-type": "application/x-www-form-urlencoded" },
          keepalive: true,
        }).catch(() => {});
      }
    };

    setInterval(() => {
      const t = video.currentTime;
      if (t > 0 && Math.abs(t - lastReported) > 10) {
        lastReported = t;
        report(t, false);
      }
    }, 5000);

    // A finished episode is reported the moment it ends, so it is not left half watched
    // because the last periodic report never fired.
    video.addEventListener("ended", () => report(video.duration || video.currentTime, false));

    // The final report on the way out. pagehide and visibilitychange both fire on a close
    // and on a bfcache entry, which sendBeacon survives and a plain fetch does not.
    const flush = () => {
      if (video.currentTime > 1) report(video.currentTime, true);
    };
    window.addEventListener("pagehide", flush);
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "hidden") flush();
    });
  }

  document.body.addEventListener("click", (e) => {
    const btn = e.target.closest("[data-sub]");
    if (!btn || !video) return;
    const track = document.createElement("track");
    track.kind = "subtitles";
    track.label = btn.textContent.trim();
    track.src = btn.dataset.sub;
    track.default = true;
    video.appendChild(track);
    if (video.textTracks.length > 1) {
      video.textTracks[video.textTracks.length - 1].mode = "showing";
    }
    document.querySelectorAll("[data-sub]").forEach((b) => b.classList.remove("oh-btn-active"));
    btn.classList.add("oh-btn-active");
  });

  const epList = document.getElementById("oh-episodes");
  const epInput = document.getElementById("oh-epq");
  if (epList && epInput) {
    const rows = [...epList.querySelectorAll("li")];
    const count = document.getElementById("oh-epcount");
    const empty = document.getElementById("oh-epempty");
    const countLabel = `${rows.length} total`;

    function apply(term) {
      const q = term.trim().toLowerCase();
      let shown = 0;
      for (const row of rows) {
        let hit = !q;
        if (!hit) {
          const range = q.match(/^(\d+)\s*-\s*(\d+)$/);
          if (range) {
            const n = Number(row.dataset.num) || row.querySelector(".oh-episode-num")?.textContent.trim();
            const v = Number(n);
            hit = v >= Number(range[1]) && v <= Number(range[2]);
          } else {
            hit = row.textContent.toLowerCase().includes(q);
          }
        }
        row.hidden = !hit;
        if (hit) shown += 1;
      }
      if (count) count.textContent = q ? `${shown} of ${rows.length}` : countLabel;
      if (empty) empty.hidden = shown > 0;
    }

    epInput.addEventListener("input", () => apply(epInput.value));

    epInput.addEventListener("keydown", (e) => {
      if (e.key !== "Enter") return;
      const first = epList.querySelector("li:not([hidden]) .oh-episode");
      if (first) {
        e.preventDefault();
        location.href = first.getAttribute("href");
      }
    });

    let armed = false;
    document.addEventListener("keydown", (e) => {
      if (e.target.matches("input, textarea, select")) return;
      if (e.key === "g" || e.key === "G") {
        armed = true;
        epInput.focus();
        epInput.select();
        return;
      }
      if (!armed) return;
      armed = false;
      if (/^\d$/.test(e.key)) {
        epInput.value = e.key;
        apply(e.key);
      }
    });

    apply("");
  }

  function humanBytes(n) {
    if (n == null || isNaN(n)) return "0 B";
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let v = Number(n);
    let i = 0;
    while (v >= 1024 && i < units.length - 1) {
      v /= 1024;
      i += 1;
    }
    return i === 0 ? `${v} B` : `${v.toFixed(1)} ${units[i]}`;
  }

  async function pollMetrics() {
    const fields = document.querySelectorAll("[data-metric]");
    if (!fields.length) return;
    try {
      const res = await fetch("/api/metrics", { headers: { accept: "application/json" } });
      if (!res.ok) return;
      const m = await res.json();
      fields.forEach((el) => {
        const key = el.dataset.metric;
        if (key === "cpu_percent") el.textContent = `${m.cpu_percent.toFixed(0)}%`;
        else if (key === "cpu_cores") el.textContent = String(m.cpu_cores);
        else if (key === "mem") el.textContent = `${humanBytes(m.mem_used)} / ${humanBytes(m.mem_total)}`;
        else if (key === "served") el.textContent = humanBytes(m.served_bytes);
        else if (key === "upstream") el.textContent = String(m.upstream_fetches);
        else if (key === "running_jobs") el.textContent = String(m.running_jobs);
      });
      if (m.gpu && m.gpu.name) {
        const gpu = document.querySelector("[data-metric=gpu]");
        if (gpu) {
          const util = m.gpu.utilisation != null ? `${m.gpu.utilisation}% util` : "active";
          gpu.textContent = `${m.gpu.name} (${util})`;
        }
      }
    } catch {
      // The server may be down, in which case the next poll tries again.
    }
  }

  if (document.querySelector("[data-metric]")) {
    pollMetrics();
    setInterval(pollMetrics, 3000);
  }

  document.addEventListener("pointerdown", (e) => {
    const btn = e.target.closest(".oh-btn, .oh-chip, .oh-episode");
    if (btn) {
      btn.style.transition = "none";
    }
  });

  document.addEventListener("pointerup", (e) => {
    const btn = e.target.closest(".oh-btn, .oh-chip, .oh-episode");
    if (btn) {
      btn.style.transition = "";
    }
  });
})();
