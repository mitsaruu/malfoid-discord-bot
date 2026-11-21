(window.CasinoGames ||= {}).blackjack = function ({ root, request }) {
  const amounts = new Intl.NumberFormat("ru");
  const outcomes = {
    blackjack: "Блэкджек",
    win: "Победа",
    push: "Ничья",
    bust: "Перебор",
    loss: "Проигрыш",
  };
  const suits = { "♥": "hearts", "♦": "diamonds", "♠": "spades", "♣": "clubs" };
  const moveLabels = { hit: "Ещё карту", stand: "Хватит", double: "Удвоить", split: "Разделить" };
  let mode = "online";
  let visible = false;
  let serial = 0;
  let timer;
  let snapshot = null;
  let busy = false;
  let syncError = "";
  let actionError = "";

  function element(tag, text = "", className = "") {
    const node = document.createElement(tag);
    node.textContent = text;
    node.className = className;
    return node;
  }

  function button(text, onClick, primary = false) {
    const control = element("button", text, primary ? "casino-button" : "");
    control.type = "button";
    control.addEventListener("click", onClick);
    return control;
  }

  const solo = button("Один на один", () => selectMode("solo"));
  const online = button("Стол сервера", () => selectMode("online"));
  const modes = element("div", "", "casino-toolbar casino-modes");
  modes.setAttribute("role", "group");
  modes.setAttribute("aria-label", "Режим блэкджека");
  modes.append(solo, online);

  const dock = element("section", "", "casino-panel casino-dock");
  const wallet = element("p", "", "casino-balance");
  const header = element("header", "", "casino-header");
  header.append(element("h1", "Блэкджек"), wallet);
  const status = element("p");
  const remaining = element("p", "", "casino-timer");
  const tableStatus = element("header", "", "casino-table-status");
  tableStatus.append(status, remaining);
  const feedback = element("p", "", "casino-feedback");
  feedback.setAttribute("role", "status");

  const betForm = element("form", "", "casino-toolbar");
  const label = element("label", "Ставка в монетах");
  const bet = element("input");
  bet.type = "number";
  bet.step = "1";
  bet.required = true;
  bet.inputMode = "numeric";
  label.append(bet);
  const allIn = button("Максимум", () => {
    bet.value = String(Math.min(snapshot.balance, snapshot.max_bet));
    bet.focus();
  });
  const join = element("button", "Занять место", "casino-button");
  join.type = "submit";
  betForm.append(label, allIn, join);
  betForm.addEventListener("submit", (event) => {
    event.preventDefault();
    void submit("join", { bet: bet.value });
  });

  const lobbyActions = element("div", "", "casino-toolbar");
  const start = button("Начать раздачу", () => submit("start"), true);
  const leave = button("Выйти и вернуть ставку", () => submit("leave"));
  lobbyActions.append(start, leave);

  const dealer = element("section", "", "blackjack-dealer");
  const dealerCards = element("div", "", "blackjack-dealer-cards");
  const emptyHand = element("div", "", "casino-cards");
  emptyHand.setAttribute("aria-hidden", "true");
  emptyHand.append(element("span", "", "casino-card casino-card--empty"),
    element("span", "", "casino-card casino-card--empty"));
  dealer.append(element("h2", "Дилер"), dealerCards);
  const moveBar = element("div", "", "casino-toolbar");
  moveBar.setAttribute("role", "group");
  moveBar.setAttribute("aria-label", "Действия с рукой");
  const moves = Object.entries(moveLabels).map(([action, label]) => {
    const control = button(label, () => submit(action), action === "hit");
    moveBar.append(control);
    return { action, control };
  });
  const players = element("div", "", "blackjack-players");
  const felt = element("section", "", "blackjack-felt");
  felt.append(tableStatus, dealer, players);
  dock.append(betForm, lobbyActions, moveBar);
  const rules = element("details", "", "casino-panel");
  rules.append(
    element("summary", "Правила блэкджека"),
    element("p", "Цель — набрать больше очков, чем дилер, не превысив 21. Туз стоит 1 или 11, картинки — 10. Дилер берёт карты до 17 и останавливается на любом 17."),
    element("p", "Победа приносит прибыль в размере ставки. Блэкджек с первых двух карт приносит 3:2 с округлением вниз до целой монеты. При ничьей ставка возвращается."),
    element("p", "Две карты одной стоимости можно разделить на две руки с одинаковыми ставками, всего до четырёх рук. Разделённым тузам дают по одной карте. Удвоение на двух картах добавляет ещё одну карту и завершает руку."),
    element("p", "Стол сервера вмещает от 2 до 7 игроков. Если раздача не началась вовремя, ставки возвращаются. Когда время хода истекает, игрок останавливается на всех своих руках."),
  );
  root.replaceChildren(header, modes, felt, dock, feedback, rules);

  function cardsFor(hand) {
    const display = element("div", "", "casino-cards");
    for (const card of hand.cards) {
      const chip = element("span", card ?? "", "casino-card");
      if (card === null) {
        chip.classList.add("casino-card--back");
        chip.setAttribute("role", "img");
        chip.setAttribute("aria-label", "Закрытая карта");
      } else {
        chip.dataset.suit = suits[card.slice(-1)];
      }
      display.append(chip);
    }
    return display;
  }

  function playerPanel(player, table, user) {
    const section = element("section", "", "casino-seat blackjack-seat");
    section.classList.toggle("casino-seat--own", player.id === user);
    section.classList.toggle("casino-seat--active", player.hands.some((hand) => hand.active));
    const name = element("h3", player.id === user ? player.name + " (вы)" : player.name);
    section.append(name);
    if (table.phase === "lobby") {
      section.append(element("p", "Ставка " + amounts.format(player.bet) + " • " +
        (player.id === table.host_id ? "Открывает раздачу" : "Готов к раздаче")));
    }
    player.hands.forEach((hand, index) => {
      const handPanel = element("div", "", "blackjack-hand");
      handPanel.classList.toggle("blackjack-hand--active", hand.active);
      const title = player.hands.length > 1 ? "Рука " + (index + 1) + " · " : "";
      const active = hand.active ? " • Ход на этой руке" : "";
      handPanel.append(element("p", title + "Очки " + hand.total + " • Ставка " + amounts.format(hand.bet) + active));
      handPanel.append(cardsFor(hand));
      if (hand.outcome) {
        const net = hand.payout - hand.bet;
        const result = net > 0 ? "+" + amounts.format(net) : amounts.format(net);
        handPanel.append(element("p", outcomes[hand.outcome] + " • " + result, "casino-feedback"));
      }
      section.append(handPanel);
    });
    return section;
  }

  function statusText(table, you, seated) {
    if (!snapshot) return syncError ? "Состояние стола недоступно." : "Загрузка стола…";
    if (!table) return mode === "solo" ? "Сыграйте против дилера." : "Стол свободен.";
    if (table.phase === "refunding") return "Возвращаем ставки. Игра продолжится, когда кошелёк будет доступен.";
    if (table.refunded) return "Раздача отменена. Ставки возвращены.";
    if (table.phase === "result") return "Раздача завершена.";
    if (table.phase === "lobby") {
      const seats = table.players.length + " из " + snapshot.max_players;
      if (seated && table.host_id !== you) return "Ждём начала раздачи • " + seats;
      if (table.players.length < 2) return "Для начала нужны два игрока • " + seats;
      return "Игроки за столом • " + seats;
    }
    if (table.current_user === you) return "Ваш ход.";
    const current = table.players.find((player) => player.id === table.current_user);
    return current ? "Ходит " + current.name : "Ход дилера.";
  }

  function render() {
    const table = snapshot?.table;
    const you = snapshot?.user_id;
    const actions = snapshot?.actions ?? [];
    const ownSeat = table?.players.find((player) => player.id === you);
    const editable = Boolean(snapshot && !syncError && !busy);
    const canBet = snapshot && (!table || table.refunded ||
      (mode === "solo" && table.phase === "result") ||
      (table.phase === "lobby" && !ownSeat && table.players.length < snapshot.max_players));
    solo.setAttribute("aria-pressed", String(mode === "solo"));
    online.setAttribute("aria-pressed", String(mode === "online"));
    solo.disabled = busy;
    online.disabled = busy;
    wallet.textContent = snapshot ? "Баланс " + amounts.format(snapshot.balance) + " монет" : "";
    status.textContent = statusText(table, you, ownSeat);
    const seconds = Math.ceil((table?.remaining_ms ?? 0) / 1000);
    remaining.textContent = ({
      lobby: "До закрытия лобби ",
      playing: "До конца хода ",
      result: mode === "online" && !table?.refunded ? "До открытия нового стола " : "",
    })[table?.phase] || "";
    remaining.hidden = !remaining.textContent;
    if (!remaining.hidden) remaining.append(seconds + " с");
    const message = actionError || syncError || (canBet && !actions.includes("join") ? "В кошельке меньше минимальной ставки." : "");
    if (feedback.textContent !== message) feedback.textContent = message;
    betForm.hidden = !canBet;
    if (canBet) {
      bet.min = String(snapshot.min_bet);
      bet.max = String(Math.min(snapshot.balance, snapshot.max_bet));
      if (!bet.value) bet.value = bet.min;
      join.textContent = mode === "solo" ? "Сдать карты" : (table?.phase === "lobby" ? "Занять место" : "Открыть стол");
      bet.disabled = allIn.disabled = join.disabled = !editable || !actions.includes("join");
    }
    lobbyActions.hidden = table?.phase !== "lobby" || !ownSeat;
    start.hidden = table?.host_id !== you;
    start.disabled = !editable || !actions.includes("start");
    leave.disabled = !editable || !actions.includes("leave");
    if (table?.dealer) {
      dealerCards.replaceChildren(cardsFor(table.dealer), element("p", "Очки " + table.dealer.total));
    } else {
      dealerCards.replaceChildren(emptyHand);
    }
    moveBar.hidden = table?.phase !== "playing" || !ownSeat;
    dock.hidden = betForm.hidden && lobbyActions.hidden && moveBar.hidden;
    for (const { action, control } of moves) control.disabled = !editable || !actions.includes(action);
    const roster = ownSeat && table.phase !== "lobby"
      ? [ownSeat, ...table.players.filter((player) => player !== ownSeat)]
      : table?.players ?? [];
    players.replaceChildren(...roster.map((player) => playerPanel(player, table, you)));
  }

  function accept(data) {
    if (typeof data?.user_id !== "string" || !Number.isFinite(data.balance) || !Array.isArray(data.actions) ||
        (data.table !== null && !Array.isArray(data.table?.players))) {
      throw new Error("Сервер вернул неполные данные стола.");
    }
    snapshot = data;
    syncError = "";
  }

  async function refresh() {
    if (!visible || busy) return;
    clearTimeout(timer);
    const token = ++serial;
    try {
      const data = await request("state", { mode });
      if (token === serial) accept(data);
    } catch (error) {
      if (token === serial) syncError = error.message || "Не удалось обновить стол.";
    } finally {
      if (visible && token === serial) {
        render();
        timer = setTimeout(refresh, snapshot?.table ? 1000 : 1900);
      }
    }
  }

  async function submit(action, fields = {}) {
    if (!visible || busy || syncError || !snapshot?.actions.includes(action)) return;
    clearTimeout(timer);
    const token = ++serial;
    const table = snapshot.table;
    busy = true;
    actionError = "";
    render();
    try {
      const data = await request(action, { ...fields, mode, round: table?.id ?? null, turn: table?.turn });
      if (token === serial) accept(data);
    } catch (error) {
      if (token === serial) actionError = error.message || "Не удалось выполнить действие.";
    } finally {
      busy = false;
      if (visible) {
        render();
        void refresh();
      }
    }
  }

  function selectMode(next) {
    if (mode === next) return;
    mode = next;
    snapshot = null;
    syncError = actionError = "";
    render();
    void refresh();
  }

  return {
    show() {
      if (visible) return;
      visible = true;
      snapshot = null;
      syncError = actionError = "";
      render();
      void refresh();
    },
    hide() {
      visible = false;
      serial++;
      clearTimeout(timer);
    },
  };
};
