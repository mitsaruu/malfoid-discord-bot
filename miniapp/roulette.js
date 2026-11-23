(window.CasinoGames ||= {}).roulette = function ({ root, request, avatar }) {
  const wheelOrder = [0, 32, 15, 19, 4, 21, 2, 25, 17, 34, 6, 27, 13, 36, 11, 30, 8, 23, 10,
    5, 24, 16, 33, 1, 20, 14, 31, 9, 22, 18, 29, 7, 28, 12, 35, 3, 26];
  const red = new Set([1, 3, 5, 7, 9, 12, 14, 16, 18, 19, 21, 23, 25, 27, 30, 32, 34, 36]);
  const tone = (number) => number === 0 ? "green" : red.has(number) ? "red" : "black";
  const colors = { green: "#247f51", red: "#ad3545", black: "#282b36" };
  const colorNames = { green: "Зеро", red: "Красное", black: "Чёрное" };
  const format = new Intl.NumberFormat("ru");
  const signed = new Intl.NumberFormat("ru", { signDisplay: "exceptZero" });
  const avatars = window.createGameAvatars?.(avatar);
  const reduceMotion = matchMedia("(prefers-reduced-motion: reduce)");
  const pocketAngle = 360 / wheelOrder.length;
  const launchRadius = 49;
  const trackRadius = 45.4;
  const pocketRadius = 34.2;
  const pocketInterval = 1000;
  const spinDuration = 2800;
  const tickSpacing = 75;
  const outcomeDelay = 180;
  const launchAngle = pocketAngle * .65;
  const easeOut = (progress) => 1 - (1 - progress) ** 3;

  function element(tag, text = "", className = "") {
    const node = document.createElement(tag);
    node.textContent = text;
    node.className = className;
    return node;
  }

  const view = element("div", "", "roulette-view");
  const header = element("header", "", "roulette-header");
  const title = element("div");
  title.append(element("h1", "Рулетка"), element("p", "Общий стол сервера"));
  const wallet = element("strong", "", "roulette-wallet");
  header.append(title, wallet);

  const wheel = element("canvas", "", "roulette-wheel");
  wheel.width = wheel.height = 900;
  wheel.setAttribute("aria-hidden", "true");
  const ball = element("span", "", "roulette-ball");
  ball.setAttribute("aria-hidden", "true");
  ball.hidden = true;
  const wheelFrame = element("div", "", "roulette-wheel-frame");
  wheelFrame.append(wheel, ball);
  const resultNumber = element("strong", "—", "roulette-result-number");
  const resultLabel = element("span", "Ожидаем первый результат");
  const resultReadout = element("div", "", "roulette-readout");
  resultReadout.setAttribute("aria-live", "polite");
  resultReadout.append(resultNumber, resultLabel);
  const wheelPanel = element("div", "", "roulette-wheel-panel");
  wheelPanel.append(wheelFrame, resultReadout);

  const betsPanel = element("section", "", "roulette-bets-panel");
  const betsTitle = element("h2", "Ставки раунда");
  betsTitle.id = "roulette-bets-title";
  betsPanel.setAttribute("aria-labelledby", betsTitle.id);
  const betsEmpty = element("p", "Ставок пока нет", "roulette-bets-empty");
  const betsList = element("ul", "", "roulette-bets");
  betsPanel.append(betsTitle, betsEmpty, betsList);

  const table = element("section", "", "roulette-table");
  table.setAttribute("aria-label", "Стол ставок");
  const tableHeader = element("div", "", "roulette-table-header");
  const tableState = element("strong", "Загружаем стол…");
  const tableCount = element("p");
  const tableInfo = element("div");
  tableInfo.append(tableState, tableCount);
  const clock = element("time", "", "roulette-clock");
  tableHeader.append(tableInfo, clock);
  const board = element("div", "", "roulette-board");
  const cells = new Map();
  const labels = new Map();
  let selected = null;
  let visible = false;
  let pageSuspended = false;
  let generation = 0;
  let pollTimer;
  let tickTimer;
  let state = null;
  let closesAt = 0;
  let sending = false;
  let pending = null;
  let syncError = "";
  let feedbackText = "";
  let feedbackTone = "";
  let shownResult = null;
  let renderedResultKey;
  let renderedBetsKey;
  let rotation = 0;
  let ballAngle = 0;
  let ballRadius = trackRadius;
  let animationFrame = null;
  let idle = null;
  let spin = null;
  let lastTickAt = -Infinity;

  function addCell(key, label, column, row, mobileColumn, mobileRow, color = "", type = "group") {
    const control = element("button", label, "roulette-cell roulette-cell--" + type);
    control.type = "button";
    control.dataset.tone = color;
    control.style.setProperty("--column", column);
    control.style.setProperty("--row", row);
    control.style.setProperty("--mobile-column", mobileColumn);
    control.style.setProperty("--mobile-row", mobileRow);
    control.addEventListener("click", () => {
      selected = key;
      feedbackText = "";
      renderControls();
      renderFeedback();
    });
    labels.set(key, label);
    cells.set(key, control);
    board.append(control);
  }

  addCell("0", "0", "1", "1 / span 3", "1 / -1", "1", "green", "zero");
  for (let number = 1; number <= 36; number++) {
    const index = number - 1;
    addCell(String(number), String(number), String(2 + Math.floor(index / 3)), String(3 - index % 3),
      String(1 + index % 6), String(2 + Math.floor(index / 6)), tone(number), "number");
  }
  ["1st", "2nd", "3rd"].forEach((key, index) => {
    addCell(key, (index + 1) + "-я кол.", "14", String(3 - index), (1 + index * 2) + " / span 2", "8");
  });
  ["1-12", "13-24", "25-36"].forEach((key, index) => {
    addCell(key, key.replace("-", "–"), (2 + 4 * index) + " / span 4", "4", (1 + index * 2) + " / span 2", "9");
  });
  const outside = [
    ["1-18", "1–18"], ["even", "Чётное"], ["red", "Красное", "red"],
    ["black", "Чёрное", "black"], ["odd", "Нечётное"], ["19-36", "19–36"],
  ];
  outside.forEach(([key, label, color], index) => {
    addCell(key, label, (2 + 2 * index) + " / span 2", "5",
      (1 + (index % 3) * 2) + " / span 2", String(10 + Math.floor(index / 3)), color);
  });

  const form = element("form", "", "roulette-form");
  const amountLabel = element("label", "Ставка в монетах");
  const amount = element("input");
  amount.type = "text";
  amount.inputMode = "numeric";
  amount.maxLength = 12;
  amount.autocomplete = "off";
  amount.required = true;
  amountLabel.append(amount);
  const all = element("button", "Максимум", "roulette-secondary");
  all.type = "button";
  all.addEventListener("click", () => {
    amount.value = String(Math.min(state.balance, state.max_bet));
  });
  const submit = element("button", "Поставить", "roulette-submit");
  submit.type = "submit";
  const actions = element("div", "", "roulette-actions");
  actions.append(amountLabel, all, submit);
  const selection = element("p", "", "roulette-selection");
  form.append(selection, actions);
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    void submitBet();
  });
  const feedback = element("p", "", "roulette-feedback");
  feedback.setAttribute("role", "status");
  const own = element("p", "", "roulette-own");
  table.append(tableHeader, board, form, feedback, own);

  const layout = element("div", "", "roulette-layout");
  layout.append(betsPanel, wheelPanel, table);
  const results = element("section", "", "roulette-panel");
  results.hidden = true;
  const resultsList = element("ul", "", "roulette-results");
  results.append(element("h2", "Итоги раунда"), resultsList);
  const rules = element("details", "", "roulette-panel");
  rules.append(
    element("summary", "Правила и выплаты"),
    element("p", "Ставка открывает раунд. После каждой принятой ставки остаётся до 15 секунд, весь раунд длится не больше минуты."),
    element("p", "Цвет, чётность и половина поля — ×2. Дюжина и колонка — ×3. Точное число — ×36."),
    element("p", "Выплаты включают поставленные монеты. При выпадении зеро выигрывают только ставки на 0."),
  );
  view.append(header, layout, results, rules);
  root.replaceChildren(view);

  function drawWheel() {
    const ctx = wheel.getContext("2d");
    if (!ctx) return;
    const step = pocketAngle * Math.PI / 180;
    ctx.translate(450, 450);
    ctx.scale(1.5, 1.5);

    function disk(radius, color) {
      ctx.beginPath();
      ctx.arc(0, 0, radius, 0, 2 * Math.PI);
      ctx.fillStyle = color;
      ctx.fill();
    }

    disk(294, "#362c25");
    disk(288, "#a7855e");
    disk(277, "#222521");
    disk(268, "#554a3c");
    disk(258, "#181d1b");
    wheelOrder.forEach((number, index) => {
      const angle = -Math.PI / 2 + index * step;
      ctx.beginPath();
      ctx.moveTo(0, 0);
      ctx.arc(0, 0, 251, angle - step / 2, angle + step / 2);
      ctx.closePath();
      ctx.fillStyle = colors[tone(number)];
      ctx.fill();
      ctx.strokeStyle = "#e3d3ae80";
      ctx.lineWidth = 1.3;
      ctx.stroke();
      ctx.save();
      ctx.rotate(angle + Math.PI / 2);
      ctx.fillStyle = "#fff6e8";
      ctx.font = "700 20px system-ui, sans-serif";
      ctx.textAlign = "center";
      ctx.textBaseline = "middle";
      ctx.fillText(number, 0, -224);
      ctx.restore();
    });
    disk(194, "#ac8964");
    disk(191, "#222926");
    disk(170, "#78644f");
    const inner = ctx.createRadialGradient(0, -18, 9, 0, 0, 166);
    inner.addColorStop(0, "#725c43");
    inner.addColorStop(1, "#362f29");
    disk(167, inner);
    disk(56, "#312d26");
    disk(49, "#b09168");
    disk(40, "#4a3e30");
    disk(19, "#a7855e");
    disk(12, "#d5b68f");
  }
  drawWheel();

  function renderResult(result) {
    const key = result?.id ?? (spin ? "spinning" : state?.round ? "betting" : null);
    if (key === renderedResultKey) return;
    renderedResultKey = key;
    if (Number.isInteger(result?.number)) {
      const color = tone(result.number);
      resultNumber.textContent = result.number;
      resultNumber.dataset.tone = color;
      resultLabel.textContent = colorNames[color];
    } else {
      resultNumber.textContent = "—";
      delete resultNumber.dataset.tone;
      resultLabel.textContent = result ? "Ставки возвращены" :
        spin ? "Колесо вращается" : "Ожидаем результат";
    }
    results.hidden = !result;
    resultsList.replaceChildren(...(result?.players ?? []).map((player) => {
      const row = element("li");
      const net = element("strong", signed.format(player.net));
      net.dataset.net = player.net > 0 ? "win" : player.net < 0 ? "loss" : "even";
      row.append(element("span", player.name + (player.id === state?.user_id ? " (вы)" : "")), net);
      return row;
    }));
  }

  function renderBets(result) {
    const round = state?.round ?? spin?.result ?? result;
    const bets = round?.bets ?? [];
    // The server appends stakes in placement order and settles each round once.
    const key = round ? round.id + ":" + bets.length + ":" + Boolean(result) : null;
    if (key === renderedBetsKey) return;
    renderedBetsKey = key;
    betsEmpty.hidden = bets.length > 0;
    betsList.replaceChildren(...bets.map((bet) => {
      const row = element("li");
      const identity = element("div", "", "roulette-bet-identity");
      const name = bet.name + (bet.user_id === state.user_id ? " (вы)" : "");
      identity.append(
        element("strong", name, "roulette-bet-name"),
        element("span", labels.get(bet.space) ?? bet.space, "roulette-bet-position"),
      );
      const amounts = element("div", "", "roulette-bet-values");
      amounts.append(element("strong", format.format(bet.amount), "roulette-bet-amount"));
      if (result && Number.isFinite(bet.payout)) {
        const payout = element("span",
          (result.refunded ? "Возврат " : "Выплата ") + format.format(bet.payout), "roulette-bet-payout");
        payout.dataset.win = String(!result.refunded && bet.payout > 0);
        amounts.append(payout);
      }
      const image = avatars?.image(bet.user_id, bet.name);
      if (image) row.append(image);
      row.append(identity, amounts);
      return row;
    }));
  }

  function rotateWheel(angle, now) {
    if (angle === rotation) return;
    const previousPocket = Math.floor(rotation / pocketAngle + 1e-9);
    rotation = angle;
    wheel.style.transform = "rotate(" + angle + "deg)";
    // Sample the painted angle and drop crossings that are too close or share a frame.
    if (now !== undefined && now - lastTickAt >= tickSpacing &&
        Math.floor(angle / pocketAngle + 1e-9) > previousPocket) {
      lastTickAt = now;
      window.GameAudio?.play?.("roulette.tick");
    }
  }

  function placeBall(angle, radius) {
    if (!ball.hidden && angle === ballAngle && radius === ballRadius) return;
    ballAngle = angle;
    ballRadius = radius;
    const radians = angle * Math.PI / 180;
    ball.style.left = (50 + radius * Math.sin(radians)) + "%";
    ball.style.top = (50 - radius * Math.cos(radians)) + "%";
    ball.hidden = false;
  }

  function canAnimate() {
    return visible && !pageSuspended && !document.hidden;
  }

  function pauseWheel() {
    if (animationFrame !== null) cancelAnimationFrame(animationFrame);
    animationFrame = null;
    idle = null;
  }

  function startIdleWheel() {
    if (!canAnimate() || spin || animationFrame !== null) return;
    idle = { startedAt: performance.now(), from: rotation, pocket: 0 };
    animationFrame = requestAnimationFrame(animateWheel);
  }

  function winningRotation(result) {
    return (360 - wheelOrder.indexOf(result.number) * pocketAngle) % 360;
  }

  function finishResult(result, audible) {
    pauseWheel();
    spin = null;
    rotateWheel(winningRotation(result));
    placeBall(0, pocketRadius);
    shownResult = result;
    if (audible) {
      window.GameAudio?.play?.("roulette.land");
      const net = result.players?.find((player) => player.id === state?.user_id)?.net;
      if (Number.isFinite(net) && net !== 0) {
        window.GameAudio?.play?.(net > 0 ? "roulette.win" : "roulette.loss", outcomeDelay);
      }
    }
    startIdleWheel();
  }

  function suspendMotion() {
    pauseWheel();
    if (spin) {
      finishResult(spin.result, false);
      render();
    }
  }

  function animateWheel(now) {
    animationFrame = null;
    if (!canAnimate()) return;
    if (spin) {
      const { result, startedAt, from, to } = spin;
      const progress = Math.min(1, Math.max(0, now - startedAt) / spinDuration);
      if (progress === 1) {
        finishResult(result, true);
        render();
        return;
      }
      const turn = easeOut(progress);
      const launch = easeOut(Math.min(1, progress / .11));
      const drop = Math.max(0, (progress - .82) / .18) ** 2;
      rotateWheel(from + (to - from) * turn, now);
      placeBall(launchAngle + (-3 * 360 - launchAngle) * turn,
        launchRadius + (trackRadius - launchRadius) * launch + (pocketRadius - trackRadius) * drop);
    } else {
      const elapsed = Math.max(0, now - idle.startedAt);
      const position = elapsed / pocketInterval;
      const pocket = Math.floor(position);
      const angle = idle.from + (reduceMotion.matches ? pocket : position) * pocketAngle;
      rotateWheel(angle, now);
      if (pocket > idle.pocket) ball.hidden = true;
      if (!ball.hidden) placeBall(angle - winningRotation(shownResult), pocketRadius);
      idle.pocket = pocket;
    }
    animationFrame = requestAnimationFrame(animateWheel);
  }

  function updateResult(result, live) {
    if (!result || result.id === shownResult?.id || result.id === spin?.result.id) return;
    if (result.refunded || result.number === null || !live) {
      if (spin) {
        pauseWheel();
        spin = null;
        startIdleWheel();
      }
      ball.hidden = true;
      shownResult = result;
      return;
    }
    if (!canAnimate() || reduceMotion.matches) {
      finishResult(result, canAnimate());
      return;
    }
    // Rotation is the last angle written to the canvas, including a partial idle step.
    pauseWheel();
    const from = rotation;
    const to = from + (winningRotation(result) - from % 360 + 360) % 360 + 3 * 360;
    placeBall(launchAngle, launchRadius);
    window.GameAudio?.play?.("roulette.launch");
    spin = { result, startedAt: performance.now(), from, to };
    animationFrame = requestAnimationFrame(animateWheel);
  }

  reduceMotion.addEventListener("change", () => {
    if (!canAnimate()) return;
    if (spin) {
      if (reduceMotion.matches) {
        finishResult(spin.result, true);
        render();
      }
    } else {
      pauseWheel();
      startIdleWheel();
    }
  });
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) suspendMotion();
    else startIdleWheel();
  });
  window.addEventListener("pagehide", () => {
    pageSuspended = true;
    suspendMotion();
  });
  window.addEventListener("pageshow", () => {
    pageSuspended = false;
    startIdleWheel();
  });

  function renderFeedback() {
    const message = (pending && !sending ? "Ответ на ставку потерян. Повторите отправку, чтобы проверить ту же ставку." : "")
      || syncError || feedbackText
      || (state && state.balance < state.min_bet ? "В кошельке меньше минимальной ставки." : "");
    if (feedback.textContent !== message) feedback.textContent = message;
    feedback.dataset.tone = (pending || syncError) ? "error" : feedbackTone;
  }

  function renderControls() {
    const locked = !state || Boolean(syncError) || sending || Boolean(spin) ||
      Boolean(state.round && (performance.now() >= closesAt || state.round.bet_count >= state.max_bets)) ||
      state.balance < state.min_bet;
    const totals = new Map();
    for (const bet of state?.round?.your_bets ?? []) {
      totals.set(bet.space, (totals.get(bet.space) ?? 0) + bet.amount);
    }
    for (const [key, control] of cells) {
      const total = totals.get(key);
      control.setAttribute("aria-pressed", String(key === selected));
      control.classList.toggle("roulette-cell--wagered", total !== undefined);
      control.setAttribute("aria-label", labels.get(key) + (total ? " · поставлено " + format.format(total) : ""));
      control.disabled = sending || Boolean(pending);
    }
    const multiplier = selected === null ? "" : /^\d+$/.test(selected) ? "×36" :
      ["1st", "2nd", "3rd", "1-12", "13-24", "25-36"].includes(selected) ? "×3" : "×2";
    selection.textContent = selected === null ? "Выберите клетку" : labels.get(selected) + " · " + multiplier;
    amount.disabled = all.disabled = locked || Boolean(pending);
    submit.disabled = sending || (pending ? Boolean(syncError) : locked || selected === null);
    submit.textContent = sending ? "Отправляем…" : pending ? "Проверить ставку" : "Поставить";
    own.textContent = totals.size ? "Ваши ставки · " +
      [...totals].map(([key, total]) => labels.get(key) + " — " + format.format(total)).join(" · ") : "";
  }

  function tick() {
    const round = state?.round;
    const seconds = round ? Math.max(0, Math.ceil((closesAt - performance.now()) / 1000)) : 0;
    const text = seconds ? "0:" + String(seconds).padStart(2, "0") : "";
    if (clock.textContent !== text) clock.textContent = text;
    const status = !state ? (syncError ? "Стол недоступен" : "Загружаем стол…") :
      round?.refunding ? "Возвращаем ставки" :
      spin || (round && !seconds) ? "Подводим итоги" :
      round ? "Ставки открыты" : "Стол свободен";
    if (tableState.textContent !== status) {
      tableState.textContent = status;
      renderControls();
    }
  }

  function render() {
    const result = !state?.round && !spin && shownResult?.id === state?.last_result?.id ? shownResult : null;
    renderResult(result);
    renderBets(result);
    if (!spin) wallet.textContent = state ? "Баланс " + format.format(state.balance) : "";
    tableCount.textContent = state?.round
      ? "Игроки " + state.round.player_count + " · Ставки " + state.round.bet_count + " / " + state.max_bets
      : state ? "Первая ставка открывает раунд" : "";
    if (state && !amount.value) amount.value = state.min_bet;
    tick();
    renderControls();
    renderFeedback();
  }

  function accept(data) {
    if (typeof data?.round_id !== "string" || !Number.isFinite(data.balance)) {
      throw new Error("Сервер вернул неполные данные стола.");
    }
    const openRound = state?.round?.id;
    state = data;
    closesAt = performance.now() + (data.round?.remaining_ms ?? 0);
    syncError = "";
    const source = pending && [data.round, data.last_result].find((round) => round?.id === pending.round_id);
    const receipt = source?.your_bets?.find((bet) => bet.request_id === pending.request_id);
    updateResult(data.last_result, data.last_result?.id === openRound || (receipt && source === data.last_result));
    if (receipt) {
      pending = null;
      feedbackText = source.refunded ? "Ставка возвращена." :
        "Ставка принята · " + format.format(receipt.amount) + " · " + labels.get(receipt.space);
      feedbackTone = "ok";
    }
  }

  function schedule() {
    clearTimeout(pollTimer);
    if (visible) pollTimer = setTimeout(refresh, state?.round ? 800 : 1400);
  }

  async function refresh() {
    if (!visible || sending) return;
    const token = ++generation;
    try {
      const data = await request("state");
      if (token === generation) accept(data);
    } catch (error) {
      if (token === generation) syncError = error.message || "Не удалось обновить стол.";
    } finally {
      if (token === generation) {
        render();
        schedule();
      }
    }
  }

  async function submitBet() {
    if (!visible || submit.disabled) return;
    if (!pending) {
      const value = amount.value.trim();
      if (!/^\d+$/.test(value) || Number(value) < state.min_bet || Number(value) > Math.min(state.balance, state.max_bet)) {
        feedbackTone = "error";
        feedbackText = "Введите сумму от " + format.format(state.min_bet) + " до " +
          format.format(Math.min(state.balance, state.max_bet)) + " монет.";
        renderFeedback();
        return;
      }
      pending = { round_id: state.round_id, request_id: crypto.randomUUID(), bet: value, space: selected };
    }
    clearTimeout(pollTimer);
    const token = ++generation;
    sending = true;
    feedbackText = "";
    render();
    try {
      const data = await request("bet", pending);
      if (token === generation) {
        accept(data);
        if (typeof data.bet_error === "string") {
          pending = null;
          feedbackText = data.bet_error;
          feedbackTone = "error";
        }
      }
    } catch {
      // The server might have committed before the connection dropped. Keep the same request ID.
    } finally {
      sending = false;
      if (visible) {
        render();
        if (pending || token !== generation) void refresh();
        else schedule();
      }
    }
  }

  return {
    show() {
      if (visible) return;
      visible = true;
      render();
      startIdleWheel();
      tickTimer = setInterval(tick, 120);
      void refresh();
    },
    hide() {
      visible = false;
      generation++;
      clearTimeout(pollTimer);
      clearInterval(tickTimer);
      suspendMotion();
      ball.hidden = true;
      state = null;
      syncError = "";
      avatars?.clear();
      render();
    },
  };
};
