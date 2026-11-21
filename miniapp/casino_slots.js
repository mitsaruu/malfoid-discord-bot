"use strict";

(window.CasinoGames ||= {}).slots = function ({ root, request }) {
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

  const number = new Intl.NumberFormat("ru-RU");
  const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
  const title = element("h1", "", "Слоты");
  title.id = "slots-title";
  root.setAttribute("aria-labelledby", title.id);
  const wallet = element("p", "casino-balance");
  const header = element("header", "casino-header");
  header.append(title, wallet);
  const panel = element("section", "slots-cabinet");
  const dock = element("div", "casino-dock slots-dock");
  const form = element("form", "casino-toolbar");
  const betLabel = element("label", "", "Ставка");
  const bet = element("input");
  bet.type = "text";
  bet.inputMode = "numeric";
  bet.autocomplete = "off";
  bet.maxLength = 20;
  bet.required = true;
  betLabel.append(bet);
  const maximum = button("Макс.");
  const spin = button("Вращать", "casino-button", "submit");
  form.append(betLabel, maximum, spin);
  const grid = element("div", "slots-reels");
  grid.setAttribute("role", "group");
  grid.setAttribute("aria-label", "Пять барабанов по три символа");
  const reels = Array.from({ length: 5 }, (_, column) => {
    const frame = element("div", "slots-reel");
    const strip = element("div", "slots-strip");
    strip.setAttribute("role", "group");
    strip.setAttribute("aria-label", `Барабан ${column + 1}`);
    const cells = Array.from({ length: 3 }, () => {
      const cell = element("span", "slots-symbol", "·");
      cell.setAttribute("role", "img");
      strip.append(cell);
      return cell;
    });
    frame.append(strip);
    grid.append(frame);
    return { strip, cells };
  });
  const feedback = element("p", "casino-feedback");
  feedback.setAttribute("role", "status");
  const wins = element("ul", "slots-wins");
  const notice = element("p", "casino-feedback");
  notice.setAttribute("role", "status");
  const retry = button("Проверить последний результат");
  dock.append(form, feedback, wins, notice, retry);
  panel.append(grid, dock);

  const rules = element("section", "casino-panel");
  const linesHeading = element("h2", "", "Линии выплат");
  const linePicker = element("div", "casino-toolbar slots-lines");
  linePicker.setAttribute("role", "group");
  linePicker.setAttribute("aria-label", "Подсветка линии на барабанах");
  const explanation = element("p", "", "Три и более одинаковых символа подряд от левого края дают выплату по линии. Каждый множитель применяется к полной ставке. Выплаты по линиям складываются.");
  const paytable = element("table", "slots-paytable");
  paytable.setAttribute("aria-label", "Таблица выплат");
  rules.append(linesHeading, linePicker, explanation, element("h2", "", "Таблица выплат"), paytable);
  root.replaceChildren(header, panel, rules);

  let visible = false;
  let busy = false;
  let ready = false;
  let snapshot = null;
  let reading = null;
  let animations = [];
  let selectedLine = null;
  let lineButtons = [];
  let message = "";
  let failure = "";

  function setText(node, text) {
    if (node.textContent !== text) node.textContent = text;
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
      const symbol = element("th", "", `${icon} ${name}`);
      symbol.scope = "row";
      row.append(symbol, ...payouts.map((value) => element("td", "", `${value}×`)));
      body.append(row);
    }
    paytable.replaceChildren(head, body);
  }

  function accept(result) {
    const rulesChanged = !snapshot || JSON.stringify([snapshot.symbols, snapshot.paylines]) !==
      JSON.stringify([result.symbols, result.paylines]);
    snapshot = result;
    if (rulesChanged) buildRules();
    ready = true;
    failure = "";
    if (!bet.value) bet.value = String(result.min_bet);
  }

  function paintReels(highlight = true) {
    const result = snapshot?.spin;
    for (const [column, reel] of reels.entries()) {
      for (const [row, cell] of reel.cells.entries()) {
        const symbol = result ? snapshot.symbols[result.grid[row][column]] : null;
        const winning = highlight && result?.wins.some(({ line, count }) => {
          return column < count && snapshot.paylines[line].rows[column] === row;
        });
        const marked = highlight && selectedLine !== null && snapshot.paylines[selectedLine].rows[column] === row;
        cell.classList.toggle("slots-symbol--win", Boolean(winning));
        cell.classList.toggle("slots-symbol--marked", marked);
        cell.classList.toggle("slots-symbol--empty", !symbol);
        setText(cell, symbol?.icon ?? "·");
        cell.setAttribute("aria-label", symbol ? `${symbol.name}${winning ? ", выигрыш" : ""}` : "Пусто");
      }
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

  function render() {
    const result = snapshot?.spin;
    setText(wallet, snapshot ? `Баланс ${number.format(snapshot.balance)} 🪙` : "Баланс —");
    bet.disabled = maximum.disabled = spin.disabled = !ready || busy;
    setText(spin, busy ? "Вращаем…" : "Вращать");
    if (snapshot) bet.title = `Ставка от ${number.format(snapshot.min_bet)} до ${number.format(snapshot.max_bet)} монет`;
    retry.hidden = !failure;
    retry.disabled = busy || reading !== null;
    notice.hidden = !(message || failure);
    setText(notice, [message, failure].filter(Boolean).join(" "));
    renderLines();
    if (busy) {
      setText(feedback, "Идёт вращение.");
      wins.hidden = true;
      return;
    }
    paintReels();
    setText(feedback, result
      ? `Ставка ${number.format(result.stake)} 🪙 · ${result.payout ? `Выплата ${number.format(result.payout)} 🪙` : "Без выигрыша"}`
      : snapshot ? "Выбери ставку и запусти барабаны." : "Загружаем слоты…");
    wins.hidden = !result?.wins.length;
    wins.replaceChildren(...(result?.wins ?? []).map(({ line, count, symbol, multiplier }) =>
      element("li", "", `${snapshot.paylines[line].name} · ${count} × ${snapshot.symbols[symbol].icon} · ${multiplier}× ставки`)
    ));
  }

  function stopAnimation() {
    for (const animation of animations) animation.cancel();
    animations = [];
  }

  async function animateReels() {
    if (reducedMotion.matches || typeof reels[0].strip.animate !== "function") return;
    animations = reels.map(({ strip }, index) => strip.animate([
      { transform: "translateY(-85%)", filter: "blur(3px)", opacity: 0.3 },
      { transform: "translateY(7%)", filter: "blur(0px)", opacity: 1, offset: 0.83 },
      { transform: "translateY(0)", filter: "blur(0px)", opacity: 1 },
    ], { duration: 320 + index * 75, easing: "ease-out", fill: "backwards" }));
    await Promise.allSettled(animations.map((animation) => animation.finished));
    animations = [];
  }

  reducedMotion.addEventListener("change", (event) => {
    if (event.matches) stopAnimation();
  });

  async function sync() {
    if (!visible || busy || reading) return;
    const current = {};
    reading = current;
    render();
    try {
      const result = await request("state");
      if (reading === current) accept(result);
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

  async function play() {
    if (!visible || !ready || busy) return;
    const expected = snapshot.spin?.id ?? null;
    busy = true;
    selectedLine = null;
    message = "";
    render();
    try {
      accept(await request("spin", { bet: bet.value, expected_round: expected }));
      if (visible) {
        paintReels(false);
        await animateReels();
      }
    } catch (error) {
      ready = false;
      message = error.message || "Ответ о вращении не получен.";
      try {
        accept(await request("state"));
        if ((snapshot.spin?.id ?? null) !== expected) message = "Показан последний сохранённый результат.";
      } catch {
        failure = "Проверь последний результат перед новой ставкой.";
      }
    } finally {
      busy = false;
      if (visible) render();
    }
  }

  maximum.addEventListener("click", () => {
    bet.value = String(Math.max(0, Math.min(snapshot.balance, snapshot.max_bet)));
  });
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    play();
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
      reading = null;
      stopAnimation();
    },
  };
};
