"use strict";

const ROCKET_POLL_MS = 500;
const ROCKET_IDLE_POLL_MS = 2400;
const ROCKET_MAX_REPEAT_ROUNDS = 10;
const ROCKET_MAX_MULTIPLIER = 1000;
const ROCKET_DOUBLE_MS = 6000;
const ROCKET_MAX_DURATION = ROCKET_DOUBLE_MS * Math.log2(ROCKET_MAX_MULTIPLIER / 100);
const ROCKET_STATUS = {
  idle: "Готовность к старту",
  countdown: "Обратный отсчёт",
  flying: "В полёте",
  paid: "Монеты получены",
  crashed: "Авария",
  refunded: "Ставка возвращена",
};

function rocketActive(flight) {
  return flight?.status === "countdown" || flight?.status === "flying";
}

function rocketCanCashout(flight) {
  return flight?.status === "flying" && flight.payout == null;
}

function rocketMultiplierAt(elapsed) {
  return elapsed >= ROCKET_MAX_DURATION ? ROCKET_MAX_MULTIPLIER : 100 * 2 ** (Math.max(0, elapsed) / ROCKET_DOUBLE_MS);
}

function rocketTelemetry(flight, sinceSample) {
  if (!flight) return { elapsed: 0, multiplier: 100 };
  const drift = Math.min(650, Math.max(0, sinceSample));
  if (flight.status === "countdown") return { elapsed: Math.min(0, flight.elapsed_ms + drift), multiplier: 100 };
  if (flight.status !== "flying" || drift === 0) {
    return { elapsed: Math.max(0, flight.elapsed_ms), multiplier: flight.multiplier };
  }
  // Only the clock is interpolated. The server confirms every displayed multiplier.
  const elapsed = Math.min(ROCKET_MAX_DURATION, Math.max(0, flight.elapsed_ms + drift));
  return { elapsed, multiplier: flight.multiplier };
}

