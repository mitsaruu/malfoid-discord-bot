"use strict";

(window.CasinoGames ||= {}).slots = function ({ root, request, portraitUrl = "static/slots-portrait.png" }) {
  function element(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text) node.textContent = text;
    return node;
  }

  function button(text, className, type = "button") {
    const node = element("button", className, text);
    node.type = type;
    return node;
  }

  function paintIcon(node, icon) {
    if (icon === "portrait") {
      if (node.classList.contains("slots-icon--portrait")) return;
      const image = element("img");
      image.src = portraitUrl;
      image.alt = "";
      node.replaceChildren(image);
    } else {
      setText(node, icon);
    }
    node.classList.toggle("slots-icon--portrait", icon === "portrait");
  }

  function symbolIcon(icon) {
    const node = element("span", "slots-icon");
    paintIcon(node, icon);
    return node;
  }

  const number = new Intl.NumberFormat("ru-RU");
  const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
  const counts = [1, 2, 3];
  const repeatCounts = [1, 3, 5, 10, 15, 20, 25, 30];
  const rewardLevels = [
    { min: 100, effect: "gold", pause: 4300 },
    { min: 51, effect: "violet", pause: 3400 },
    { min: 26, effect: "blue", pause: 2600 },
    { min: 11, effect: "green", pause: 2100 },
  ];
  let boardCount = 1;
  let repeatCount = 1;
  const title = element("h1", "", "Слоты");
  title.id = "slots-title";
  root.setAttribute("aria-labelledby", title.id);
  const wallet = element("p", "casino-balance");
  const header = element("header", "casino-header slots-header");
  const identity = element("div", "slots-identity");
  identity.append(title, wallet);
  const settings = element("div", "slots-settings");
  const boardSwitch = element("div", "slots-board-switch");
  boardSwitch.setAttribute("role", "group");
  boardSwitch.setAttribute("aria-label", "Наборы барабанов");
  const boardChoices = counts.map((count) => {
    const choice = button(String(count), "slots-board-choice");
    choice.dataset.count = String(count);
    choice.setAttribute("aria-label", `Наборов барабанов: ${count}`);
    choice.addEventListener("click", () => {
      if (choice.disabled || count === boardCount) return;
      boardCount = count;
      selectBoards();
      render();
    });
    return choice;
  });
  boardSwitch.append(...boardChoices);

  const repeatPicker = element("div", "slots-repeat-picker");
  const repeats = button("1 запуск", "slots-repeats");
  const repeatMenu = element("div", "slots-repeat-menu");
  repeatMenu.id = "slots-repeat-menu";
  repeatMenu.setAttribute("role", "group");
  repeatMenu.setAttribute("aria-label", "Количество запусков");
  repeatMenu.hidden = true;
  repeats.setAttribute("aria-controls", repeatMenu.id);
  repeats.setAttribute("aria-expanded", "false");
  const repeatChoices = repeatCounts.map((count) => {
    const choice = button(count === 1 ? "1 запуск" : String(count), "slots-repeat-choice");
    choice.dataset.count = String(count);
    choice.addEventListener("click", () => {
      if (repeats.disabled || repeatMenu.hidden) return;
      repeatCount = count;
      showRepeatMenu(false);
      repeats.focus();
      render();
    });
    return choice;
  });
  repeatMenu.append(...repeatChoices);
  repeatPicker.append(repeats, repeatMenu);
  settings.append(boardSwitch, repeatPicker);
  header.append(identity, settings);
  const panel = element("section", "slots-cabinet");
  const dock = element("div", "casino-dock slots-dock");
  const form = element("form", "casino-toolbar");
  form.id = "slots-form";
  const betLabel = element("label");
  const betCaption = element("span", "", "Ставка");
  const bet = element("input");
  bet.type = "text";
  bet.inputMode = "numeric";
  bet.autocomplete = "off";
  bet.maxLength = 20;
  bet.required = true;
  betLabel.append(betCaption, bet);
  const maximum = button("Макс.");
  const spin = button("Вращать", "casino-button", "submit");
  const stop = button("Остановить серию", "slots-stop");
  stop.hidden = true;
  const actions = element("div", "slots-actions");
  actions.append(spin, stop);
  form.append(betLabel, maximum, actions);
  const lever = button("", "slots-lever", "submit");
  lever.setAttribute("form", form.id);
  const leverArm = element("span", "slots-lever-arm");
  leverArm.setAttribute("aria-hidden", "true");
  leverArm.append(element("span", "slots-lever-shaft"), element("span", "slots-lever-knob"));
  lever.append(leverArm, element("span", "slots-lever-pivot"));
  const cost = element("p", "slots-cost");
  const reelBoards = element("div", "slots-boards");
  const boards = [];

  function createBoard(index) {
    const container = element("section", "slots-board");
    const heading = element("h2", "", `Набор ${index + 1}`);
    const grid = element("div", "slots-reels");
    grid.setAttribute("role", "group");
    grid.setAttribute("aria-label", `Набор ${index + 1}, пять барабанов по три символа`);
    const reels = Array.from({ length: 5 }, (_, column) => {
      const frame = element("div", "slots-reel");
      const strip = element("div", "slots-strip");
      strip.style.position = "relative";
      strip.setAttribute("role", "group");
      strip.setAttribute("aria-label", `Барабан ${column + 1}`);
      const cells = Array.from({ length: 3 }, () => {
        const cell = element("span", "slots-symbol");
        const icon = symbolIcon("·");
        cell.setAttribute("role", "img");
        cell.append(icon);
        strip.append(cell);
        return { cell, icon };
      });
      frame.append(strip);
      grid.append(frame);
      return { strip, cells };
    });
    const result = element("p", "slots-board-result");
    const wins = element("ul", "slots-wins");
    const summary = element("div", "slots-board-summary");
    summary.append(result, wins);
    container.append(heading, grid, summary);
    return { container, heading, grid, reels, result, wins };
  }

  let activeBoards = [];
  function selectBoards() {
    const count = boardCount;
    while (boards.length < count) boards.push(createBoard(boards.length));
    activeBoards = boards.slice(0, count);
    reelBoards.classList.toggle("slots-boards--multiple", count > 1);
    reelBoards.classList.toggle("slots-boards--three", count === 3);
    reelBoards.replaceChildren(...activeBoards.map(({ container, heading }) => {
      heading.hidden = count === 1;
      return container;
    }));
  }
  selectBoards();

  const feedback = element("p", "casino-feedback");
  feedback.setAttribute("role", "status");
  const retry = button("Проверить последний результат");
  dock.append(form, cost, feedback, retry);
  panel.append(dock, reelBoards);

  const rules = element("section", "casino-panel slots-rules");
  const linesHeading = element("h2", "", "Линии выплат");
  const linePicker = element("div", "casino-toolbar slots-lines");
  linePicker.setAttribute("role", "group");
  linePicker.setAttribute("aria-label", "Подсветка линии на барабанах");
  const explanation = element("p", "", "Три и более одинаковых символа подряд от левого края дают выплату по линии. Каждый множитель применяется к полной ставке. Выплаты по линиям складываются.");
  const paytable = element("table", "slots-paytable");
  paytable.setAttribute("aria-label", "Таблица выплат");
  rules.append(linesHeading, linePicker, explanation, element("h2", "", "Таблица выплат"), paytable);
  const payoutBar = element("footer", "slots-payout");
  payoutBar.setAttribute("role", "status");
  payoutBar.setAttribute("aria-atomic", "true");
  const lastPayout = element("p", "slots-payout-last");
  const seriesTotal = element("p", "slots-payout-series");
  payoutBar.append(lastPayout, seriesTotal);
  const winOverlay = element("div", "slots-win-overlay");
  const winAmount = element("p", "slots-win-amount");
  winOverlay.setAttribute("aria-hidden", "true");
  winOverlay.hidden = true;
  winOverlay.append(winAmount);
  winAmount.addEventListener("animationend", () => { winOverlay.hidden = true; });
  const machine = element("section", "slots-machine");
  machine.append(header, panel, lever);
  root.replaceChildren(machine, rules, payoutBar, winOverlay);

  let visible = false;
  let pageSuspended = false;
  let busy = false;
  let ready = false;
  let snapshot = null;
  let reading = null;
  let series = null;
  let seriesPayout = null;
  let nextSpin = null;
  let spinWait = null;
  let rewardEffect = "";
  const motions = new Map();
  let selectedLine = null;
  let lineButtons = [];
  let message = "";
  let failure = "";

  function setText(node, text) {
    if (node.textContent !== text) node.textContent = text;
  }

  function showRepeatMenu(open) {
    repeatMenu.hidden = !open;
    repeats.setAttribute("aria-expanded", String(open));
  }

  function buildRules() {
    lineButtons = snapshot.paylines.map(({ name, rows }, index) => {
      const choice = button(name);
      choice.setAttribute("aria-pressed", "false");
      const diagram = element("span", "slots-line-diagram");
      diagram.setAttribute("aria-hidden", "true");
      for (let row = 0; row < 3; row++) {
        for (const activeRow of rows) {
          diagram.append(element("span", "", row === activeRow ? "●" : "·"));
        }
      }
      choice.prepend(diagram);
      choice.addEventListener("click", () => {
        selectedLine = selectedLine === index ? null : index;
        paintReels();
        renderLines();
      });
      return choice;
    });
    linePicker.replaceChildren(...lineButtons);
    const head = element("thead");
    const labels = element("tr");
    for (const text of ["Символ", "3 подряд", "4 подряд", "5 подряд"]) {
      const cell = element("th", "", text);
      cell.scope = "col";
      labels.append(cell);
    }
    head.append(labels);
    const body = element("tbody");
    for (const { icon, name, payouts } of snapshot.symbols) {
      const row = element("tr");
      const symbol = element("th");
      symbol.scope = "row";
      symbol.append(symbolIcon(icon), element("span", "", ` ${name}`));
      row.append(symbol, ...payouts.map((value) => element("td", "", `${value}×`)));
      body.append(row);
    }
    paytable.replaceChildren(head, body);
  }

  function boardResults(spin) {
    return spin?.boards ?? (spin ? [spin] : []);
  }

  function accept(result, restoreBoardCount = false) {
    const rulesChanged = !snapshot || JSON.stringify([snapshot.symbols, snapshot.paylines]) !==
      JSON.stringify([result.symbols, result.paylines]);
    if (restoreBoardCount && result.spin && snapshot?.spin?.id !== result.spin.id) {
      seriesPayout = null;
      boardCount = boardResults(result.spin).length;
      selectBoards();
    }
    snapshot = result;
    if (rulesChanged) buildRules();
    ready = true;
    failure = "";
    if (!bet.value) bet.value = String(result.min_bet);
  }

  function paintReel(reel, column, data, result, highlight) {
    for (const [row, { cell, icon }] of reel.cells.entries()) {
      const symbol = result ? data.symbols[result.grid[row][column]] : null;
      const winning = highlight && result?.wins.some(({ line, count }) => {
        return column < count && data.paylines[line].rows[column] === row;
      });
      const marked = highlight && selectedLine !== null && data?.paylines[selectedLine]?.rows[column] === row;
      cell.classList.toggle("slots-symbol--win", Boolean(winning));
      cell.classList.toggle("slots-symbol--marked", Boolean(marked));
      cell.classList.toggle("slots-symbol--empty", !symbol);
      paintIcon(icon, symbol?.icon ?? "·");
      cell.setAttribute("aria-label", symbol ? `${symbol.name}${winning ? ", выигрыш" : ""}` : "Пусто");
    }
  }

  function paintReels() {
    const results = boardResults(snapshot?.spin);
    for (const [index, { grid, reels, result, wins }] of activeBoards.entries()) {
      const board = results[index];
      grid.classList.toggle("slots-reels--has-win", Boolean(board?.wins.length));
      for (const [column, reel] of reels.entries()) paintReel(reel, column, snapshot, board, true);
      result.hidden = activeBoards.length === 1 || !board;
      setText(result, board ? payoutText(board.payout) : "");
      wins.hidden = !board?.wins.length;
      wins.replaceChildren(...(board?.wins ?? []).map(({ line, count, symbol, multiplier }) => {
        const item = element("li", "", `${snapshot.paylines[line].name} · ${count} × `);
        const { icon, name } = snapshot.symbols[symbol];
        const mark = symbolIcon(icon);
        mark.setAttribute("role", "img");
        mark.setAttribute("aria-label", name);
        item.append(mark, element("span", "", ` · ${multiplier}× ставки`));
        return item;
      }));
    }
  }

  function renderLines() {
    for (const [index, choice] of lineButtons.entries()) {
      const active = selectedLine === index;
      choice.setAttribute("aria-pressed", String(active));
      choice.classList.toggle("casino-button", active);
      choice.disabled = busy;
    }
  }

  function payoutText(payout) {
    return payout ? `Выплата ${number.format(payout)} 🪙` : "Без выигрыша";
  }

  function renderCost() {
    const count = activeBoards.length;
    const amount = Number(bet.value.trim().replaceAll(" ", ""));
    setText(cost, count > 1 && Number.isSafeInteger(amount) && amount > 0 && amount <= snapshot?.max_bet
      ? `За запуск ${number.format(amount * count)} 🪙` : "");
  }

  function render() {
    const result = snapshot?.spin;
    root.dataset.slotsReward = busy ? "" : rewardEffect;
    setText(wallet, snapshot ? `Баланс ${number.format(snapshot.balance)} 🪙` : "Баланс —");
    setText(lastPayout, `Выиграно за запуск ${result ? `${number.format(result.payout)} 🪙` : "—"}`);
    seriesTotal.hidden = seriesPayout === null;
    setText(seriesTotal, seriesPayout === null ? "" : `За серию ${number.format(seriesPayout)} 🪙`);
    reelBoards.setAttribute("aria-busy", String(busy));
    const locked = !ready || busy || series !== null;
    bet.disabled = maximum.disabled = repeats.disabled = spin.disabled = lever.disabled = locked;
    lever.setAttribute("aria-label", repeatCount > 1 ? "Потянуть рычаг и запустить серию" : "Потянуть рычаг и запустить барабаны");
    for (const [index, choice] of boardChoices.entries()) {
      choice.disabled = locked;
      choice.setAttribute("aria-pressed", String(counts[index] === boardCount));
    }
    for (const [index, choice] of repeatChoices.entries()) {
      choice.setAttribute("aria-pressed", String(repeatCounts[index] === repeatCount));
    }
    setText(betCaption, activeBoards.length > 1 ? "Ставка на набор" : "Ставка");
    setText(repeats, repeatCount === 1 ? "1 запуск" : `${repeatCount} запусков`);
    setText(spin, busy ? "Вращаем…" : repeatCount > 1 ? "Запустить серию" : "Вращать");
    spin.hidden = series?.total > 1;
    stop.hidden = !spin.hidden;
    renderCost();
    if (snapshot) bet.title = `Ставка от ${number.format(snapshot.min_bet)} до ${number.format(snapshot.max_bet)} монет`;
    retry.hidden = !failure;
    retry.disabled = busy || reading !== null;
    renderLines();
    if (busy) {
      setText(feedback, series?.total > 1 ? `Вращение ${series.completed + 1} из ${series.total}…` : "Идёт вращение.");
      for (const { result, wins } of activeBoards) result.hidden = wins.hidden = true;
      return;
    }
    paintReels();
    const progress = series?.total > 1 ? `${series.completed} из ${series.total} · ` : "";
    setText(feedback, [message, failure].filter(Boolean).join(" ") || (result
      ? `${progress}Ставка ${number.format(result.stake)} 🪙`
      : snapshot ? "Выбери ставку и запусти барабаны." : "Загружаем слоты…"));
  }

  function removeTrack(reel) {
    const motion = motions.get(reel);
    if (!motion) return;
    motions.delete(reel);
    const { track, animation } = motion;
    animation?.cancel();
    track.remove();
    for (const { cell } of reel.cells) cell.style.visibility = "";
  }

  function stopAnimation() {
    finishSpinWait();
    for (const reel of motions.keys()) removeTrack(reel);
  }

  function finishSpinWait() {
    if (!spinWait) return;
    clearTimeout(spinWait.timer);
    spinWait.resolve();
    spinWait = null;
  }

  function moveTrack(track, gap, from, to, options) {
    // The overlay stays three rows tall. Each step is one third of its height plus a third of a gap.
    const keyframes = [from, to].map((rows) => ({
      transform: `translateY(calc(-${rows * 100 / 3}% - ${rows * gap / 3}px))`,
    }));
    return track.animate(keyframes, options);
  }

  function fillTrack(track, icons) {
    track.replaceChildren(...icons.map((icon) => {
      const cell = element("span", "slots-symbol");
      cell.append(symbolIcon(icon));
      return cell;
    }));
  }

  function canAnimate() {
    return visible && !pageSuspended && !document.hidden && !reducedMotion.matches;
  }

  function startAnimation() {
    const firstStrip = activeBoards[0].reels[0].strip;
    if (!canAnimate() || typeof firstStrip.animate !== "function" || !snapshot.symbols.length) return;
    const icons = snapshot.symbols.map(({ icon }) => icon);
    const gap = parseFloat(window.getComputedStyle(firstStrip).rowGap) || 0;
    for (const [index, { reels }] of activeBoards.entries()) {
      for (const [column, reel] of reels.entries()) {
        const track = element("div", "slots-strip");
        track.setAttribute("aria-hidden", "true");
        Object.assign(track.style, {
          position: "absolute",
          inset: "0",
          rowGap: `${gap}px`,
          gridAutoRows: `calc((100% - ${2 * gap}px) / 3)`,
        });
        const iconAt = (row) => icons[(row + column * 2 + index) % icons.length];
        const duration = icons.length * (48 + column * 4);
        const motion = { index, column, track, animation: null, gap, iconAt, length: icons.length };
        motions.set(reel, motion);
        fillTrack(track, Array.from({ length: icons.length + 3 }, (_, row) => iconAt(row)));
        reel.strip.append(track);
        for (const { cell } of reel.cells) cell.style.visibility = "hidden";
        motion.animation = moveTrack(track, gap, 0, icons.length, { duration, iterations: Infinity });
      }
    }
  }

  async function settleReels(data, results, automatic) {
    await Promise.all([...motions].map(async ([reel, motion]) => {
      try {
        const { index, column, track, gap, iconAt, length } = motion;
        // Keep the visible symbols and their fractional offset when switching to the stop animation.
        const position = (motion.animation.effect.getComputedTiming().progress ?? 0) * length;
        const row = Math.floor(position);
        const steps = 9 + column * 2;
        const icons = Array.from({ length: steps }, (_, index) => iconAt(row + index));
        for (const resultRow of results[index].grid) icons.push(data.symbols[resultRow[column]].icon);
        motion.animation.cancel();
        fillTrack(track, icons);
        motion.animation = moveTrack(track, gap, position - row, steps, {
          duration: 690 + column * 155 + index * 45 + (automatic ? 280 : 0),
          easing: "cubic-bezier(.25,.4,.55,1)",
          fill: "both",
        });
        await motion.animation.finished;
        if (motions.get(reel) !== motion || !canAnimate()) return;
        paintReel(reel, column, data, results[index], false);
      } catch {
        // The final render still paints the receipt if animation is canceled or unavailable.
      } finally {
        removeTrack(reel);
      }
    }));
  }

  function cancelSeries(status) {
    if (nextSpin !== null) clearTimeout(nextSpin);
    nextSpin = null;
    series = null;
    if (status) message = status;
  }

  reducedMotion.addEventListener("change", (event) => {
    if (event.matches) stopAnimation();
  });
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) return;
    winOverlay.hidden = true;
    rewardEffect = "";
    showRepeatMenu(false);
    cancelSeries(series?.total > 1 ? "Автопрокрутка остановлена." : "");
    stopAnimation();
    if (visible) render();
  });
  window.addEventListener("pagehide", () => {
    pageSuspended = true;
    winOverlay.hidden = true;
    rewardEffect = "";
    showRepeatMenu(false);
    cancelSeries(series?.total > 1 ? "Автопрокрутка остановлена." : "");
    stopAnimation();
    if (visible) render();
  });
  window.addEventListener("pageshow", () => { pageSuspended = false; });

  async function sync() {
    if (!visible || busy || reading || series) return;
    const current = {};
    reading = current;
    render();
    try {
      const result = await request("state");
      if (reading === current) accept(result, true);
    } catch (error) {
      if (reading === current) {
        ready = false;
        failure = error.message || "Не удалось получить состояние слотов.";
      }
    } finally {
      if (reading === current) {
        reading = null;
        render();
      }
    }
  }

  async function play(run) {
    if (series !== run || !visible || pageSuspended || document.hidden || !ready || busy) return;
    const expected = snapshot.spin?.id ?? null;
    const automatic = run.total > 1;
    busy = true;
    winOverlay.hidden = true;
    rewardEffect = "";
    selectedLine = null;
    render();
    try {
      startAnimation();
    } catch {
      stopAnimation();
    }
    const leadIn = automatic && motions.size ? new Promise((resolve) => {
      spinWait = { resolve, timer: setTimeout(finishSpinWait, 1050) };
    }) : null;
    try {
      const result = await request("spin", { bet: run.bet, boards: run.boards, expected_round: expected });
      const id = result.spin?.id;
      const newResult = typeof id === "string" && id !== expected;
      const results = boardResults(result.spin);
      const matching = results.length === run.boards;
      let reward;
      if (newResult && matching) {
        if (leadIn) await leadIn;
        await settleReels(result, results, automatic);
        const multiplier = Math.max(...results.map((board) => board.payout / board.stake));
        reward = rewardLevels.find(({ min }) => multiplier >= min);
        if (visible && !document.hidden && !pageSuspended) {
          rewardEffect = reward?.effect ?? "";
          if (multiplier >= 3) {
            setText(winAmount, `${number.format(result.spin.payout)} 🪙`);
            winOverlay.hidden = false;
          }
        }
        if (automatic) seriesPayout += result.spin.payout;
      }
      accept(result);
      if (series === run) {
        if (!newResult || !matching) {
          cancelSeries(run.total > 1 ? "Автопрокрутка остановлена. Проверь последний результат." : "");
        } else {
          run.completed++;
          run.bet = String(results[0].stake);
          if (run.completed === run.total) {
            cancelSeries(run.total > 1 ? `Автопрокрутка завершена. Запусков ${run.total}.` : "");
          } else if (result.balance < result.spin.stake) {
            cancelSeries(`Не хватает монет для следующего запуска. Выполнено ${run.completed} из ${run.total}.`);
          } else {
            nextSpin = setTimeout(() => {
              nextSpin = null;
              play(run);
            }, reward?.pause ?? (result.spin.payout > 0 ? 1350 : 750));
          }
        }
      }
    } catch (error) {
      stopAnimation();
      cancelSeries();
      ready = false;
      message = error.message || "Ответ о вращении не получен.";
      try {
        accept(await request("state"), true);
        if ((snapshot.spin?.id ?? null) !== expected) message = "Показан последний сохранённый результат.";
      } catch {
        failure = "Проверь последний результат перед новой ставкой.";
      }
    } finally {
      stopAnimation();
      busy = false;
      if (visible) render();
    }
  }

  maximum.addEventListener("click", () => {
    bet.value = String(Math.max(0, Math.min(Math.floor(snapshot.balance / activeBoards.length), snapshot.max_bet)));
    renderCost();
  });
  bet.addEventListener("input", renderCost);
  repeats.addEventListener("click", () => {
    if (!repeats.disabled) showRepeatMenu(repeatMenu.hidden);
  });
  repeatPicker.addEventListener("keydown", (event) => {
    if (event.key !== "Escape" || repeatMenu.hidden) return;
    event.preventDefault();
    showRepeatMenu(false);
    repeats.focus();
  });
  document.addEventListener("click", (event) => {
    if (!repeatMenu.hidden && !repeatPicker.contains(event.target)) showRepeatMenu(false);
  });
  stop.addEventListener("click", () => {
    cancelSeries("Автопрокрутка остановлена.");
    render();
  });
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    if (!visible || pageSuspended || document.hidden || !ready || busy || series) return;
    if (event.submitter === lever && !reducedMotion.matches) {
      const from = window.getComputedStyle(leverArm).transform;
      leverArm.animate?.([
        { transform: from },
        { transform: "var(--slots-lever-pull)", offset: .42 },
        { transform: "none" },
      ], { duration: 370, easing: "ease-out" });
    }
    const total = repeatCount;
    showRepeatMenu(false);
    message = "";
    seriesPayout = total > 1 ? 0 : null;
    series = { total, completed: 0, bet: bet.value, boards: activeBoards.length };
    play(series);
  });
  retry.addEventListener("click", () => {
    message = "";
    sync();
  });
  render();
  return {
    show() {
      if (visible) return;
      visible = true;
      ready = false;
      message = "";
      sync();
      render();
    },
    hide() {
      visible = false;
      winOverlay.hidden = true;
      rewardEffect = "";
      showRepeatMenu(false);
      reading = null;
      cancelSeries();
      stopAnimation();
    },
  };
};
