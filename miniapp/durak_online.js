"use strict";

window.createDurakOnline = function ({ root, request, avatar }) {
  const modes = { throw: "Подкидной", transfer: "Переводной" };
  const actionLabels = {
    attack: "Подкинуть", defend: "Отбить", transfer: "Перевести", take: "Забрать", pass: "Пас", cheat: "Обменять карту",
  };
  const seatOptions = [2, 3, 4, 5, 6].map(count => [String(count), `${count} ${count < 5 ? "игрока" : "игроков"}`]);
  const deckLabels = { 24: "24 карты", 36: "36 карт", 52: "52 карты" };
  const tempos = { slow: "Медленная · 45 с", fast: "Быстрая · 15 с" };
  const emojis = ["👍", "😂", "😮", "👏", "😎", "😤"];
  const amounts = new Intl.NumberFormat("ru-RU");
  const minStake = 10;
  const maxStake = 1_000_000;

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
    setOptions(control, options);
    return control;
  }

  function setOptions(control, options) {
    control.replaceChildren(...options.map(([value, text]) => {
      const option = node("option", text);
      option.value = value;
      return option;
    }));
    control.value = options[0][0];
  }

  function labelled(label, control, className = "") {
    const wrapper = node("label", "", className);
    wrapper.append(node("span", label), control);
    return wrapper;
  }

  function input(label, type) {
    const control = node("input");
    control.type = type;
    control.setAttribute("aria-label", label);
    return control;
  }

  function settingsBadges(room) {
    const badges = node("div", "", "durak-room-badges");
    const settings = [
      modes[room.mode],
      deckLabels[room.deck] ?? deckLabels[36],
      room.cheating ? "С шулерством" : "Честная игра",
      room.throwers === "neighbors" ? "Подкидывают соседи" : "Подкидывают все",
      tempos[room.speed] ?? tempos.slow,
      room.stake > 0 ? `Ставка ${amounts.format(room.stake)} монет` : "Бесплатно",
    ];
    for (const setting of settings) badges.append(node("span", setting, "durak-room-badge"));
    return badges;
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
  const form = node("form", "", "durak-create-form");
  const rules = select("Режим стола", Object.entries(modes));
  const capacity = select("Число игроков", seatOptions);
  const deck = select("Колода", [["24", "24 карты · 9–A"], ["36", "36 карт · 6–A"], ["52", "52 карты · 2–A"]]);
  deck.value = "36";
  const throwers = select("Кто может подкидывать", [["all", "Все игроки"], ["neighbors", "Соседи и атакующий"]]);
  const speed = select("Темп игры", Object.entries(tempos));
  const cheating = input("Разрешить шулерство", "checkbox");
  cheating.title = "Один тайный обмен карты с колодой за партию, если в ней остались хотя бы две карты.";
  const free = input("Играть бесплатно", "checkbox");
  free.checked = true;
  const stake = input("Ставка в монетах", "number");
  const slider = input("Размер ставки", "range");
  stake.inputMode = "numeric";
  stake.required = true;
  for (const control of [stake, slider]) {
    control.min = String(minStake);
    control.max = String(maxStake);
    control.step = "1";
    control.value = String(minStake);
  }
  const stakeFields = node("div", "", "durak-stake-fields");
  stakeFields.append(labelled("Ставка в монетах", stake), slider);
  const submit = node("button", "Создать стол", "casino-button");
  submit.type = "submit";
  form.append(
    labelled("Режим", rules), labelled("Колода", deck), labelled("Число игроков", capacity),
    labelled("Кто может подкидывать", throwers), labelled("Темп", speed),
    labelled("Разрешить шулерство", cheating, "durak-create-toggle"),
    labelled("Играть бесплатно", free, "durak-create-toggle"), stakeFields, submit,
  );
  create.append(node("summary", "Создать своё лобби"), form);
  lobby.append(tools, list, create);

  const roomPanel = node("section", "", "casino-panel durak-online-room");
  const note = node("p", "", "casino-feedback");
  note.setAttribute("role", "status");
  const clock = node("span", "", "casino-timer durak-online-clock");
  clock.setAttribute("role", "timer");
  clock.setAttribute("aria-label", "До конца хода");
  clock.hidden = true;
  const palette = node("div", "", "durak-reaction-palette");
  palette.setAttribute("role", "group");
  palette.setAttribute("aria-label", "Отправить реакцию");
  palette.hidden = true;
  for (const emoji of emojis) palette.append(button(emoji, () => { void react(emoji); }));
  root.replaceChildren(lobby, roomPanel, note);

  let visible = false;
  let generation = 0;
  let timer;
  let countdownTimer;
  let deadline = null;
  let pending = false;
  let reading = false;
  let snapshot = null;
  let selected = null;
  let notice = "";
  let ready = false;
  let reactionTarget = null;
  let reactionPending = false;
  let nextReactionAt = 0;
  let avatarRoom = null;
  const avatarCache = new Map();
  const seats = new Map();

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
      description.append(node("strong", room.players.map(player => player.name).join(", ")), settingsBadges(room));
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
    const legalActions = room.status === "playing" ? game.actions : [];
    const stock = node("div", "", "durak-online-stock");
    stock.append(node("span", `В колоде: ${game.deck_count}`), node("span", "Козырь"), makeCard(game.trump));
    const turn = node("div", "", "durak-online-turn");
    turn.append(node("p", game.message), clock);
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
      const legal = legalActions.filter(action => game.legal_cards[action]?.includes(card.id));
      const face = makeCard(card, () => {
        if (legal.length === 1 && legal[0] !== "cheat") move(room, legal[0], card.id);
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
    for (const action of legalActions) {
      const takesCard = Object.hasOwn(game.legal_cards, action);
      const needsSelection = takesCard && !game.legal_cards[action].includes(selected);
      if (!Object.hasOwn(actionLabels, action) || (needsSelection && action !== "cheat")) continue;
      if (action === "cheat" && !game.legal_cards.cheat?.length) continue;
      const control = button(actionLabels[action], () => move(room, action, takesCard ? selected : undefined));
      control.disabled = pending || !ready || needsSelection;
      if (action === "cheat") control.title = "Выберите карту для тайного обмена с колодой.";
      actions.append(control);
    }
    field.append(stock, turn, board, node("h3", "Ваши карты"), hand, actions);
    return field;
  }

  function renderRoom(room, you) {
    retainAvatars(room);
    const header = node("div", "", "casino-table-status");
    header.append(node("h2", "Стол"), node("span", `${room.players.length}/${room.capacity}`, "casino-timer"));
    const players = node("ul", "", "durak-player-list");
    seats.clear();
    for (const player of room.players) {
      const cardCount = Number.isInteger(player.cards) ? ` · ${player.cards} карт` : "";
      const playerLabel = `${player.name}${player.id === you ? " (вы)" : ""}${cardCount}`;
      const seat = node("li");
      const trigger = button("", () => {
        reactionTarget = reactionTarget === player.id ? null : player.id;
        updatePalette();
      }, "durak-avatar");
      trigger.setAttribute("aria-label", `Реакции · ${player.name}`);
      trigger.disabled = !ready;
      trigger.append(avatarFace(room, player));
      const reaction = node("span", "", "durak-player-reaction");
      reaction.hidden = true;
      seat.append(trigger, node("span", playerLabel), reaction);
      seats.set(player.id, { trigger, reaction });
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
    roomPanel.replaceChildren(header, settingsBadges(room), players, palette);
    updateReactions();
    updatePalette();
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
    else {
      retainAvatars(null);
      seats.clear();
      roomPanel.replaceChildren();
      renderLobby(snapshot?.rooms ?? []);
    }
  }

  function retainAvatars(room) {
    const roomId = room?.id ?? null;
    if (roomId !== avatarRoom) reactionTarget = null;
    for (const [id, entry] of avatarCache) {
      if (roomId === avatarRoom && room?.players.some(player => player.id === id)) continue;
      if (entry.url) URL.revokeObjectURL(entry.url);
      avatarCache.delete(id);
    }
    avatarRoom = roomId;
  }

  function avatarFace(room, player) {
    let entry = avatarCache.get(player.id);
    if (!entry) {
      const initial = Array.from(player.name.trim())[0]?.toLocaleUpperCase() ?? "?";
      entry = { face: node("span", initial, "durak-avatar-face"), requested: false };
      avatarCache.set(player.id, entry);
    }
    if (avatar && visible && ready && !entry.requested) {
      entry.requested = true;
      void loadAvatar(room.id, player.id, entry);
    }
    return entry.face;
  }

  async function loadAvatar(roomId, playerId, entry) {
    try {
      const blob = await avatar(roomId, playerId);
      if (!blob || avatarCache.get(playerId) !== entry) return;
      const image = node("img");
      image.alt = "";
      image.hidden = true;
      image.addEventListener("load", () => { image.hidden = false; });
      entry.url = URL.createObjectURL(blob);
      image.src = entry.url;
      entry.face.append(image);
    } catch {
      // The initials remain visible if the avatar is unavailable.
    }
  }

  function updateReactions() {
    for (const player of snapshot?.room?.players ?? []) {
      const reaction = seats.get(player.id)?.reaction;
      if (!reaction) continue;
      const emoji = emojis.includes(player.reaction) ? player.reaction : "";
      if (reaction.textContent !== emoji) reaction.textContent = emoji;
      reaction.hidden = !emoji;
    }
  }

  function updatePalette() {
    if (!seats.has(reactionTarget)) reactionTarget = null;
    palette.hidden = reactionTarget === null || !ready;
    for (const [id, { trigger }] of seats) {
      trigger.setAttribute("aria-expanded", String(!palette.hidden && id === reactionTarget));
    }
  }

  function closePalette() {
    const trigger = seats.get(reactionTarget)?.trigger;
    reactionTarget = null;
    updatePalette();
    trigger?.focus();
  }

  async function react(emoji) {
    const room = snapshot?.room;
    if (!visible || !room || !ready || palette.hidden || reactionPending) return;
    if (performance.now() < nextReactionAt) {
      note.textContent = notice = "Реакцию можно отправлять раз в 2 секунды.";
      return;
    }
    const version = generation;
    reactionPending = true;
    nextReactionAt = performance.now() + 2000;
    note.textContent = notice = "";
    closePalette();
    try {
      await request("online_react", { room_id: room.id, emoji });
      if (version === generation) void sync();
    } catch (error) {
      if (version === generation) note.textContent = notice = error.message || "Не удалось отправить реакцию.";
    } finally {
      reactionPending = false;
    }
  }

  function updateClock() {
    clock.hidden = deadline === null;
    const seconds = deadline === null ? 0 : Math.max(0, Math.ceil((deadline - performance.now()) / 1000));
    const label = `${seconds} с`;
    if (clock.textContent !== label) clock.textContent = label;
    clock.dataset.urgent = String(seconds <= 5);
    if (seconds === 0) stopCountdown();
  }

  function stopCountdown() {
    clearInterval(countdownTimer);
    countdownTimer = undefined;
  }

  function syncClock(room) {
    const ms = room?.game?.remaining_ms;
    deadline = room?.status === "playing" && Number.isFinite(ms) ? performance.now() + Math.max(0, ms) : null;
    updateClock();
    if (visible && !document.hidden && deadline !== null && ms > 0 && countdownTimer === undefined) {
      countdownTimer = setInterval(updateClock, 250);
    }
  }

  function roomContent(key, value) {
    if (key !== "remaining_ms" && key !== "reaction") return value;
  }

  function accept(next) {
    if (!Array.isArray(next?.rooms) || typeof next.you !== "string") throw new Error("Не удалось загрузить столы.");
    if (next.room?.id !== snapshot?.room?.id || next.room?.game?.revision !== snapshot?.room?.game?.revision) selected = null;
    const changed = !ready || snapshot?.you !== next.you || (next.room || snapshot?.room
      ? JSON.stringify(snapshot?.room, roomContent) !== JSON.stringify(next.room, roomContent)
      : JSON.stringify(snapshot?.rooms) !== JSON.stringify(next.rooms));
    snapshot = next;
    ready = true;
    if (changed) render();
    else updateReactions();
    syncClock(next.room);
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
      if (visible && !document.hidden && endpoint === "online_move" && data.action !== "cheat") window.GameAudio?.play("cards.place");
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
  palette.addEventListener("keydown", event => {
    if (event.key === "Escape") closePalette();
  });
  deck.addEventListener("change", () => {
    const maxSeats = deck.value === "24" ? 4 : 6;
    const count = Math.min(Number(capacity.value), maxSeats);
    setOptions(capacity, seatOptions.slice(0, maxSeats - 1));
    capacity.value = String(count);
  });
  function showStake() {
    stakeFields.hidden = free.checked;
    stake.disabled = free.checked;
    slider.disabled = free.checked;
  }
  function validStake(value) {
    return Number.isInteger(value) && value >= minStake && value <= maxStake;
  }
  showStake();
  free.addEventListener("change", showStake);
  slider.addEventListener("input", () => { stake.value = slider.value; });
  stake.addEventListener("input", () => {
    if (validStake(Number(stake.value))) slider.value = stake.value;
  });
  document.addEventListener("visibilitychange", () => {
    if (!visible) return;
    if (document.hidden) {
      clearTimeout(timer);
      stopCountdown();
    } else {
      updateClock();
      void sync();
    }
  });
  form.addEventListener("submit", event => {
    event.preventDefault();
    const amount = free.checked ? 0 : Number(stake.value);
    if (!free.checked && !validStake(amount)) return;
    void act("online_create", {
      mode: rules.value,
      capacity: Number(capacity.value),
      deck: Number(deck.value),
      cheating: cheating.checked,
      throwers: throwers.value,
      speed: speed.value,
      stake: amount,
    });
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
      stopCountdown();
      retainAvatars(null);
      updatePalette();
    },
  };
};