function rocketResponse(data) {
  if (data?.error) throw new Error(data.error);
  const flight = data?.flight;
  if (!Number.isFinite(data?.balance) || !Number.isFinite(data.min_bet) || !Number.isFinite(data.max_bet) ||
      (flight !== null && !(typeof flight?.id === "string" && Object.hasOwn(ROCKET_STATUS, flight.status) &&
        flight.status !== "idle" && Number.isFinite(flight.elapsed_ms) && Number.isFinite(flight.multiplier)))) {
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
    if (!repeat) return false;
    const { flight, balance, min_bet, max_bet } = snapshot;
    if (!canAutoStart()) clearRepeat("connection");
    else if (flight?.id !== repeat.id || flight.stake !== repeat.stake || flight.auto !== repeat.target) {
      clearRepeat("changed");
    } else if (flight.status === "refunded") clearRepeat("refunded");
    else if (!rocketActive(flight)) {
      if (repeat.stake >= min_bet && repeat.stake <= Math.min(balance, max_bet)) return true;
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
    const interval = rocketActive(snapshot?.flight) ? ROCKET_POLL_MS : ROCKET_IDLE_POLL_MS;
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
    const previous = snapshot?.flight;
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
        const ready = repeatReady();
        if (ready && !rocketActive(previous)) {
          repeat.remaining--;
          const { id, stake, target } = repeat;
          void mutate("start", { bet: String(stake), auto: target === null ? null : String(target / 100), expected_id: id }, repeat);
        } else {
          report();
          // Leave time to see the result and stop, then confirm it with a fresh poll.
          schedule(ready ? now() : startedAt);
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
      accept(await request("api/rocket/" + action, body));
      if (run && repeat === run) {
        const next = snapshot.flight;
        if (!next || next.id === run.id || next.stake !== run.stake || next.auto !== run.target) clearRepeat("changed");
        else if (next.status === "refunded") clearRepeat("refunded");
        else if (!run.remaining) clearRepeat("complete");
        else run.id = next.id;
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
    startRepeat(rounds) {
      if (!visible || !canAutoStart() || pending || needsSync || repeat ||
          !Number.isInteger(rounds) || rounds < 1 || rounds > ROCKET_MAX_REPEAT_ROUNDS) return;
      const flight = snapshot?.flight;
      if (!rocketActive(flight) || !Number.isSafeInteger(flight.stake) || flight.stake <= 0 ||
          (flight.auto !== null && (!Number.isInteger(flight.auto) || flight.auto < 101 || flight.auto > 999))) return;
      repeat = { id: flight.id, stake: flight.stake, target: flight.auto, remaining: rounds };
      repeatNotice = "";
      report();
    },
    stopRepeat(notice = "stopped") {
      if (clearRepeat(notice)) report();
    },
    async launch(bet, auto) {
      if (!visible || pending || needsSync || !snapshot || rocketActive(snapshot.flight)) return;
      if (!clearRepeat("stopped")) repeatNotice = "";
      await mutate("start", { bet, auto, expected_id: snapshot.flight?.id ?? null });
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
  const colors = { crashed: "#ff957b", paid: "#7af4c7", refunded: "#ffd690" };

  return function draw(status, { elapsed, multiplier: factor }, timestamp, transition, reducedMotion) {
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

    const left = width < 365 ? 45 : 57;
    const right = width - (width < 365 ? 22 : 30);
    const bottom = height - 34;
    const top = 25;
    const time = Math.max(0, elapsed);
    const timeLimit = Math.max(10500, time * 1.21);
    const factorLimit = Math.max(250, factor * 1.24);
    const xFor = (value) => left + (right - left) * value / timeLimit;
    const yFor = (value) => bottom - (bottom - top) * (value - 100) / (factorLimit - 100);
    const x = xFor(time);
    const y = yFor(factor);
    const color = colors[status] || "#bcabff";

    stars.forEach(([sx, sy], i) => {
      ctx.fillStyle = i % 5 ? "#b5a8ee34" : "#d5c9ff75";
      ctx.fillRect(sx * width, sy * (height - 12), i % 5 ? 1 : 1.6, i % 5 ? 1 : 1.6);
    });

    ctx.font = `${width < 365 ? 10 : 11}px ui-monospace, Consolas, monospace`;
    ctx.fillStyle = "#a59dbb";
    ctx.lineWidth = 1;
    ctx.strokeStyle = "#b5a1eb21";
    ctx.textAlign = "right";
    const factorStep = factorLimit >= 800 ? 200 : factorLimit >= 425 ? 100 : 50;
    ctx.beginPath();
    for (let value = 100; value <= factorLimit; value += factorStep) {
      const lineY = yFor(value);
      ctx.moveTo(left, lineY);
      ctx.lineTo(right, lineY);
      ctx.fillText((value / 100).toLocaleString("ru-RU", { minimumFractionDigits: 1 }) + "×", left - 8, lineY + 4);
    }
    ctx.textAlign = "center";
    const timeStep = timeLimit > 15500 ? 4000 : width < 365 ? 2500 : 2000;
    for (let value = 0; value <= timeLimit; value += timeStep) {
      const lineX = xFor(value);
      ctx.moveTo(lineX, top);
      ctx.lineTo(lineX, bottom);
      ctx.fillText((value / 1000).toLocaleString("ru-RU") + " с", lineX, bottom + 21);
    }
    ctx.stroke();

    if (time > 0) {
      const heightAtTime = rocketMultiplierAt(time) - 100;
      const curve = new Path2D();
      curve.moveTo(left, bottom);
      for (let i = 1; i <= 48; i++) {
        const t = time * i / 48;
        const gain = (rocketMultiplierAt(t) - 100) / heightAtTime;
        curve.lineTo(xFor(t), yFor(100 + (factor - 100) * gain));
      }
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
    }

    if (status === "idle" || status === "countdown" || status === "flying") {
      const ahead = yFor(rocketMultiplierAt(time + 90));
      const angle = Math.atan2(ahead - yFor(rocketMultiplierAt(time)), xFor(90) - xFor(0));
      drawRocketShip(ctx, x, y, Math.min(-.22, angle), timestamp, status === "flying", reducedMotion, color);
    } else {
      ctx.fillStyle = color;
      ctx.beginPath();
      ctx.arc(x, y, 4, 0, Math.PI * 2);
      ctx.fill();
      ctx.beginPath();
      ctx.strokeStyle = color;
      ctx.arc(x, y, 8, 0, Math.PI * 2);
      ctx.stroke();
      if (!reducedMotion && transition !== null && timestamp - transition < 760 && status !== "refunded") {
        drawRocketBurst(ctx, x, y, Math.max(0, (timestamp - transition) / 760), color, status === "crashed");
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
  let lastTelemetry = null;
  let fieldError = "";
  let connectionError = "";

  const repeatPanel = document.createElement("section");
  repeatPanel.className = "rocket-repeat";
  repeatPanel.setAttribute("aria-label", "Автостарт");
  const repeatSetup = document.createElement("div");
  repeatSetup.className = "rocket-repeat-setup";
  repeatSetup.hidden = true;
  const repeatLabel = document.createElement("label");
  repeatLabel.textContent = "Доп. раунды";
  repeatLabel.htmlFor = "rocket-repeat-rounds";
  const repeatRounds = document.createElement("select");
  repeatRounds.id = "rocket-repeat-rounds";
  repeatRounds.setAttribute("aria-describedby", "rocket-repeat-status");
  for (let count = 0; count <= ROCKET_MAX_REPEAT_ROUNDS; count++) {
    const option = document.createElement("option");
    option.value = count ? String(count) : "";
    option.textContent = count ? String(count) : "Выберите";
    repeatRounds.append(option);
  }
  const repeatStart = document.createElement("button");
  repeatStart.type = "button";
  repeatStart.className = "rocket-secondary";
  repeatStart.textContent = "Включить автостарт";
  repeatStart.disabled = true;
  repeatSetup.append(repeatLabel, repeatRounds, repeatStart);
  const repeatStop = document.createElement("button");
  repeatStop.type = "button";
  repeatStop.className = "rocket-secondary";
  repeatStop.textContent = "Остановить автостарт";
  repeatStop.hidden = true;
  const repeatStatus = document.createElement("p");
  repeatStatus.id = "rocket-repeat-status";
  repeatStatus.setAttribute("role", "status");
  repeatStatus.textContent = "Автостарт можно включить во время полёта.";
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
    const running = !motion.matches && !connectionError;
    const telemetry = rocketTelemetry(flight, running ? timestamp - current.sampledAt : 0);
    // A latency spike can make a new live sample trail the preceding frame slightly.
    if (running && rocketActive(flight) && lastTelemetry?.id === flight.id && lastTelemetry.status === status) {
      telemetry.elapsed = Math.max(lastTelemetry.elapsed, telemetry.elapsed);
      telemetry.multiplier = Math.max(lastTelemetry.multiplier, telemetry.multiplier);
    }
    lastTelemetry = { id: flight?.id, status, ...telemetry };
    write(multiplierLabel, displayFactor(telemetry.multiplier));
    write(clockLabel, status === "countdown" ? "До старта" : "Время полёта");
    write(clock, flight ? seconds.format(Math.abs(telemetry.elapsed) / 1000) + " с" : "—");
    draw(status, telemetry, timestamp, transition, !running);
    if (running && (rocketActive(flight) || (transition !== null && timestamp - transition < 760))) invalidate();
  }

  function maximum(data) {
    return Math.min(data.balance, data.max_bet);
  }

  function repeatTerms(stake, target, rounds) {
    const cashout = target === null ? "ручной вывод" : "автовывод " + displayFactor(target);
    const limit = rounds ? " Лимит ставок — " + number.format(stake * rounds) + " мон." : "";
    return number.format(stake) + " мон. за раунд, " + cashout + "." + limit;
  }

  function renderRepeat(flight, locked) {
    const { repeat, repeatNotice, pending } = current;
    const active = rocketActive(flight);
    repeatSetup.hidden = !active || Boolean(repeat);
    repeatStop.hidden = !repeat;
    repeatRounds.disabled = locked || !canAutoStart();
    repeatStart.disabled = repeatRounds.disabled || !repeatRounds.value;

    const notices = {
      stopped: "Автостарт остановлен.",
      connection: "Автостарт прерван. Для продолжения включите его заново.",
      changed: "Автостарт остановлен: раунд изменился.",
      refunded: "Автостарт остановлен: ставка возвращена.",
      stake: "Автостарт остановлен: ставка недоступна.",
      complete: "Автостарт завершён. Лимит раундов исчерпан.",
    };
    let status = notices[repeatNotice] || "Автостарт можно включить во время полёта.";
    if (repeat) {
      const progress = pending === "start" ?
        "Запускаем раунд. После него осталось: " : "Автостарт включён. Осталось раундов: ";
      status = progress + repeat.remaining + ". " + repeatTerms(repeat.stake, repeat.target, repeat.remaining);
    } else if (active && repeatRounds.value) {
      status = repeatTerms(flight.stake, flight.auto, Number(repeatRounds.value));
    } else if (active && !repeatNotice) {
      status = repeatTerms(flight.stake, flight.auto, 0);
    }
    write(repeatStatus, status);
  }

  function renderControls() {
    const { snapshot, pending, needsSync, syncing, syncError, actionError } = current;
    const flight = snapshot?.flight;
    const status = flight?.status || "idle";
    const active = rocketActive(flight);
    const paid = flight?.payout != null && status !== "refunded";
    const lowBalance = snapshot && maximum(snapshot) < snapshot.min_bet;
    const locked = Boolean(connectionError || pending || needsSync || !snapshot);
    const actionEnabled = !locked && (rocketCanCashout(flight) || (!active && !lowBalance));
    root.dataset.phase = status;
    root.dataset.cashedOut = String(paid);
    write(phaseLabel, connectionError || (!snapshot && syncError) ? "Нет соединения" :
      snapshot ? ROCKET_STATUS[status] : "Подключение");
    write(multiplierCaption, status === "crashed" ? "Пик полёта" : "Коэффициент");
    write(balance, snapshot ? number.format(snapshot.balance) + " мон." : "—");
    settings.hidden = active;
    settings.disabled = locked || active || lowBalance;
    action.setAttribute("aria-disabled", String(!actionEnabled));
    const actionText = pending === "start" ? "Готовим запуск…" : pending === "cashout" ? "Забираем монеты…" :
      connectionError ? "Соединение закрыто" : needsSync ? "Подключение…" :
      status === "countdown" ? "Ждём старта…" : status === "flying" ? (paid ? "Ждём конца полёта…" : "Забрать монеты") :
      lowBalance ? "Недостаточно монет" : "Запустить ракету";
    write(action, actionText);
    retry.hidden = !syncError || Boolean(connectionError);
    retry.disabled = Boolean(syncing || pending);
    renderRepeat(flight, locked);

    get("flight").hidden = !flight;
    if (flight) {
      write(get("stake"), number.format(flight.stake) + " мон.");
      write(get("target"), flight.auto == null ? "Выключен" : displayFactor(flight.auto));
      get("payout-row").hidden = flight.payout == null;
      const payoutLabel = status === "refunded" ? "Возврат" : flight.automatic ? "Автовыплата" : "Выплата";
      const cashoutMultiplier = flight.cashout_multiplier ?? (status === "paid" ? flight.multiplier : null);
      write(get("payout-label"), cashoutMultiplier == null ? payoutLabel : payoutLabel + " · " + displayFactor(cashoutMultiplier));
      if (flight.payout != null) write(get("payout"), number.format(flight.payout) + " мон.");
    }

    if (snapshot) {
      if (!bet.value && !bet.dataset.edited) bet.value = String(snapshot.min_bet);
      write(get("bet-hint"), lowBalance ? "Минимальная ставка — " + number.format(snapshot.min_bet) + " мон." :
        number.format(snapshot.min_bet) + "–" + number.format(maximum(snapshot)) + " мон.");
    }

    const result = {
      idle: "Укажите ставку перед стартом.",
      countdown: "Вывод станет доступен после старта.",
      flying: paid ? (flight.automatic ? "Автовывод сработал. Полёт продолжается." :
        "Монеты зачислены. Полёт продолжается.") : "Вывод подтверждает сервер в момент запроса.",
      paid: flight?.automatic ? "Автовывод сработал." : "Монеты зачислены на баланс.",
      crashed: paid ? "Ракета разбилась. Выплата уже на балансе." : "Ракета потеряна вместе со ставкой.",
      refunded: "Монеты возвращены на баланс.",
    };
    const error = connectionError || syncError || fieldError || actionError;
    message.dataset.tone = error ? "error" : status;
    write(message, error || (needsSync ? "Обновляем данные полёта…" : result[status]));
  }

  function update(view) {
    const previous = current.snapshot?.flight;
    const next = view.snapshot?.flight;
    if (next?.id !== previous?.id) repeatRounds.value = "";
    if (next?.id !== previous?.id || next?.status !== previous?.status) {
      transition = shown && rocketActive(previous) && previous.id === next?.id && !rocketActive(next) ?
        performance.now() : null;
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

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    if (action.getAttribute("aria-disabled") === "true") return;
    const data = current.snapshot;
    if (rocketCanCashout(data.flight)) {
      feed.cashout();
      return;
    }
    const stake = bet.value.trim().replace(/\s+/g, "");
    if (!/^\d+$/.test(stake) || Number(stake) < data.min_bet || Number(stake) > maximum(data)) {
      invalid(bet, "Введите целое число монет от " + number.format(data.min_bet) + " до " + number.format(maximum(data)) + ".");
      return;
    }
    const target = auto.value.trim().replace(",", ".");
    if (target && (!/^[1-9](?:\.\d{1,2})?$/.test(target) || Number(target) < 1.01)) {
      invalid(auto, "Введите коэффициент от 1,01× до 9,99× или оставьте поле пустым.");
      return;
    }
    clearFieldError();
    feed.launch(stake, target || null);
  });

  bet.addEventListener("input", () => {
    bet.dataset.edited = "true";
    clearFieldError();
  });
  auto.addEventListener("input", clearFieldError);
  for (const shortcut of ["min", "max"]) {
    get(shortcut).addEventListener("click", () => {
      const data = current.snapshot;
      bet.value = String(shortcut === "min" ? data.min_bet : maximum(data));
      bet.dataset.edited = "true";
      clearFieldError();
    });
  }
  retry.addEventListener("click", feed.sync);
  repeatRounds.addEventListener("change", renderControls);
  repeatStart.addEventListener("click", () => {
    const rounds = Number(repeatRounds.value);
    repeatRounds.value = "";
    feed.startRepeat(rounds);
  });
  repeatStop.addEventListener("click", () => feed.stopRepeat());

  function suspend() {
    repeatRounds.value = "";
    feed.hide();
    cancelAnimationFrame(frame);
    frame = null;
    lastTelemetry = null;
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
  window.addEventListener("online", renderControls);
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
