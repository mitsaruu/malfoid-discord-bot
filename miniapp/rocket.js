"use strict";

const ROCKET_POLL_MS = 500;
const ROCKET_RETRY_POLL_MS = 2400;
const ROCKET_MAX_REPEAT_ROUNDS = 10;
// These shape the chart for confirmed samples and match the Rust flight phases.
const ROCKET_DOUBLE_MS = 6000;
const ROCKET_CINEMATIC_DOUBLE_MS = 600;
const ROCKET_CRASH_EASE_MS = 160;
const ROCKET_BURST_MS = 760;
const ROCKET_STATUS = {
  idle: "Готовность к старту",
  countdown: "Обратный отсчёт",
  flying: "В полёте",
  crashed: "Авария",
  refunded: "Ставка возвращена",
};

function rocketActive(flight) {
  return flight?.status === "countdown" || flight?.status === "flying";
}

function rocketCanCashout(flight) {
  return flight?.status === "flying" && flight.can_cashout === true && flight.stake !== null && flight.payout == null;
}

function rocketUntilLaunch(flight, sampledAt, timestamp) {
  return Math.max(0, flight.next_launch_ms - Math.max(0, timestamp - sampledAt));
}

function rocketCanBet(flight, sampledAt, timestamp) {
  return flight?.status === "countdown" && flight.can_bet === true && flight.stake === null &&
    rocketUntilLaunch(flight, sampledAt, timestamp) > 0;
}

function rocketMaxStake({ balance, max_bet }) {
  return Math.min(balance, max_bet);
}

function rocketValidStake(snapshot, stake) {
  return Number.isSafeInteger(stake) && stake >= snapshot.min_bet && stake <= rocketMaxStake(snapshot);
}

function rocketMultiplierAt(elapsed, cinematicFrom) {
  const normal = Math.min(elapsed, cinematicFrom ?? elapsed);
  return 100 * 2 ** (normal / ROCKET_DOUBLE_MS + (elapsed - normal) / ROCKET_CINEMATIC_DOUBLE_MS);
}

function rocketPoint(flight) {
  return { elapsed: flight?.elapsed_ms ?? 0, multiplier: flight?.multiplier ?? 100 };
}

function interpolateRocket({ from, to }, progress) {
  if (progress <= 0) return from;
  if (progress >= 1) return to;
  return {
    elapsed: from.elapsed + (to.elapsed - from.elapsed) * progress,
    multiplier: from.multiplier + (to.multiplier - from.multiplier) * progress,
  };
}

function rocketResponse(data) {
  if (data?.error) throw new Error(data.error);
  const flight = data?.flight;
  const cinematicFrom = flight?.cinematic_from_ms;
  if (!Number.isFinite(data?.balance) || !Number.isFinite(data.min_bet) || !Number.isFinite(data.max_bet) ||
      typeof flight?.id !== "string" || !flight.id || !Object.hasOwn(ROCKET_STATUS, flight.status) ||
      flight.status === "idle" || !Number.isFinite(flight.elapsed_ms) || !Number.isFinite(flight.multiplier) ||
      (flight.stake !== null && !(Number.isSafeInteger(flight.stake) && flight.stake > 0)) ||
      !Number.isFinite(flight.next_launch_ms) || flight.next_launch_ms < 0 ||
      typeof flight.can_bet !== "boolean" || typeof flight.can_cashout !== "boolean" ||
      (cinematicFrom != null && (!Number.isFinite(cinematicFrom) || cinematicFrom < 0 || cinematicFrom > flight.elapsed_ms))) {
    throw new Error("Сервер вернул неполные данные полёта.");
  }
  return data;
}

