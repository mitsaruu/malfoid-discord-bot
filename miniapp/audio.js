"use strict";

(() => {
  const Audio = window.AudioContext || window.webkitAudioContext;
  const toggle = document.getElementById("game-audio-toggle");
  const slider = document.getElementById("game-audio-volume");
  const storageKey = "malfoid-game-audio";
  const clips = new Map();
  const active = new Map();
  let context;
  let master;
  let lastHover = -Infinity;
  let enabled = true;
  let volume = 50;

  try {
    const saved = JSON.parse(localStorage.getItem(storageKey));
    if (typeof saved?.enabled === "boolean") enabled = saved.enabled;
    if (Number.isFinite(saved?.volume)) volume = Math.max(0, Math.min(100, Math.round(saved.volume)));
  } catch { /* The Activity can run without browser storage. */ }

  function tone(duration, gain, from, to = from, offset = 0) {
    return { duration, gain, from, to, offset };
  }

  function noise(duration, gain, cutoff, offset = 0) {
    return { duration, gain, cutoff, offset };
  }

  const effects = {
    "ui.hover": [tone(.035, .14, 820, 650)],
    "ui.click": [tone(.065, .29, 560, 320), noise(.026, .18, 2200)],
    "roulette.tick": [noise(.044, .24, 2300), tone(.05, .16, 1050, 880)],
    "roulette.launch": [noise(.16, .40, 2600), tone(.14, .20, 180, 640), tone(.06, .16, 1130, 680, .12)],
    "roulette.land": [tone(.1, .32, 470, 240), noise(.075, .38, 2900), tone(.14, .19, 820, 690, .075)],
    "slots.spin": [noise(.51, .28, 1350), ...Array.from({ length: 6 }, (_, index) =>
      tone(.064, .14, 290 + index * 19, 180, index * .084))],
    "slots.stop": [noise(.054, .45, 1700), tone(.102, .36, 245, 103), tone(.07, .14, 660, 540, .032)],
    "cards.deal": [noise(.097, .47, 3700), noise(.034, .33, 1300, .057), tone(.068, .15, 162, 85, .042)],
    "mines.safe": [tone(.12, .29, 720, 780), tone(.16, .22, 1070, 1090, .07)],
    "mines.blast": [noise(.36, .51, 1600), tone(.42, .61, 93, 30), noise(.49, .31, 380, .038)],
    "coin.launch": [tone(.21, .29, 1600, 1020), tone(.11, .13, 2530, 2000, .023)],
    "coin.land": [tone(.18, .35, 1550, 1180), tone(.16, .22, 2390, 2160), noise(.083, .28, 4400)],
  };

  function renderSettings() {
    if (toggle) {
      toggle.disabled = !Audio;
      toggle.setAttribute("aria-pressed", String(Boolean(Audio && enabled)));
      toggle.textContent = enabled ? "Звук вкл." : "Звук выкл.";
    }
    if (slider) {
      slider.value = String(volume);
      slider.disabled = !Audio || !enabled;
    }
    if (master) master.gain.setTargetAtTime(enabled ? volume / 150 : 0, context.currentTime, .02);
  }

  function save() {
    try { localStorage.setItem(storageKey, JSON.stringify({ enabled, volume })); }
    catch { /* Settings still apply for this visit. */ }
  }

  async function unlock() {
    if (!Audio || !enabled || document.hidden) return;
    try {
      if (!context) {
        context = new Audio();
        master = context.createGain();
        master.connect(context.destination);
        renderSettings();
      }
      if (context.state !== "running") await context.resume();
    } catch { /* This client did not permit playback yet. */ }
  }

  function clipFor(name) {
    if (clips.has(name)) return clips.get(name);
    const layers = effects[name];
    const rate = context.sampleRate;
    const length = Math.max(...layers.map(({ offset, duration }) => offset + duration));
    const buffer = context.createBuffer(1, Math.ceil(length * rate), rate);
    const samples = buffer.getChannelData(0);
    for (const { offset, duration, gain, cutoff, from, to } of layers) {
      const start = Math.round(offset * rate);
      const frames = Math.round(duration * rate);
      const filter = cutoff ? 1 - Math.exp(-2 * Math.PI * cutoff / rate) : 0;
      let phase = 0;
      let low = 0;
      for (let frame = 0; frame < frames; frame++) {
        const progress = frame / frames;
        const fade = Math.min(1, frame / (rate * .004)) * (1 - progress) ** 2;
        let value;
        if (cutoff) {
          const white = Math.random() * 2 - 1;
          low += filter * (white - low);
          value = low;
        } else {
          phase += 2 * Math.PI * (from + (to - from) * progress) / rate;
          value = Math.sin(phase);
        }
        samples[start + frame] += value * gain * fade;
      }
    }
    clips.set(name, buffer);
    return buffer;
  }

  function stop(source) {
    if (!active.delete(source)) return;
    source.onended = null;
    source.stop();
    source.disconnect();
  }

  function stopSounds(includeUi) {
    for (const [source, name] of active) {
      if (includeUi || !name.startsWith("ui.")) stop(source);
    }
  }

  function playSound(name, delay, loop) {
    if (!context || context.state !== "running" || !enabled || !volume || document.hidden || !Object.hasOwn(effects, name)) return;
    if (active.size >= 24) {
      const oldestOneShot = [...active.keys()].find((source) => !source.loop);
      if (oldestOneShot) stop(oldestOneShot);
      else return;
    }
    const source = context.createBufferSource();
    source.buffer = clipFor(name);
    source.loop = loop;
    source.connect(master);
    source.start(context.currentTime + Math.max(0, Number(delay) || 0) / 1000);
    active.set(source, name);
    source.onended = () => {
      active.delete(source);
      source.disconnect();
    };
    return () => stop(source);
  }

  function buttonAt(target) {
    const button = target?.closest?.("button, summary, [role='button']");
    return button && !button.disabled && button.getAttribute?.("aria-disabled") !== "true" ? button : null;
  }

  window.GameAudio = {
    play: (name, delay = 0) => playSound(name, delay, false),
    startLoop: (name) => playSound(name, 0, true) ?? (() => {}),
    stopGame: () => stopSounds(false),
  };

  document.addEventListener("click", async (event) => {
    if (!event.isTrusted) return;
    const button = buttonAt(event.target);
    await unlock();
    if (button && button !== toggle) playSound("ui.click", 0, false);
  }, true);
  document.addEventListener("pointerover", (event) => {
    if (event.pointerType !== "mouse" || !event.isTrusted) return;
    const button = buttonAt(event.target);
    if (!button || (event.relatedTarget?.nodeType && button.contains(event.relatedTarget))) return;
    const now = performance.now();
    if (now - lastHover >= 90 && playSound("ui.hover", 0, false)) lastHover = now;
  });
  document.addEventListener("visibilitychange", () => { if (document.hidden) stopSounds(true); });

  toggle?.addEventListener("click", async () => {
    enabled = !enabled;
    if (!enabled) stopSounds(true);
    renderSettings();
    save();
    if (enabled) {
      await unlock();
      playSound("ui.click", 0, false);
    }
  });
  slider?.addEventListener("input", () => {
    volume = Number(slider.value);
    if (!volume) stopSounds(true);
    renderSettings();
    save();
  });
  renderSettings();
})();
