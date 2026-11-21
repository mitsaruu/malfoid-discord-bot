(window.CasinoGames ||= {}).poker = function ({ root, request }) {
  const streets = { preflop: "Префлоп", flop: "Флоп", turn: "Тёрн", river: "Ривер" };
  const ranks = ["Старшая карта", "Пара", "Две пары", "Тройка", "Стрит", "Флеш",
    "Фулл-хаус", "Каре", "Стрит-флеш"];
  const suits = { "♣": "clubs", "♦": "diamonds", "♥": "hearts", "♠": "spades" };
  const moves = { fold: "пас", check: "чек", call: "колл", bet: "ставка", raise: "рейз", all_in: "олл-ин" };
  const fmt = new Intl.NumberFormat("ru-RU");
  const amount = (value) => fmt.format(value);

  function node(tag, label, className) {
    const element = document.createElement(tag);
    if (label) element.textContent = label;
    if (className) element.className = className;
    return element;
  }

  function button(label, fn, className) {
    const element = node("button", label, className);
    element.type = "button";
    if (fn) element.addEventListener("click", fn);
    return element;
  }

  function setText(element, label) {
    if (element.textContent !== label) element.textContent = label;
  }

  function makeCards(cards, count = 0) {
    const row = node("div", "", "casino-toolbar");
    for (const label of cards || Array(count).fill(null)) {
      const card = node("span", label || "▧", "casino-card");
      if (label) card.dataset.suit = suits[label.slice(-1)];
      else card.setAttribute("aria-label", "Закрытая карта");
      row.append(card);
    }
    return row;
  }

  const modeBar = node("div", "", "casino-toolbar");
  modeBar.setAttribute("role", "group");
  modeBar.setAttribute("aria-label", "Режим покера");
  const onlineTab = button("Игра на сервере", () => select("online"));
  const soloTab = button("Тренировка с ботом", () => select("solo"));
  modeBar.append(onlineTab, soloTab);

  const account = node("p");
  const status = node("h2", "Загрузка…");
  status.setAttribute("aria-live", "polite");
  const clock = node("p");
  const board = node("div");
  const last = node("p");
  const tablePanel = node("section", "", "casino-panel");
  tablePanel.append(status, clock, board, last);
  const seats = node("div", "", "casino-grid");
  seats.style.gridTemplateColumns = "repeat(auto-fit, minmax(min(100%, 220px), 1fr))";
  const results = node("div", "", "casino-panel");
  const previous = node("p");

  const joinForm = node("form", "", "casino-toolbar");
  const buyInLabel = node("label", "Взнос за место");
  const buyIn = node("input");
  buyIn.type = "text";
  buyIn.maxLength = 32;
  buyIn.required = true;
  buyIn.autocomplete = "off";
  buyIn.placeholder = "100";
  buyInLabel.append(buyIn);
  const join = button("Занять место", null, "casino-button");
  join.type = "submit";
  joinForm.append(buyInLabel, join);
  const lobbyControls = node("div", "", "casino-toolbar");
  const start = button("Начать раздачу", () => act("start"), "casino-button");
  const leave = button("Покинуть стол", () => act("leave"));
  const soloStart = button("Начать тренировку", () => act("solo_start"), "casino-button");
  lobbyControls.append(start, leave, soloStart);

  const handControls = node("div", "", "casino-toolbar");
  const fold = button("Пас", () => act("fold"));
  const check = button("Чек", () => act("check"), "casino-button");
  const call = button("Колл", () => act("call"), "casino-button");
  const allIn = button("Олл-ин", () => act("all_in"));
  const raiseForm = node("form", "", "casino-toolbar");
  const raiseLabel = node("label", "Всего на этой улице");
  const raiseInput = node("input");
  raiseInput.type = "number";
  raiseInput.required = true;
  raiseInput.step = "1";
  raiseLabel.append(raiseInput);
  const raise = button("Рейз");
  raise.type = "submit";
  raiseForm.append(raiseLabel, raise);
  handControls.append(fold, check, call, allIn, raiseForm);
  const controls = node("div", "", "casino-panel");
  controls.append(joinForm, lobbyControls, handControls);

  const feedback = node("p", "", "casino-feedback");
  feedback.setAttribute("role", "status");
  root.replaceChildren(node("h1", "Техасский холдем"), modeBar, account, tablePanel,
    controls, feedback, seats, results, previous);

  let visible = false;
  let mode = "online";
  let state;
  let generation = 0;
  let pendingPoll;
  let pendingAction;
  let pollError = false;
  let timer;
  let deadline = 0;
  let tableKey;
  let raiseKey;

  function renderTable(table) {
    const tableData = { ...table };
    delete tableData.remaining_ms;
    const key = table ? JSON.stringify(tableData) : state ? mode : "loading";
    if (key === tableKey) return;
    tableKey = key;
    board.replaceChildren();
    seats.replaceChildren();
    results.replaceChildren();
    results.hidden = true;
    last.hidden = true;
    if (!table) {
      status.textContent = state ? (mode === "online" ? "Свободный стол" : "Тренировка") : "Загрузка…";
      return;
    }

    switch (table.status) {
      case "lobby":
        status.textContent = `Набор игроков · ${table.seats.length} из ${state.max_seats}`;
        break;
      case "playing":
        status.textContent = table.actor === table.you ? `${streets[table.street]} · Ваш ход`
          : `${streets[table.street]} · Ходит ${table.seats[table.actor].name}`;
        break;
      case "finished":
        status.textContent = table.pots?.some((pot) => pot.winners.length > 1)
          ? "Раздача завершена · Банк разделён" : "Раздача завершена";
        break;
    }

    if (table.status !== "lobby") {
      board.append(node("p", `Банк · ${amount(table.pot)}`));
      if (table.board.length) board.append(makeCards(table.board));
      const mine = table.seats[table.you]?.cards;
      if (mine) board.append(node("p", "Ваши карты"), makeCards(mine));
    }
    if (table.last_action) {
      const action = table.last_action;
      const chips = action.amount ? ` · ${amount(action.amount)}` : "";
      last.hidden = false;
      last.textContent = `${table.seats[action.seat].name} · ${moves[action.kind]}${chips}${action.timed_out ? " по таймеру" : ""}`;
    }

    table.seats.forEach((seat, i) => {
      const player = node("section", "", "casino-panel");
      const name = seat.name + (table.you === i ? " (вы)" : "");
      player.append(node("h2", name));
      const chips = mode === "solo" ? "фишек" : "монет";
      const details = [];
      if (table.status === "lobby" && i === 0) details.push("Создатель стола");
      if (table.dealer === i) details.push("Дилер");
      if (table.small_blind_seat === i) details.push("Малый блайнд");
      if (table.big_blind_seat === i) details.push("Большой блайнд");
      if (seat.folded) details.push("Пас");
      if (seat.all_in) details.push("Олл-ин");
      if (table.actor === i) details.push("Ходит");
      if (table.status !== "finished" && seat.street_bet) details.push(`Ставка ${amount(seat.street_bet)}`);
      if (seat.rank !== null) details.push(ranks[seat.rank]);
      if (seat.won) details.push(`Выигрыш ${amount(seat.won)}`);
      if (seat.returned) details.push(`Возврат ${amount(seat.returned)}`);
      const stack = table.status === "finished" && mode === "online" ? "Выплата" : "Стек";
      details.unshift(`${stack} ${amount(seat.stack)} ${chips}`);
      player.append(node("p", details.join(" · ")));
      if (i !== table.you && (seat.cards || (table.status === "playing" && !seat.folded))) {
        player.append(makeCards(seat.cards, 2));
      }
      seats.append(player);
    });

    if (table.pots?.length) {
      results.hidden = false;
      results.append(node("h2", "Результат"));
      for (const [i, pot] of table.pots.entries()) {
        const winners = pot.winners.map((winner) => table.seats[winner].name).join(", ");
        results.append(node("p", `${i === 0 ? "Основной банк" : `Побочный банк ${i}`} · ${amount(pot.amount)} · ${winners}`));
      }
    }
  }

  function renderClock() {
    const table = state?.table;
    clock.hidden = table?.remaining_ms == null;
    if (clock.hidden) return;
    const seconds = Math.max(0, Math.ceil((deadline - performance.now()) / 1000));
    const label = table.status === "lobby" ? "До закрытия лобби" : "До конца хода";
    setText(clock, `${label} · ${seconds} с`);
  }

  function render() {
    const online = mode === "online";
    const table = state?.table;
    const lobby = table?.status === "lobby";
    const finished = table?.status === "finished";
    const seated = table?.you != null;
    const actions = table?.actions;
    const busy = Boolean(pendingAction);

    onlineTab.setAttribute("aria-pressed", String(online));
    soloTab.setAttribute("aria-pressed", String(!online));
    const blinds = state ? ` · Блайнды ${state.small_blind}/${state.big_blind}` : "";
    const summary = online
      ? state ? `Кошелёк сервера · ${amount(state.balance)} монет` : ""
      : state ? `Тренировочные фишки · по ${amount(state.training_stack)} при пополнении` : "";
    setText(account, summary + blinds);
    renderTable(table);
    renderClock();

    joinForm.hidden = !online || !state || (table && !finished && (!lobby || seated));
    const full = lobby && table.seats.length >= state.max_seats;
    buyIn.disabled = join.disabled = busy || full;
    if (online && state) {
      buyIn.placeholder = String(Math.max(100, state.min_buy_in));
      buyIn.title = `Взнос от ${amount(state.min_buy_in)} до ${amount(state.max_buy_in)} монет или «всё»`;
    }
    setText(join, full ? "Стол заполнен" : "Занять место");
    start.hidden = !online || !lobby || table.you !== 0;
    start.disabled = busy || !lobby || table.seats.length < 2;
    leave.hidden = !online || !lobby || !seated;
    leave.disabled = busy;
    soloStart.hidden = online || !state || (table && !finished);
    soloStart.disabled = busy;
    setText(soloStart, finished ? "Новая раздача" : "Начать тренировку");
    lobbyControls.hidden = start.hidden && leave.hidden && soloStart.hidden;

    handControls.hidden = !actions;
    fold.disabled = busy || !actions?.fold;
    check.hidden = !actions?.check;
    check.disabled = busy;
    call.hidden = !actions?.call;
    call.disabled = busy;
    if (actions?.call) setText(call, `Колл · ${amount(actions.call)}`);
    allIn.hidden = !actions?.all_in;
    allIn.disabled = busy;
    raiseForm.hidden = !actions?.raise;
    raiseInput.disabled = raise.disabled = busy;
    const nextRaiseKey = actions?.raise ? `${table.id}/${table.turn}` : null;
    if (nextRaiseKey && nextRaiseKey !== raiseKey) {
      raiseInput.min = actions.raise.min;
      raiseInput.max = actions.raise.max;
      raiseInput.value = String(actions.raise.min);
      setText(raise, actions.raise.opening ? "Поставить" : "Повысить");
    }
    raiseKey = nextRaiseKey;
    controls.hidden = joinForm.hidden && lobbyControls.hidden && handControls.hidden;

    previous.hidden = !online || !state?.previous;
    if (!previous.hidden) {
      const prior = state.previous;
      setText(previous, `Предыдущая раздача · В кошелёк начислено ${amount(prior.seats[prior.you].stack)} монет`);
    }
  }

  function receive(next) {
    state = next;
    deadline = performance.now() + (state.table?.remaining_ms || 0);
    render();
  }

  async function poll() {
    if (!visible || pendingPoll || pendingAction) return;
    const gen = generation;
    const token = {};
    pendingPoll = token;
    try {
      const next = await request("state", { mode });
      if (!visible || gen !== generation || pendingPoll !== token) return;
      if (pollError) setText(feedback, "");
      pollError = false;
      receive(next);
    } catch (error) {
      if (!visible || gen !== generation || pendingPoll !== token) return;
      pollError = true;
      setText(feedback, error.message || "Не удалось обновить стол.");
    } finally {
      if (pendingPoll === token) pendingPoll = null;
    }
  }

  async function act(action, extra = {}) {
    if (!visible || !state || pendingAction) return;
    const gen = generation;
    const token = {};
    pendingAction = token;
    pendingPoll = null;
    pollError = false;
    setText(feedback, "");
    render();
    try {
      const table = state.table;
      const next = await request(action, { mode, table_id: table?.id ?? null, turn: table?.turn, ...extra });
      if (visible && gen === generation) receive(next);
    } catch (error) {
      if (visible && gen === generation) setText(feedback, error.message || "Не удалось выполнить ход.");
    } finally {
      if (pendingAction === token) {
        pendingAction = null;
        if (visible && gen === generation) {
          render();
          poll();
        }
      }
    }
  }

  joinForm.addEventListener("submit", (event) => {
    event.preventDefault();
    act("join", { bet: buyIn.value.trim() });
  });
  raiseForm.addEventListener("submit", (event) => {
    event.preventDefault();
    if (Number.isSafeInteger(raiseInput.valueAsNumber)) act("raise", { amount: raiseInput.valueAsNumber });
  });

  function reset() {
    generation++;
    pendingPoll = null;
    pendingAction = null;
    pollError = false;
    state = undefined;
    tableKey = null;
    setText(feedback, "");
  }

  function select(next) {
    if (mode === next) return;
    mode = next;
    reset();
    render();
    poll();
  }

  return {
    show() {
      if (visible) return;
      visible = true;
      render();
      poll();
      timer = setInterval(() => {
        renderClock();
        poll();
      }, 1000);
    },
    hide() {
      visible = false;
      clearInterval(timer);
      reset();
    },
  };
};