function createRocketFeed({
  request, onChange, canAutoStart = () => true,
  now = () => performance.now(), setTimer = setTimeout, clearTimer = clearTimeout,
}) {
  let snapshot = null;
  let sampledAt = 0;
  let visible = false;
  let timer = null;
  let read = null;
  let pending = null;
  let needsSync = true;
  let syncError = "";
  let actionError = "";
  let repeat = null;
  let repeatNotice = "";
  let lastStartId = null;

  function report() {
    onChange({
      snapshot, sampledAt, pending, needsSync, syncing: Boolean(read), syncError, actionError,
      repeat: repeat && { ...repeat }, repeatNotice,
    });
  }

  function clearRepeat(notice) {
    if (!repeat) return false;
    repeat = null;
    repeatNotice = notice;
    return true;
  }

  function repeatReady() {
    if (!repeat || needsSync) return false;
    const { flight } = snapshot;
    if (!canAutoStart()) clearRepeat("connection");
    else if (flight.status === "refunded" && flight.stake !== null) clearRepeat("refunded");
    else if (flight.id !== lastStartId && rocketCanBet(flight, sampledAt, now())) {
      if (rocketValidStake(snapshot, repeat.stake)) return true;
      clearRepeat("stake");
    }
    return false;
  }

  function clearPoll() {
    clearTimer(timer);
    timer = null;
  }

  function schedule(startedAt = now()) {
    clearPoll();
    if (!visible || pending) return;
    const interval = needsSync ? ROCKET_RETRY_POLL_MS : ROCKET_POLL_MS;
    timer = setTimer(sync, Math.max(50, interval - (now() - startedAt)));
  }

  function accept(data) {
    snapshot = rocketResponse(data);
    sampledAt = now();
    needsSync = false;
    syncError = "";
  }

  async function sync() {
    if (!visible || pending || read) return;
    clearPoll();
    const startedAt = now();
    const current = {};
    read = current;
    if (needsSync) report();
    try {
      const data = await request("api/rocket/state");
      if (read === current) accept(data);
    } catch (error) {
      if (read !== current) return;
      needsSync = true;
      clearRepeat("connection");
      syncError = error.name === "TimeoutError" ? "Сервер не ответил вовремя." : error.message;
    } finally {
      if (read === current) {
        read = null;
        if (repeatReady()) {
          repeat.id = snapshot.flight.id;
          repeat.remaining--;
          const { id, stake, target } = repeat;
          void mutate("start", { bet: String(stake), auto: target === null ? null : String(target / 100), expected_id: id }, repeat);
        } else {
          report();
          schedule(startedAt);
        }
      }
    }
  }

  async function mutate(action, body, run = null) {
    clearPoll();
    // A read started before this mutation can never replace its response.
    read = null;
    pending = action;
    actionError = "";
    report();
    try {
      if (run && (repeat !== run || !visible || !canAutoStart())) {
        if (repeat === run) clearRepeat("connection");
        return;
      }
      if (action === "start") lastStartId = body.expected_id;
      accept(await request("api/rocket/" + action, body));
      if (run && repeat === run) {
        const next = snapshot.flight;
        if (next.id !== run.id || next.stake !== run.stake || next.auto !== run.target) clearRepeat("changed");
        else if (next.status === "refunded") clearRepeat("refunded");
        else if (!run.remaining) clearRepeat("complete");
      }
    } catch (error) {
      needsSync = true;
      clearRepeat("connection");
      actionError = error.name === "TimeoutError" || error.name === "TypeError" ?
        "Ответ на запрос потерян. Проверьте состояние полёта после обновления." : error.message;
    } finally {
      pending = null;
      report();
      if (needsSync) sync();
      else schedule();
    }
  }

  return {
    show() {
      if (visible) return;
      visible = true;
      needsSync = true;
      sync();
    },
    hide() {
      visible = false;
      clearPoll();
      read = null;
      if (clearRepeat("stopped")) report();
    },
    sync,
    startRepeat(rounds, stake = snapshot?.flight.stake, target = snapshot?.flight.auto ?? null) {
      if (!visible || !canAutoStart() || pending || needsSync || repeat ||
          !Number.isInteger(rounds) || rounds < 1 || rounds > ROCKET_MAX_REPEAT_ROUNDS) return;
      if (!Number.isSafeInteger(stake) || stake < snapshot.min_bet || stake > snapshot.max_bet ||
          (target !== null && (!Number.isInteger(target) || target < 101 || target > 999))) return;
      repeat = { id: snapshot.flight.id, stake, target, remaining: rounds };
      repeatNotice = "";
      report();
      // Recheck the round after opting in, even if the cached countdown was open.
      void sync();
    },
    stopRepeat(notice = "stopped") {
      if (clearRepeat(notice)) report();
    },
    async launch(bet, auto) {
      if (!visible || pending || needsSync || !rocketCanBet(snapshot?.flight, sampledAt, now())) return;
      if (!clearRepeat("stopped")) repeatNotice = "";
      await mutate("start", { bet, auto, expected_id: snapshot.flight.id });
    },
    async cashout() {
      if (!visible || pending || needsSync || !rocketCanCashout(snapshot?.flight)) return;
      await mutate("cashout", { id: snapshot.flight.id });
    },
  };
}

