"use strict";

window.createDurakOnline = function ({ root, request }) {
  const modes = { throw: "Подкидной", transfer: "Переводной" };
  const actionLabels = { attack: "Подкинуть", defend: "Отбить", transfer: "Перевести", take: "Забрать", pass: "Пас" };
  const seatOptions = [2, 3, 4, 5, 6].map(count => [String(count), `${count} ${count < 5 ? "игрока" : "игроков"}`]);

  function node(tag, text = "", className = "") {
    const element = document.createElement(tag);
    element.textContent = text;
    element.className = className;
    return element;
  }

  function button(label, click, className = "") {
    const control = node("button", label, className);
    control.type = "button";
    control.addEventListener("click", click);
    return control;
  }

  function select(label, options) {
    const control = node("select");
    control.setAttribute("aria-label", label);
    for (const [value, text] of options) {
      const option = node("option", text);
      option.value = value;
      control.append(option);
    }
    return control;
  }

  const lobby = node("section", "", "casino-panel durak-lobby");
  const filter = select("Фильтр режима", [["all", "Все режимы"], ...Object.entries(modes)]);
  const sizeFilter = select("Фильтр числа мест", [["all", "Любой размер"], ...seatOptions]);
  const tools = node("div", "", "casino-table-status");
  const filters = node("div", "", "casino-toolbar");
  filters.append(filter, sizeFilter);
  tools.append(node("h2", "Открытые столы"), filters);
  const list = node("ul", "", "durak-room-list");
  const create = node("details", "", "durak-create");
  const form = node("form", "", "casino-toolbar");
  const rules = select("Режим стола", Object.entries(modes));
  const capacity = select("Число игроков", seatOptions);
  const submit = node("button", "Создать стол", "casino-button");
  submit.type = "submit";
  form.append(rules, capacity, submit);
  create.append(node("summary", "Создать своё лобби"), form);
  lobby.append(tools, list, create);

  const roomPanel = node("section", "", "casino-panel durak-online-room");
  const note = node("p", "", "casino-feedback");
  note.setAttribute("role", "status");
  root.replaceChildren(lobby, roomPanel, note);

  let visible = false;
  let generation = 0;
  let timer;
  let pending = false;
  let reading = false;
  let snapshot = null;
  let selected = null;
  let notice = "";
  let ready = false;

  function makeCard(card, onClick) {
    const face = onClick ? button(`${card.rank}${card.suit}`, onClick, "casino-card durak-hand-card")
      : node("span", `${card.rank}${card.suit}`, "casino-card");
    face.dataset.suit = { "♥": "hearts", "♦": "diamonds", "♣": "clubs", "♠": "spades" }[card.suit];
    return face;
  }

  function renderLobby(rooms) {
    const shown = rooms.filter(room => (filter.value === "all" || room.mode === filter.value) &&
      (sizeFilter.value === "all" || String(room.capacity) === sizeFilter.value));
    if (!shown.length) {
      list.replaceChildren(node("li", snapshot ? "Нет открытых столов. Создайте свой или смените фильтр." : "Загружаем столы…", "durak-empty"));
      return;
    }
    list.replaceChildren(...shown.map(room => {
      const row = node("li", "", "durak-room-row");
      const description = node("div");
      description.append(node("strong", room.players.map(player => player.name).join(", ")),
        node("small", `${modes[room.mode]} · 36 карт`));
      const seats = node("span", `${room.players.length}/${room.capacity}`, "durak-seats");
      const join = button("Войти", () => act("online_join", { room_id: room.id }));
      join.disabled = pending || !ready;
      row.append(description, seats, join);
      return row;
    }));
  }

  function renderTable(room) {
    const game = room.game;
    const field = node("div", "", "durak-field");
    if (!game) return field;
    const stock = node("div", "", "durak-online-stock");
    stock.append(node("span", `В колоде: ${game.deck_count}`), node("span", "Козырь"), makeCard(game.trump));
    const turn = node("p", game.message, "durak-online-turn");
    const board = node("div", "", "durak-board durak-table");
    if (!game.table.length) board.append(node("span", "Стол пуст", "durak-empty"));
    for (const pair of game.table) {
      const cards = node("div", "", "durak-pair");
      cards.append(makeCard(pair.attack));
      if (pair.defense) cards.append(makeCard(pair.defense));
      board.append(cards);
    }
    const hand = node("div", "", "durak-hand");
    for (const card of game.hand) {
      const legal = game.actions.filter(action => game.legal_cards[action]?.includes(card.id));
      const face = makeCard(card, () => {
        if (legal.length === 1) move(room, legal[0], card.id);
        else {
          selected = card.id;
          render();
          roomPanel.querySelector(".durak-online-actions button:not(:disabled)")?.focus();
        }
      });
      face.disabled = pending || !ready || !legal.length;
      face.setAttribute("aria-pressed", String(card.id === selected));
      hand.append(face);
    }
    const actions = node("div", "", "casino-toolbar durak-online-actions");
    for (const action of game.actions) {
      const takesCard = Object.hasOwn(game.legal_cards, action);
      if ((takesCard && !game.legal_cards[action].includes(selected)) || !Object.hasOwn(actionLabels, action)) continue;
      const control = button(actionLabels[action], () => move(room, action, takesCard ? selected : undefined));
      control.disabled = pending || !ready;
      actions.append(control);
    }
    field.append(stock, turn, board, node("h3", "Ваши карты"), hand, actions);
    return field;
  }

  function renderRoom(room, you) {
    const header = node("div", "", "casino-table-status");
    header.append(node("h2", modes[room.mode]), node("span", `${room.players.length}/${room.capacity} · 36 карт`, "casino-timer"));
    const players = node("ul", "", "durak-player-list");
    for (const player of room.players) {
      const cardCount = Number.isInteger(player.cards) ? ` · ${player.cards} карт` : "";
      const playerLabel = `${player.name}${player.id === you ? " (вы)" : ""}${cardCount}`;
      const seat = node("li", playerLabel);
      if (room.game?.turn === player.id && room.status === "playing") seat.dataset.turn = "true";
      players.append(seat);
    }
    const controls = node("div", "", "casino-toolbar");
    if (room.status === "waiting" && room.host === you) {
      const start = button("Начать игру", () => act("online_start", { room_id: room.id }), "casino-button");
      start.disabled = pending || !ready || room.players.length < 2;
      controls.append(start);
    }
    const leave = button(room.status === "finished" ? "Назад к столам" : "Покинуть стол", () => {
      if (room.status === "playing" && !window.confirm("Выход завершит текущую партию. Покинуть стол?")) return;
      act("online_leave", { room_id: room.id });
    });
    leave.disabled = pending || !ready;
    controls.append(leave);
    roomPanel.replaceChildren(header, players);
    if (room.status === "waiting") {
      const hint = room.players.length < 2 ? "Для начала игры нужны как минимум два участника."
        : room.host === you ? "Можно начать игру или дождаться других участников." : "Ждём, пока ведущий начнёт игру.";
      roomPanel.append(node("p", hint, "durak-empty"));
    }
    else roomPanel.append(renderTable(room));
    roomPanel.append(controls);
  }

  function render() {
    const room = snapshot?.room;
    lobby.hidden = Boolean(room);
    roomPanel.hidden = !room;
    submit.disabled = pending || !ready;
    note.textContent = notice;
    if (room) renderRoom(room, snapshot.you);
    else renderLobby(snapshot?.rooms ?? []);
  }

  function accept(next) {
    if (!Array.isArray(next?.rooms) || typeof next.you !== "string") throw new Error("Не удалось загрузить столы.");
    if (next.room?.game?.revision !== snapshot?.room?.game?.revision) selected = null;
    const changed = !ready || snapshot?.you !== next.you || (next.room || snapshot?.room
      ? JSON.stringify(snapshot?.room) !== JSON.stringify(next.room)
      : JSON.stringify(snapshot?.rooms) !== JSON.stringify(next.rooms));
    snapshot = next;
    ready = true;
    if (changed) render();
  }

  function schedule() {
    clearTimeout(timer);
    if (visible && !document.hidden && !pending) timer = setTimeout(sync, 2200);
  }

  async function sync() {
    if (!visible || document.hidden || pending || reading) return;
    const version = generation;
    reading = true;
    try {
      const data = await request("online_state", {});
      if (version === generation) accept(data);
    } catch (error) {
      if (version === generation) {
        ready = false;
        notice = error.message || "Не удалось загрузить столы.";
        render();
      }
    } finally {
      reading = false;
      schedule();
    }
  }

  async function act(endpoint, data) {
    if (!visible || pending || !ready) return;
    generation++;
    pending = true;
    notice = "";
    render();
    try {
      accept(await request(endpoint, data));
      if (visible && !document.hidden && endpoint === "online_move") window.GameAudio?.play("cards.place");
    } catch (error) {
      ready = false;
      notice = error.message || "Не удалось выполнить действие.";
    } finally {
      pending = false;
      if (visible) {
        render();
        if (!ready && !reading) void sync();
        else schedule();
      }
    }
  }

  function move(room, action, card) {
    const fields = { room_id: room.id, revision: room.game.revision, action };
    if (card !== undefined) fields.card = card;
    void act("online_move", fields);
  }

  filter.addEventListener("change", render);
  sizeFilter.addEventListener("change", render);
  document.addEventListener("visibilitychange", () => {
    if (!visible) return;
    if (document.hidden) clearTimeout(timer);
    else void sync();
  });
  form.addEventListener("submit", event => {
    event.preventDefault();
    void act("online_create", { mode: rules.value, capacity: Number(capacity.value) });
  });
  return {
    show() {
      if (visible) return;
      visible = true;
      ready = false;
      notice = "";
      render();
      void sync();
    },
    hide() {
      visible = false;
      generation++;
      clearTimeout(timer);
    },
  };
};
