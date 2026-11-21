"use strict";

(window.CasinoGames ||= {}).mines = function ({ root, request }) {
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
  const multiplier = new Intl.NumberFormat("ru-RU", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
  const title = element("h1", "", "Мины");
  title.id = "mines-title";
  root.setAttribute("aria-labelledby", title.id);
  const wallet = element("p");
  const panel = element("section", "casino-panel");
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
  const countLabel = element("label", "", "Мины");
  const mineCount = element("select");
  for (let count = 1; count <= 24; count++) {
    const option = element("option", "", String(count));
    option.value = String(count);
    mineCount.append(option);
  }
  mineCount.value = "3";
  countLabel.append(mineCount);
  const start = button("Открыть поле", "casino-button", "submit");
  form.append(betLabel, maximum, countLabel, start);
  const controls = element("div", "casino-toolbar");
  const cashout = button("Забрать монеты", "casino-button");
  const countdown = element("span");
  controls.append(cashout, countdown);
  const feedback = element("p", "casino-feedback");
  feedback.setAttribute("role", "status");
  const errorMessage = element("p", "casino-feedback");
  errorMessage.setAttribute("role", "status");
  const retry = button("Обновить состояние");
  const grid = element("div", "casino-grid");
  grid.setAttribute("role", "group");
  grid.setAttribute("aria-label", "Поле из 25 клеток");
  grid.style.gridTemplateColumns = "repeat(5, minmax(0, 1fr))";
  grid.style.maxWidth = "32rem";
  const tiles = Array.from({ length: 25 }, (_, index) => {
    const tile = button("·", "casino-card");
    tile.style.minWidth = "0";
    tile.style.padding = "0";
    tile.addEventListener("click", () => {
      if (snapshot?.round?.status !== "active" || snapshot.round.cells[index] !== "hidden") return;
      mutate("reveal", { round_id: snapshot.round.id, cell: index });
    });
    grid.append(tile);
    return tile;
  });
  const rules = element("p");
  panel.append(form, controls, feedback, grid, errorMessage, retry, rules);
  root.replaceChildren(title, wallet, panel);

  let visible = false;
  let pending = false;
  let ready = false;
  let snapshot = null;
  let reading = null;
  let poll = null;
  let clock = null;
  let expiresAt = 0;
  let failure = "";
  let actionError = "";
  const tileViews = {
    hidden: ["·", "закрыта", ""],
    safe: ["◆", "безопасна", "#23534a"],
    mine: ["✹", "мина", "#5a343d"],
    hit: ["✕", "подрыв", "#a33446"],
  };

  function setText(node, text) {
    if (node.textContent !== text) node.textContent = text;
  }

  function accept(result) {
    snapshot = result;
    expiresAt = performance.now() + (result.round?.remaining_ms ?? 0);
    ready = true;
    failure = "";
    if (!bet.value) bet.value = String(result.min_bet);
    setText(rules, `Открывай клетки и забирай монеты после любого удачного хода. ` +
      `Через ${result.idle_seconds / 60} мин бездействия сервер выплатит выигрыш. Если ни одной клетки не открыто, ставка вернётся. ` +
      `Теоретическое преимущество игры — ${number.format(result.house_edge_bps / 100)}%. ` +
      `Коэффициент округляется вниз до сотых, выплата — до целой монеты.`);
  }

  function updateClock() {
    if (snapshot?.round?.status !== "active") return;
    const seconds = Math.max(0, Math.ceil((expiresAt - performance.now()) / 1000));
    setText(countdown, `До авторасчёта ${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`);
  }

  function render() {
    const round = snapshot?.round;
    const active = round?.status === "active";
    const blocked = pending || !ready;
    setText(wallet, snapshot ? `Баланс ${number.format(snapshot.balance)} 🪙` : "Баланс —");
    bet.disabled = maximum.disabled = mineCount.disabled = start.disabled = blocked || active;
    cashout.disabled = blocked || !active || round.cashout == null;
    cashout.hidden = countdown.hidden = !active;
    retry.hidden = !failure;
    retry.disabled = pending || reading !== null;
    errorMessage.hidden = !(failure || actionError);
    setText(errorMessage, [actionError, failure].filter(Boolean).join(" "));
    if (snapshot) bet.title = `Ставка от ${number.format(snapshot.min_bet)} до ${number.format(snapshot.max_bet)} монет`;
    if (active) mineCount.value = String(round.mine_count);
    setText(cashout, active && round.cashout != null
      ? `Забрать ${number.format(round.cashout)} 🪙`
      : "Забрать монеты");

    let message = snapshot ? "Выбери ставку и число мин." : "Загружаем поле…";
    if (active) {
      const progress = `${round.safe_count} из ${25 - round.mine_count} безопасных клеток`;
      message = `Ставка ${number.format(round.stake)} 🪙 · ${progress} · ${multiplier.format(round.multiplier / 100)}×`;
    } else if (round?.status === "lost") {
      message = `Мина! Ставка ${number.format(round.stake)} 🪙 проиграна.`;
    } else if (round?.status === "paid") {
      message = `${round.timed_out ? "Автовыплата" : "Выплата"} ${number.format(round.payout)} 🪙 · ${multiplier.format(round.multiplier / 100)}×`;
    } else if (round?.status === "refunded") {
      message = `Таймер истёк. Ставка ${number.format(round.payout)} 🪙 возвращена.`;
    }
    setText(feedback, message);
    updateClock();
    for (const [index, tile] of tiles.entries()) {
      const state = round?.cells[index] ?? "hidden";
      const [icon, description, color] = tileViews[state];
      tile.disabled = blocked || !active || state !== "hidden";
      if (tile.dataset.state !== state) {
        tile.dataset.state = state;
        tile.style.backgroundColor = color;
        tile.style.opacity = state === "hidden" ? "" : "1";
        tile.setAttribute("aria-label", `Ряд ${Math.floor(index / 5) + 1}, клетка ${index % 5 + 1}, ${description}`);
        setText(tile, icon);
      }
    }
  }

  function schedule() {
    clearTimeout(poll);
    if (visible && !pending && snapshot?.round?.status === "active") poll = setTimeout(sync, 3000);
  }

  async function sync() {
    if (!visible || pending || reading) return;
    const current = {};
    reading = current;
    render();
    try {
      const result = await request("state");
      if (reading === current) accept(result);
    } catch (error) {
      if (reading === current) {
        ready = false;
        failure = error.message || "Не удалось загрузить поле.";
      }
    } finally {
      if (reading === current) {
        reading = null;
        render();
        schedule();
      }
    }
  }

  async function mutate(action, fields) {
    if (!visible || pending || !ready) return;
    clearTimeout(poll);
    reading = null;
    pending = true;
    actionError = "";
    render();
    try {
      accept(await request(action, fields));
    } catch (error) {
      actionError = error.message || "Не удалось подтвердить ход.";
      ready = false;
      try {
        accept(await request("state"));
      } catch {
        failure = "Обнови поле перед следующим ходом.";
      }
    } finally {
      pending = false;
      if (visible) {
        render();
        schedule();
      }
    }
  }

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    if (!snapshot || snapshot.round?.status === "active") return;
    mutate("start", { expected_round: snapshot.revision, bet: bet.value, mine_count: Number(mineCount.value) });
  });
  maximum.addEventListener("click", () => {
    bet.value = String(Math.max(0, Math.min(snapshot.balance, snapshot.max_bet)));
  });
  cashout.addEventListener("click", () => {
    if (snapshot?.round?.cashout != null) mutate("cashout", { round_id: snapshot.round.id });
  });
  retry.addEventListener("click", () => {
    actionError = "";
    sync();
  });
  render();
  return {
    show() {
      if (visible) return;
      visible = true;
      ready = false;
      actionError = "";
      clock = setInterval(updateClock, 1000);
      sync();
      render();
    },
    hide() {
      visible = false;
      reading = null;
      clearTimeout(poll);
      clearInterval(clock);
    },
  };
};