function drawRocketShip(ctx, x, y, angle, timestamp, flying, reducedMotion, color) {
  ctx.save();
  ctx.translate(x, y);
  ctx.rotate(angle);

  if (flying) {
    const pulse = reducedMotion ? 0 : 3.5 * Math.sin(timestamp / 32) + 2 * Math.cos(timestamp / 19);
    const flame = ctx.createLinearGradient(-11, 0, -45, 0);
    flame.addColorStop(0, "#fff7df");
    flame.addColorStop(.3, "#b5a1ff");
    flame.addColorStop(.72, "#9260ff70");
    flame.addColorStop(1, "#7540ff00");
    ctx.fillStyle = flame;
    ctx.beginPath();
    ctx.moveTo(-10, -4);
    ctx.quadraticCurveTo(-22, -5, -45 - pulse, 0);
    ctx.quadraticCurveTo(-25, 5, -10, 4);
    ctx.fill();
    if (!reducedMotion) {
      for (let i = 0; i < 10; i++) {
        const age = ((timestamp / (180 + i * 17) + i * .37) % 1);
        ctx.globalAlpha = (1 - age) * .7;
        ctx.fillStyle = i % 2 ? "#dcbaff" : "#fce0b6";
        ctx.beginPath();
        ctx.arc(-12 - age * 55, (i % 2 ? 1 : -1) * age * (2 + i * 1.3), 1.6 - age, 0, Math.PI * 2);
        ctx.fill();
      }
      ctx.globalAlpha = 1;
    }
  }

  const metal = ctx.createLinearGradient(0, -9, 0, 9);
  metal.addColorStop(0, "#fcfaff");
  metal.addColorStop(.53, "#d9ceff");
  metal.addColorStop(1, "#9181c9");
  ctx.shadowColor = color;
  ctx.shadowBlur = flying ? 13 : 5;
  ctx.fillStyle = color;
  ctx.beginPath();
  ctx.moveTo(-5, -4);
  ctx.lineTo(-15, -11);
  ctx.lineTo(-13, 0);
  ctx.lineTo(-15, 11);
  ctx.lineTo(-5, 4);
  ctx.fill();
  ctx.fillStyle = metal;
  ctx.beginPath();
  ctx.moveTo(-12, -4);
  ctx.bezierCurveTo(0, -9, 9, -8, 18, 0);
  ctx.bezierCurveTo(9, 8, 0, 9, -12, 4);
  ctx.closePath();
  ctx.fill();
  ctx.shadowBlur = 0;
  ctx.beginPath();
  ctx.fillStyle = "#302450";
  ctx.arc(5, 0, 3.5, 0, Math.PI * 2);
  ctx.fill();
  ctx.strokeStyle = "#fcfaff";
  ctx.lineWidth = 1;
  ctx.stroke();
  ctx.restore();
}

function drawRocketBurst(ctx, x, y, progress, color, crashed) {
  ctx.save();
  ctx.strokeStyle = color;
  ctx.fillStyle = color;
  ctx.lineWidth = 1.4;
  ctx.globalAlpha = (1 - progress) * .75;
  const distance = 7 + 54 * (1 - (1 - progress) ** 3);
  ctx.beginPath();
  ctx.arc(x, y, distance * .75, 0, Math.PI * 2);
  ctx.stroke();
  for (let i = 0; i < 18; i++) {
    const angle = i * 2.4;
    const spread = distance * (.6 + (i % 5) / 5);
    const sparkX = x + Math.cos(angle) * spread;
    const sparkY = y + Math.sin(angle) * spread * (crashed ? .9 : .7);
    ctx.beginPath();
    ctx.arc(sparkX, sparkY, (i % 4 + 2) * (1 - progress) / 1.8, 0, Math.PI * 2);
    ctx.fill();
  }
  ctx.restore();
}

