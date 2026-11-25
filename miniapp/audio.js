"use strict";

(() => {
  const Audio = window.AudioContext || window.webkitAudioContext;
  const toggle = document.getElementById("game-audio-toggle");
  const slider = document.getElementById("game-audio-volume");
  const storageKey = "malfoid-game-audio";
  const clips = new Map();
  const caseClips = new Map();
  const active = new Set();
  const loops = new Set();
  const lastCue = new Map();
  let context;
  let mix;
  let master;
  let ambience;
  let ambientLoading;
  let introEndsAt = 0;
  let introduced = false;
  let inLobby = false;
  let menuAvailable = false;
  let inputWithKeyboard = false;
  let enabled = true;
  let volume = 50;

  try {
    const saved = JSON.parse(localStorage.getItem(storageKey));
    if (typeof saved?.enabled === "boolean") enabled = saved.enabled;
    if (Number.isFinite(saved?.volume)) volume = Math.max(0, Math.min(100, Math.round(saved.volume)));
  } catch { /* The Activity can run without browser storage. */ }

  const tone = (duration, gain, from, to = from, offset = 0) => ({ duration, gain, from, to, offset });
  const noise = (duration, gain, cutoff, offset = 0) => ({ duration, gain, cutoff, offset });
  const wood = (gain, pitch) => [tone(.067, gain, pitch, pitch * .77), noise(.05, gain * .65, 630)];
  const warmPair = (lower, higher) => [tone(.22, .105, lower), tone(.28, .09, higher, higher, .105)];
  const warmChord = (notes, duration, level) => notes.map((frequency, index) =>
    tone(duration, level, frequency, frequency, index * .026));
  const effects = {
    "menu.ready": warmChord([130.81, 164.81, 196, 293.66], .62, .046),
    "ui.hover": wood(.019, 224),
    "ui.click": wood(.041, 262),
    "roulette.tick": wood(.034, 337),
    "roulette.launch": [noise(.16, .08, 460), tone(.17, .082, 140, 101)],
    "roulette.land": wood(.085, 245),
    "roulette.win": warmPair(174.61, 261.63),
    "roulette.loss": [noise(.22, .058, 420), tone(.20, .050, 149, 139)],
    "cards.deal": [noise(.132, .085, 1220), noise(.084, .035, 500)],
    "cards.place": [noise(.058, .042, 410), tone(.064, .022, 122, 85)],
    "cards.flip": [noise(.117, .101, 1050), tone(.072, .023, 181, 139, .032)],
    "cards.bet": [tone(.084, .060, 245, 152), noise(.069, .070, 670), tone(.062, .041, 291, 212, .047)],
    "round.win": warmChord([130.81, 196, 261.63], .34, .067),
    "round.loss": [tone(.30, .11, 176, 138)],
    "round.push": [tone(.24, .085, 164.81)],
    "mines.safe": [...wood(.066, 271), tone(.195, .066, 329.63, 317, .032)],
    "mines.blast": [tone(.215, .145, 90, 43), noise(.25, .16, 300)],
    "coin.launch": [noise(.123, .054, 760), tone(.14, .061, 243, 339)],
    "coin.land": [...wood(.067, 324), tone(.116, .040, 221, 193, .065)],
    "inventory.sell": [
      noise(.028, .11, 4200), tone(.21, .17, 1568), tone(.13, .10, 2640),
      tone(.11, .08, 2093, 2093, .044),
    ],
    "rocket.count": [tone(.085, .047, 135)],
    "rocket.launch": [noise(.36, .12, 405), tone(.325, .102, 65, 96)],
    "rocket.cashout": warmPair(164.81, 246.94),
    "rocket.crash": [tone(.205, .14, 89, 49), noise(.22, .13, 345)],
  };

  const quietCues = new Set(["ui.hover", "ui.click", "roulette.tick",
    "cards.deal", "cards.place", "cards.flip", "cards.bet", "rocket.count"]);
  const minGap = { "ui.hover": .045, "ui.click": .040, "roulette.tick": .065 };
  const caseSounds = {
    opening: { file: "case-opening.mp3", level: 1 },
    secret: { file: "case-secret.mp3", level: .16 },
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
    if (master) master.gain.setTargetAtTime(enabled ? volume / 185 : 0, context.currentTime, .02);
  }

  function save() {
    try { localStorage.setItem(storageKey, JSON.stringify({ enabled, volume })); }
    catch { /* Settings still apply for this visit. */ }
  }

  function prepareAudio() {
    if (!Audio || context || !enabled || document.hidden) return;
    try {
      context = new Audio();
      master = context.createGain();
      mix = context.createDynamicsCompressor?.() || master;
      if (mix !== master) {
        mix.threshold.value = -23;
        mix.knee.value = 12;
        mix.ratio.value = 5;
        mix.attack.value = .012;
        mix.release.value = .16;
        mix.connect(master);
      }
      master.connect(context.destination);
      context.addEventListener?.("statechange", refreshAudio);
      renderSettings();
    } catch { /* Some clients do not expose usable audio output. */ }
  }

  async function unlock() {
    prepareAudio();
    try {
      if (enabled && !document.hidden && context && context.state !== "running") await context.resume();
    } catch { /* This client did not permit playback yet. */ }
    refreshAudio();
  }

  function periodicBed(name) {
    const duration = name === "menu.ambient" ? 16 : 3.2;
    const rate = Math.min(context.sampleRate, 12000);
    const frames = Math.round(duration * rate);
    const buffer = context.createBuffer(1, frames, rate);
    const samples = buffer.getChannelData(0);
    const menu = name === "menu.ambient";
    const partials = menu
      ? [[65.41, .028], [98, .018], [130.81, .019], [164.81, .011], [196, .009],
        [285.06, .0028], [314.84, .002], [378.31, .002], [451.75, .0014], [516.31, .0011], [614, .001]]
      : [[57, .025], [82, .016], [126, .012], [173, .010], [228, .006], [283, .005], [368, .003], [449, .002]];
    // Integer cycles close every partial and its slow modulation at the loop seam.
    for (const [index, [frequency, gain]] of partials.entries()) {
      const cycles = Math.round(frequency * duration);
      const offset = index * 2.39996;
      for (let frame = 0; frame < frames; frame++) {
        const phase = 2 * Math.PI * frame / frames;
        const drift = .87 + .13 * Math.sin(phase * (1 + index % 2) + offset);
        samples[frame] += Math.sin(cycles * phase + offset + .5 * Math.sin(phase + offset)) * gain * drift;
      }
    }
    const texture = Float32Array.from({ length: frames }, () => Math.random() * 2 - 1);
    let low = 0;
    let high = 0;
    for (let frame = -Math.ceil(rate / 20); frame < frames; frame++) {
      const sample = texture[(frame + frames) % frames];
      high += .28 * (sample - high);
      low += .061 * (sample - low);
      if (frame >= 0) {
        const drift = .82 + .18 * Math.sin(2 * Math.PI * frame / frames);
        samples[frame] += (high - low) * (menu ? .010 : .040) * drift;
      }
    }
    return buffer;
  }

  function effectBuffer(layers) {
    const rate = context.sampleRate;
    const length = Math.max(...layers.map(({ offset, duration }) => offset + duration));
    const buffer = context.createBuffer(1, Math.ceil(length * rate), rate);
    const samples = buffer.getChannelData(0);
    for (const { offset, duration, gain, cutoff, from, to } of layers) {
      const start = Math.round(offset * rate);
      const frames = Math.min(samples.length - start, Math.round(duration * rate));
      const filter = cutoff ? 1 - Math.exp(-2 * Math.PI * cutoff / rate) : 0;
      const attack = rate * Math.min(.031, duration / 4);
      let phase = 0;
      let low = 0;
      for (let frame = 0; frame < frames; frame++) {
        const progress = frame / frames;
        const rise = Math.min(1, frame / attack);
        const envelope = rise * rise * (1 - progress) ** 2;
        let value;
        if (cutoff) {
          low += filter * (Math.random() * 2 - 1 - low);
          value = low;
        } else {
          phase += 2 * Math.PI * (from + (to - from) * progress) / rate;
          value = Math.sin(phase);
        }
        samples[start + frame] += value * gain * envelope;
      }
    }
    return buffer;
  }

  function clipFor(name) {
    if (!clips.has(name)) clips.set(name, effects[name] ? effectBuffer(effects[name]) : periodicBed(name));
    return clips.get(name);
  }

  async function loadAmbience() {
    ambientLoading = true;
    try {
      const response = await fetch("static/menu-ambient.mp3");
      if (!response.ok) return;
      clips.set("menu.ambient", await context.decodeAudioData(await response.arrayBuffer()));
    } catch { /* The synthesized ambience keeps working if the track cannot load. */ }
    finally {
      ambientLoading = false;
      refreshLobby();
    }
  }

  function loadCaseSound(name) {
    if (!caseClips.has(name)) {
      const load = (async () => {
        const response = await fetch(`static/${caseSounds[name].file}`);
        if (!response.ok) throw new Error("Case sound unavailable");
        return context.decodeAudioData(await response.arrayBuffer());
      })().catch(() => {
        caseClips.delete(name);
        return null;
      });
      caseClips.set(name, load);
    }
    return caseClips.get(name);
  }

  function prepareCaseSounds() {
    prepareAudio();
    if (!context) return;
    for (const name of Object.keys(caseSounds)) void loadCaseSound(name);
  }

  function playCaseSound(name) {
    if (!enabled || !volume || document.hidden || !Object.hasOwn(caseSounds, name)) return;
    const started = performance.now();
    let cancelled = false;
    let voice;
    void unlock().then(async () => {
      if (cancelled || !canPlay()) return;
      const buffer = await loadCaseSound(name);
      const offset = name === "opening" ? (performance.now() - started) / 1000 : 0;
      if (cancelled || !canPlay() || !buffer || offset >= buffer.duration) return;
      voice = startSound(`case.${name}`, context.currentTime, false, caseSounds[name].level, { buffer, offset });
    });
    return (fadeMs = 0) => {
      cancelled = true;
      if (voice) stop(voice, fadeMs / 4000);
    };
  }

  function canPlay() {
    return enabled && volume > 0 && !document.hidden && context?.state === "running";
  }

  function disconnect(voice) {
    active.delete(voice);
    voice.source.disconnect();
    voice.gain.disconnect();
    voice.filter?.disconnect();
  }

  function retarget(voice, level, fade) {
    const now = context.currentTime;
    voice.gain.gain.cancelScheduledValues(now);
    voice.gain.gain.setTargetAtTime(level, Math.max(now, voice.start), fade);
  }

  function stop(voice, fade = 0) {
    if (!active.has(voice)) return;
    if (voice === ambience) ambience = undefined;
    if (fade && voice.start <= context.currentTime) {
      retarget(voice, 0, fade);
      voice.source.stop(context.currentTime + 4 * fade);
    } else {
      voice.source.onended = null;
      voice.source.stop();
      disconnect(voice);
    }
  }

  function stopSounds(includeUi) {
    for (const voice of active) {
      if (includeUi || (voice !== ambience && !voice.name.startsWith("ui."))) stop(voice);
    }
    if (includeUi) lastCue.clear();
  }

  function startSound(name, start, loop, level, { buffer = clipFor(name), offset = 0 } = {}) {
    const source = context.createBufferSource();
    const gain = context.createGain();
    const filter = name === "rocket.flight" ? context.createBiquadFilter?.() : undefined;
    const voice = { source, gain, filter, name, start, end: loop ? Infinity : start + buffer.duration - offset };
    source.buffer = buffer;
    source.loop = loop;
    gain.gain.value = loop ? 0 : level;
    if (filter) {
      filter.type = "lowpass";
      filter.frequency.value = 505;
      source.connect(filter);
      filter.connect(gain);
    } else source.connect(gain);
    gain.connect(mix);
    source.onended = () => disconnect(voice);
    active.add(voice);
    source.start(start, offset);
    return voice;
  }

  function play(name, delay = 0) {
    if (!canPlay() || !Object.hasOwn(effects, name)) return false;
    const start = context.currentTime + Math.max(0, Number(delay) || 0) / 1000;
    const gap = minGap[name];
    if (gap && start - (lastCue.get(name) ?? -Infinity) < gap) return false;
    let simultaneous = 0;
    for (const voice of active) {
      if (!voice.source.loop && voice.start <= start + .015 && voice.end > start) simultaneous++;
    }
    if (active.size >= 24 || (quietCues.has(name) && simultaneous >= 4)) return false;
    startSound(name, start, false, 1 / (1 + simultaneous * .43));
    if (gap) lastCue.set(name, start);
    return true;
  }

  function tuneLoop(loop) {
    retarget(loop.voice, .56 + loop.intensity * .33, .10);
    loop.voice.filter?.frequency.setTargetAtTime(505 + loop.intensity * 290, context.currentTime, .14);
  }

  function refreshLoops() {
    if (!canPlay()) return;
    for (const loop of loops) {
      if (active.has(loop.voice)) continue;
      loop.voice = startSound(loop.name, context.currentTime, true, 0);
      tuneLoop(loop);
    }
  }

  function startLoop(name) {
    if (name !== "rocket.flight") return () => {};
    const loop = { name, intensity: .25, voice: undefined };
    loops.add(loop);
    refreshLoops();
    const end = () => {
      loops.delete(loop);
      if (loop.voice) stop(loop.voice, .035);
    };
    end.setIntensity = (level) => {
      if (!Number.isFinite(level)) return;
      loop.intensity = Math.max(0, Math.min(1, level));
      if (active.has(loop.voice)) tuneLoop(loop);
    };
    return end;
  }

  function refreshLobby() {
    if (inLobby && menuAvailable && context && enabled && volume > 0 && !document.hidden &&
        ambientLoading === undefined && typeof fetch === "function") void loadAmbience();
    if (!inLobby || !menuAvailable || !canPlay()) {
      if (ambience) retarget(ambience, 0, .20);
      return;
    }
    if (!introduced && play("menu.ready")) {
      introduced = true;
      introEndsAt = context.currentTime + clipFor("menu.ready").duration;
    }
    if (ambientLoading) return;
    if (!ambience) ambience = startSound("menu.ambient", Math.max(context.currentTime, introEndsAt), true, 0);
    retarget(ambience, 1, .45);
  }

  function refreshAudio() {
    refreshLobby();
    refreshLoops();
  }

  function quietView() {
    return ["cases", "slots"].includes(document.body?.dataset.game);
  }

  function buttonAt(target) {
    if (quietView()) return null;
    const button = target?.closest?.("button, summary, [role='button']");
    if (["cases", "slots"].includes(button?.dataset?.game)) return null;
    return button && !button.disabled && button.getAttribute?.("aria-disabled") !== "true" ? button : null;
  }

  window.GameAudio = {
    play, startLoop, prepareCaseSounds, playCaseSound,
    async inventorySold(signal) {
      if (!enabled || !volume || document.hidden) return;
      await unlock();
      if (!signal.aborted) play("inventory.sell");
    },
    stopGame() { loops.clear(); stopSounds(false); },
    setLobby(value) { inLobby = value; refreshLobby(); },
    menuReady() {
      menuAvailable = true;
      if (inLobby) prepareAudio();
      refreshLobby();
    },
  };

  document.addEventListener("click", async (event) => {
    if (!event.isTrusted) return;
    const button = buttonAt(event.target);
    await unlock();
    if (button && button !== toggle && !quietView()) play("ui.click");
  }, true);
  document.addEventListener("pointerdown", (event) => {
    if (event.isTrusted) inputWithKeyboard = false;
  }, true);
  document.addEventListener("keydown", (event) => {
    if (!event.isTrusted || !["Tab", "ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown"].includes(event.key)) return;
    inputWithKeyboard = true;
    void unlock();
  }, true);
  document.addEventListener("pointerover", (event) => {
    if (event.pointerType !== "mouse" || !event.isTrusted) return;
    const button = buttonAt(event.target);
    if (!button || (event.relatedTarget?.nodeType && button.contains(event.relatedTarget))) return;
    play("ui.hover");
  });
  document.addEventListener("focusin", (event) => {
    if (!inputWithKeyboard || !event.isTrusted) return;
    const button = buttonAt(event.target);
    if (button && button !== buttonAt(event.relatedTarget)) play("ui.hover");
  });
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) stopSounds(true);
    else refreshAudio();
  });

  toggle?.addEventListener("click", async () => {
    enabled = !enabled;
    if (!enabled) stopSounds(true);
    renderSettings();
    save();
    if (enabled) {
      await unlock();
      if (!quietView()) play("ui.click");
    }
  });
  slider?.addEventListener("input", () => {
    volume = Number(slider.value);
    if (!volume) stopSounds(true);
    renderSettings();
    save();
    refreshAudio();
  });
  renderSettings();
})();