function createRocketChart(canvas) {
  const ctx = canvas.getContext("2d");
  const stars = Array.from({ length: 44 }, (_, i) => [((i * 197 + 71) % 503) / 503, ((i * 71 + 13) % 241) / 241]);
  const colors = { crashed: "#ff957b", refunded: "#ffd690" };
  const tickFactor = new Intl.NumberFormat("ru-RU", { minimumFractionDigits: 1 });
  const tickTime = new Intl.NumberFormat("ru-RU");

  return function draw(status, { elapsed, multiplier: factor }, cinematicFrom, timestamp, transition, reducedMotion) {
    const width = canvas.clientWidth;
    const height = canvas.clientHeight;
    if (!ctx || width < 100 || height < 100) return;
    const scale = Math.min(window.devicePixelRatio || 1, 2);
    const pixelWidth = Math.round(width * scale);
    const pixelHeight = Math.round(height * scale);
    if (canvas.width !== pixelWidth) canvas.width = pixelWidth;
    if (canvas.height !== pixelHeight) canvas.height = pixelHeight;
    ctx.setTransform(pixelWidth / width, 0, 0, pixelHeight / height, 0, 0);
    ctx.clearRect(0, 0, width, height);

    const left = width < 365 ? 51 : 57;
    const right = width - (width < 365 ? 22 : 30);
    const bottom = height - 34;
    const top = 25;
    const time = Math.max(0, elapsed);
    const timeLimit = Math.max(10500, time + 4000);
    const factorLimit = Math.max(250, factor * 1.12 + 70);
    const xFor = (value) => left + (right - left) * value / timeLimit;
    const yFor = (value) => bottom - (bottom - top) * (value - 100) / (factorLimit - 100);
    const x = xFor(time);
    const y = yFor(factor);
    const color = colors[status] || (cinematicFrom != null && time >= cinematicFrom ? "#f5cd91" : "#bcabff");

    stars.forEach(([sx, sy], i) => {
      ctx.fillStyle = i % 5 ? "#b5a8ee34" : "#d5c9ff75";
      ctx.fillRect(sx * width, sy * (height - 12), i % 5 ? 1 : 1.6, i % 5 ? 1 : 1.6);
    });

    ctx.font = `${width < 365 ? 10 : 11}px ui-monospace, Consolas, monospace`;
    ctx.fillStyle = "#a59dbb";
    ctx.lineWidth = 1;
    ctx.strokeStyle = "#b5a1eb21";
    ctx.textAlign = "right";
    const stepScale = 10 ** Math.floor(Math.log10(factorLimit / 5));
    const factorStep = Math.max(50, [1, 2, 5, 10].find((step) => step * stepScale >= factorLimit / 5) * stepScale);
    ctx.beginPath();
    for (let value = 100; value <= factorLimit; value += factorStep - value % factorStep) {
      const lineY = yFor(value);
      ctx.moveTo(left, lineY);
      ctx.lineTo(right, lineY);
      ctx.fillText(tickFactor.format(value / 100) + "×", left - 8, lineY + 4);
    }
    ctx.textAlign = "center";
    const timeStep = timeLimit > 15500 ? 4000 : width < 365 ? 2500 : 2000;
    for (let value = 0; value <= timeLimit; value += timeStep) {
      const lineX = xFor(value);
      ctx.moveTo(lineX, top);
      ctx.lineTo(lineX, bottom);
      ctx.fillText(tickTime.format(value / 1000) + " с", lineX, bottom + 21);
    }
    ctx.stroke();

    let angle = -.22;
    if (time > 0) {
      const heightAtTime = rocketMultiplierAt(time, cinematicFrom) - 100;
      const yAt = (t) => {
        const gain = heightAtTime ? (rocketMultiplierAt(t, cinematicFrom) - 100) / heightAtTime : t / time;
        return yFor(100 + (factor - 100) * gain);
      };
      const curve = new Path2D();
      curve.moveTo(left, bottom);
      for (let i = 1; i < 48; i++) {
        const t = time * i / 48;
        curve.lineTo(xFor(t), yAt(t));
      }
      curve.lineTo(x, y);
      const area = new Path2D(curve);
      area.lineTo(x, bottom);
      area.closePath();
      const glow = ctx.createLinearGradient(0, top, 0, bottom);
      glow.addColorStop(0, color + "59");
      glow.addColorStop(1, color + "12");
      ctx.fillStyle = glow;
      ctx.fill(area);
      ctx.strokeStyle = color;
      ctx.lineWidth = 2.4;
      ctx.shadowColor = color;
      ctx.shadowBlur = 13;
      ctx.stroke(curve);
      ctx.shadowBlur = 0;
      const behind = Math.max(0, time - 90);
      angle = Math.min(angle, Math.atan2(y - yAt(behind), x - xFor(behind)));
    }

    if (status === "idle" || status === "countdown" || status === "flying") {
      drawRocketShip(ctx, x, y, angle, timestamp, status === "flying", reducedMotion, color);
    } else {
      ctx.fillStyle = color;
      ctx.beginPath();
      ctx.arc(x, y, 4, 0, Math.PI * 2);
      ctx.fill();
      ctx.beginPath();
      ctx.strokeStyle = color;
      ctx.arc(x, y, 8, 0, Math.PI * 2);
      ctx.stroke();
      if (!reducedMotion && transition !== null && timestamp >= transition &&
          timestamp - transition < ROCKET_BURST_MS && status !== "refunded") {
        drawRocketBurst(ctx, x, y, (timestamp - transition) / ROCKET_BURST_MS, color, status === "crashed");
      }
    }
  };
}

function createRocketView(request) {
  const get = (name) => document.getElementById("rocket-" + name);
  const root = document.getElementById("rocket");
  const chart = get("chart");
  const phaseLabel = get("phase");
  const clock = get("time");
  const clockLabel = get("time-label");
  const multiplierLabel = get("multiplier");
  const multiplierCaption = get("multiplier-caption");
  const balance = get("balance");
  const message = get("message");
  const retry = get("retry");
  const form = get("form");
  const action = get("action");
  const settings = get("settings");
  const bet = get("bet");
  const auto = get("auto");
  const number = new Intl.NumberFormat("ru-RU");
  const factor = new Intl.NumberFormat("ru-RU", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
  const seconds = new Intl.NumberFormat("ru-RU", { minimumFractionDigits: 1, maximumFractionDigits: 1 });
  const motion = window.matchMedia("(prefers-reduced-motion: reduce)");
  const draw = createRocketChart(chart);
  const feed = createRocketFeed({ request, onChange: update, canAutoStart });
  let current = { snapshot: null, sampledAt: 0, needsSync: true };
  let shown = false;
  let frame = null;
  let transition = null;
  let flightMotion = null;
  let crashApproach = null;
  let fieldError = "";
  let connectionError = "";

  const repeatPanel = document.createElement("section");
  repeatPanel.className = "rocket-repeat";
  repeatPanel.setAttribute("aria-label", "Автоставка");
  const repeatSetup = document.createElement("div");
  repeatSetup.className = "rocket-repeat-setup";
  const repeatLabel = document.createElement("label");
  repeatLabel.textContent = "Раунды";
  repeatLabel.htmlFor = "rocket-repeat-rounds";
  const repeatRounds = document.createElement("select");
  repeatRounds.id = "rocket-repeat-rounds";
  repeatRounds.setAttribute("aria-describedby", "rocket-repeat-status");
  for (let count = 1; count <= ROCKET_MAX_REPEAT_ROUNDS; count++) {
    const option = document.createElement("option");
    option.value = String(count);
    option.textContent = option.value;
    repeatRounds.append(option);
  }
  repeatRounds.value = "1";
  const repeatStart = document.createElement("button");
  repeatStart.type = "button";
  repeatStart.className = "rocket-secondary";
  repeatStart.textContent = "Включить автоставку";
  repeatStart.disabled = true;
  repeatSetup.append(repeatLabel, repeatRounds, repeatStart);
  const repeatStop = document.createElement("button");
  repeatStop.type = "button";
  repeatStop.className = "rocket-secondary";
  repeatStop.textContent = "Остановить автоставку";
  repeatStop.hidden = true;
  const repeatStatus = document.createElement("p");
  repeatStatus.id = "rocket-repeat-status";
  repeatStatus.setAttribute("role", "status");
  repeatPanel.append(repeatSetup, repeatStatus, repeatStop);
  form.insertBefore(repeatPanel, action);

  function canAutoStart() {
    return shown && !root.hidden && !document.hidden && !connectionError && navigator.onLine !== false;
  }

  function write(element, value) {
    if (element.textContent !== value) element.textContent = value;
  }

  function displayFactor(multiplier) {
    return factor.format(multiplier / 100) + "×";
  }

  function invalidate() {
    if (shown && !document.hidden && frame === null) frame = requestAnimationFrame(paint);
  }

  function paint(timestamp) {
    frame = null;
    if (!shown || document.hidden) return;
    const flight = current.snapshot?.flight;
    const status = flight?.status || "idle";
    const running = !motion.matches && !connectionError && !current.needsSync;
    const confirmed = rocketPoint(flight);
    const approaching = running && crashApproach && timestamp < transition;
    let visual = confirmed;
    if (running && status === "flying") {
      if (flightMotion?.flight !== flight) {
        // Start from the position at receipt, and stop at the newest confirmed point.
        // A crash can happen immediately after any live sample.
        const from = flightMotion?.flight.id === flight.id ?
          interpolateRocket(flightMotion, (current.sampledAt - flightMotion.startedAt) / ROCKET_POLL_MS) : confirmed;
        flightMotion = { flight, from, to: confirmed, startedAt: current.sampledAt };
      }
      visual = interpolateRocket(flightMotion, (timestamp - flightMotion.startedAt) / ROCKET_POLL_MS);
      flightMotion.painted = visual;
    } else {
      flightMotion = null;
      if (approaching) {
        const remaining = Math.min(1, (transition - timestamp) / ROCKET_CRASH_EASE_MS);
        visual = interpolateRocket(crashApproach, 1 - remaining ** 2);
      } else {
        crashApproach = null;
      }
    }
    const waiting = flight && status !== "flying";
    const clockTimestamp = current.needsSync ? current.sampledAt : timestamp;
    const launchIn = waiting ? rocketUntilLaunch(flight, current.sampledAt, clockTimestamp) : 0;
    write(multiplierLabel, displayFactor(Math.floor(status === "flying" ? visual.multiplier : confirmed.multiplier)));
    write(clockLabel, status === "countdown" ? "До старта" : waiting ? "До следующего старта" : "Время полёта");
    const time = waiting ? Math.ceil(launchIn / 100) / 10 : visual.elapsed / 1000;
    write(clock, flight ? seconds.format(time) + " с" : "—");
    if (status === "countdown" && launchIn === 0 && action.getAttribute("aria-disabled") === "false") {
      renderControls(timestamp);
    }
    draw(approaching ? "flying" : status, visual, flight?.cinematic_from_ms, timestamp, transition, !running);
    if (running && (status === "flying" || launchIn > 0 ||
        (transition !== null && timestamp - transition < ROCKET_BURST_MS))) invalidate();
  }

  function useCurrentWager(flight) {
    return rocketActive(flight) && flight.stake !== null;
  }

  function repeatTerms(stake, target, rounds) {
    const cashout = target == null ? "ручной вывод" : "автовывод " + displayFactor(target);
    const limit = rounds ? " Лимит ставок — " + number.format(stake * rounds) + " мон." : "";
    return number.format(stake) + " мон. за раунд, " + cashout + "." + limit;
  }

  function renderRepeat(flight, locked, lowBalance) {
    const { repeat, repeatNotice, pending, snapshot } = current;
    const useWager = useCurrentWager(flight);
    repeatSetup.hidden = Boolean(repeat);
    repeatStop.hidden = !repeat;
    repeatRounds.disabled = locked || Boolean(repeat) || (lowBalance && !useWager);
    repeatStart.disabled = repeatRounds.disabled || !repeatRounds.value;

    const notices = {
      stopped: "Автоставка остановлена.",
      connection: "Автоставка прервана. Для продолжения включите её заново.",
      changed: "Автоставка остановлена. Проверьте ставку текущего раунда.",
      refunded: "Автоставка остановлена. Ставка возвращена.",
      stake: "Автоставка остановлена. Сумма ставки недоступна.",
      complete: "Автоставка завершена. Лимит раундов исчерпан.",
    };
    const notice = notices[repeatNotice] || "";
    let status = notice || "Выберите число раундов для автоматических ставок.";
    if (repeat) {
      const progress = pending === "start" ?
        "Принимаем ставку. После неё осталось раундов: " : "Автоставка включена. Осталось раундов: ";
      status = progress + repeat.remaining + ". " + repeatTerms(repeat.stake, repeat.target, repeat.remaining);
    } else if (snapshot) {
      const wager = useWager ? { stake: flight.stake, target: flight.auto } : readWager(snapshot);
      if (wager) status = (notice ? notice + " " : "") + repeatTerms(wager.stake, wager.target, Number(repeatRounds.value));
    }
    write(repeatStatus, status);
  }

  function renderControls(timestamp = performance.now()) {
    const { snapshot, pending, needsSync, syncing, syncError, actionError } = current;
    const flight = snapshot?.flight;
    const status = flight?.status || "idle";
    const hasStake = flight?.stake != null;
    const paid = flight?.payout != null && status !== "refunded";
    const cinematic = flight?.cinematic_from_ms != null;
    const lowBalance = snapshot && rocketMaxStake(snapshot) < snapshot.min_bet;
    const locked = Boolean(!canAutoStart() || pending || needsSync || !snapshot);
    const canBet = rocketCanBet(flight, current.sampledAt, timestamp);
    const canCashout = rocketCanCashout(flight);
    const lost = hasStake && !paid && (status === "crashed" || (status === "flying" && !canCashout));
    const actionEnabled = !locked && (canCashout || (canBet && !lowBalance));
    root.dataset.phase = status;
    root.dataset.cashedOut = String(paid);
    root.dataset.cinematic = String(cinematic);
    write(phaseLabel, connectionError || (!snapshot && syncError) ? "Нет соединения" :
      snapshot ? ROCKET_STATUS[status] : "Подключение");
    write(multiplierCaption, status === "crashed" ? "Пик полёта" : cinematic ? "Кинополёт" : "Коэффициент");
    write(balance, snapshot ? number.format(snapshot.balance) + " мон." : "—");
    settings.hidden = useCurrentWager(flight);
    settings.disabled = locked || settings.hidden || lowBalance;
    action.setAttribute("aria-disabled", String(!actionEnabled));
    const actionText = pending === "start" ? "Принимаем ставку…" : pending === "cashout" ? "Забираем монеты…" :
      connectionError ? "Соединение закрыто" : needsSync ? "Подключение…" :
      canCashout ? "Забрать монеты" : canBet ? (lowBalance ? "Недостаточно монет" : "Поставить") :
      status === "countdown" && hasStake ? "Ставка принята" :
      status === "countdown" ? "Ждём старта…" : status === "flying" ? "Ждём конца полёта…" : "Ждём следующий раунд…";
    write(action, actionText);
    retry.hidden = !syncError || Boolean(connectionError);
    retry.disabled = Boolean(syncing || pending);

    get("flight").hidden = !hasStake;
    if (hasStake) {
      write(get("stake"), number.format(flight.stake) + " мон.");
      write(get("target"), flight.auto == null ? "Выключен" : displayFactor(flight.auto));
      get("payout-row").hidden = flight.payout == null;
      const payoutLabel = status === "refunded" ? "Возврат" : flight.automatic ? "Автовыплата" : "Выплата";
      const cashoutMultiplier = flight.cashout_multiplier;
      write(get("payout-label"), cashoutMultiplier == null ? payoutLabel : payoutLabel + " · " + displayFactor(cashoutMultiplier));
      if (flight.payout != null) write(get("payout"), number.format(flight.payout) + " мон.");
    }

    if (snapshot) {
      if (!bet.value && !bet.dataset.edited) bet.value = String(snapshot.min_bet);
      write(get("bet-hint"), lowBalance ? "Минимальная ставка — " + number.format(snapshot.min_bet) + " мон." :
        number.format(snapshot.min_bet) + "–" + number.format(rocketMaxStake(snapshot)) + " мон.");
    }
    renderRepeat(flight, locked, lowBalance);

    const result = {
      idle: "Укажите ставку перед стартом.",
      countdown: hasStake ? "Вывод станет доступен после старта." :
        canBet ? "Ставки принимаются до старта." : "Ожидаем старта раунда.",
      flying: cinematic && paid ? "Выплата зафиксирована. Продолжается кинополёт." :
        paid ? (flight.automatic ? "Автовывод сработал. Полёт продолжается." :
        "Монеты зачислены. Полёт продолжается.") : canCashout ? "Вывод подтверждает сервер в момент запроса." :
        "Ставки откроются в следующем раунде.",
      crashed: paid ? "Ракета разбилась. Выплата уже на балансе." : "Ставки откроются в следующем раунде.",
      refunded: hasStake ? "Монеты возвращены на баланс." : "Ставки откроются в следующем раунде.",
    };
    const error = connectionError || syncError || fieldError || actionError;
    message.dataset.tone = error ? "error" : status;
    const feedback = error || (needsSync ? "Обновляем данные полёта…" : result[status]);
    write(message, !needsSync && lost ? "Ставка проиграна" + (error ? ". " + error : "") : feedback);
  }

  function update(view) {
    const previous = current.snapshot?.flight;
    const next = view.snapshot?.flight;
    if (next?.id !== previous?.id || next?.status !== previous?.status) {
      const sameFlight = shown && previous && previous.id === next?.id;
      transition = sameFlight && rocketActive(previous) && !rocketActive(next) ? view.sampledAt : null;
      crashApproach = null;
      if (sameFlight && !current.needsSync && !view.needsSync && !motion.matches) {
        if (previous.status === "countdown" && next.status === "flying") {
          flightMotion = { flight: next, from: rocketPoint(null), to: rocketPoint(next), startedAt: view.sampledAt };
        } else if (previous.status === "flying" && next.status === "crashed" &&
            flightMotion?.flight.id === next.id && flightMotion.painted) {
          // The peak is now known. Finish the last segment before drawing the burst.
          crashApproach = { from: flightMotion.painted, to: rocketPoint(next) };
          transition += ROCKET_CRASH_EASE_MS;
        }
      }
    }
    current = view;
    renderControls();
    invalidate();
  }

  function invalid(input, text) {
    fieldError = text;
    input.setAttribute("aria-invalid", "true");
    input.focus();
    renderControls();
  }

  function clearFieldError() {
    fieldError = "";
    bet.removeAttribute("aria-invalid");
    auto.removeAttribute("aria-invalid");
    renderControls();
  }

  function readWager(data, showError = false) {
    const rawStake = bet.value.trim().replace(/\s+/g, "");
    const stake = Number(rawStake);
    if (!/^\d+$/.test(rawStake) || !rocketValidStake(data, stake)) {
      if (showError) invalid(bet, "Введите целое число монет от " + number.format(data.min_bet) +
        " до " + number.format(rocketMaxStake(data)) + ".");
      return null;
    }
    const rawTarget = auto.value.trim().replace(",", ".");
    if (rawTarget && (!/^[1-9](?:\.\d{1,2})?$/.test(rawTarget) || Number(rawTarget) < 1.01)) {
      if (showError) invalid(auto, "Введите коэффициент от 1,01× до 9,99× или оставьте поле пустым.");
      return null;
    }
    return { stake, target: rawTarget ? Math.round(Number(rawTarget) * 100) : null };
  }

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    if (action.getAttribute("aria-disabled") === "true") return;
    const data = current.snapshot;
    if (rocketCanCashout(data.flight)) {
      feed.cashout();
      return;
    }
    const wager = readWager(data, true);
    if (!wager) return;
    clearFieldError();
    feed.launch(String(wager.stake), wager.target === null ? null : String(wager.target / 100));
  });

  bet.addEventListener("input", () => {
    bet.dataset.edited = "true";
    clearFieldError();
  });
  auto.addEventListener("input", clearFieldError);
  for (const shortcut of ["min", "max"]) {
    get(shortcut).addEventListener("click", () => {
      const data = current.snapshot;
      bet.value = String(shortcut === "min" ? data.min_bet : rocketMaxStake(data));
      bet.dataset.edited = "true";
      clearFieldError();
    });
  }
  retry.addEventListener("click", feed.sync);
  repeatRounds.addEventListener("change", () => renderControls());
  repeatStart.addEventListener("click", () => {
    if (repeatStart.disabled) return;
    const data = current.snapshot;
    const rounds = Number(repeatRounds.value);
    const wager = useCurrentWager(data.flight) ? { stake: data.flight.stake, target: data.flight.auto ?? null } :
      readWager(data, true);
    if (!wager) return;
    clearFieldError();
    feed.startRepeat(rounds, wager.stake, wager.target);
    if (!repeatStop.hidden) repeatStop.focus();
  });
  repeatStop.addEventListener("click", () => {
    feed.stopRepeat();
    if (!repeatStart.disabled) repeatStart.focus();
  });

  function suspend() {
    feed.hide();
    cancelAnimationFrame(frame);
    frame = null;
    flightMotion = null;
    crashApproach = null;
    transition = null;
  }

  function resume() {
    if (shown && !document.hidden && !connectionError) feed.show();
    invalidate();
  }

  document.addEventListener("visibilitychange", () => {
    if (!shown) return;
    if (document.hidden) suspend();
    else resume();
  });
  window.addEventListener("pagehide", suspend);
  window.addEventListener("pageshow", resume);
  window.addEventListener("offline", () => {
    feed.stopRepeat("connection");
    renderControls();
  });
  window.addEventListener("online", () => {
    renderControls();
    if (canAutoStart()) feed.sync();
  });
  motion.addEventListener("change", invalidate);
  new ResizeObserver(invalidate).observe(chart);

  return {
    show() {
      shown = true;
      resume();
    },
    hide() {
      shown = false;
      suspend();
    },
    disconnect(reason) {
      connectionError = reason;
      suspend();
      renderControls();
      invalidate();
    },
  };
}
