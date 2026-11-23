"use strict";

function createDurakSolo({ root, request }) {
  const suits = {
    "♣": ["clubs", "трефы"],
    "♦": ["diamonds", "бубны"],
    "♥": ["hearts", "червы"],
    "♠": ["spades", "пики"],
  };
  const faces = { J: "валет", Q: "дама", K: "король", A: "туз" };
  const results = { won: "Вы выиграли.", lost: "Бот выиграл.", draw: "Ничья." };

  function element(tag, className = "", text = "") {
    const node = document.createElement(tag);
    node.className = className;
    node.textContent = text;
    return node;
  }

  function button(text, onClick, className = "") {
    const node = element("button", className, text);
    node.type = "button";
    node.addEventListener("click", onClick);
    return node;
  }

  function label(card) {
    return `${faces[card.rank] ?? card.rank}, ${suits[card.suit][1]}`;
  }

  function cardFace(card, node = element("span", "casino-card")) {
    node.textContent = card.rank + card.suit;
    node.dataset.suit = suits[card.suit][0];
    return node;
  }

  const title = element("h2", "", "Подкидной дурак");
  title.id = "durak-title";
  root.setAttribute("aria-labelledby", title.id);
  const header = element("header", "casino-header");
  header.append(title, element("p", "durak-format", "Против бота · 36 карт"));

  const opponent = element("div", "durak-counter");
  const opponentCount = element("strong");
  opponent.append(element("span", "casino-card casino-card--back durak-mini-card"), opponentCount);
  const deck = element("div", "durak-counter");
  const stock = element("span", "casino-deck");
  const deckCount = element("strong");
  deck.append(stock, deckCount);
  const trump = element("div", "durak-counter durak-trump");
  const trumpCard = element("span", "casino-card durak-mini-card");
  trump.append(trumpCard, element("strong", "", "Козырь"));
  const counters = element("div", "durak-counters");
  counters.append(opponent, deck, trump);

  const board = element("section", "durak-board");
  const boardTitle = element("h2", "", "Стол");
  const table = element("ol", "durak-table");
  table.setAttribute("aria-label", "Карты на столе");
  const emptyTable = element("p", "durak-empty", "На столе пока нет карт.");
  board.append(boardTitle, table, emptyTable);

  const handTitle = element("h2", "", "Ваши карты");
  const hand = element("div", "durak-hand");
  hand.setAttribute("role", "group");
  hand.setAttribute("aria-label", "Карты в руке");
  hand.addEventListener("keydown", navigateHand);
  const emptyHand = element("p", "durak-empty", "В руке нет карт.");
  const field = element("section", "casino-panel durak-field");
  field.append(counters, board, handTitle, hand, emptyHand);

  const feedback = element("p", "casino-feedback durak-feedback");
  feedback.setAttribute("role", "status");
  feedback.setAttribute("aria-atomic", "true");
  feedback.tabIndex = -1;
  const start = button("Начать игру", onStart, "casino-button");
  const take = button("Взять карты", () => move("take"), "casino-button");
  const pass = button("Завершить атаку", () => move("pass"), "casino-button");
  const retry = button("Обновить состояние", () => sync(), "casino-button");
  const cancel = button("Продолжить игру", cancelRestart);
  const restart = button("Да, начать заново", () => mutate("start", {}), "casino-button");
  const toolbar = element("div", "casino-toolbar durak-actions");
  toolbar.append(take, pass, start, retry, cancel, restart);
  const controls = element("section", "casino-panel durak-controls");
  controls.append(toolbar);
  root.replaceChildren(header, feedback, field, controls);

  let visible = false;
  let view = 0;
  let reading = false;
  let pending = false;
  let ready = false;
  let confirming = false;
  let game = null;
  let notice = "";
  let cards = [];

  function canAct() {
    return visible && ready && !pending && !reading;
  }

  function canMove(action) {
    return game?.status === "playing" && game.actions.includes(action);
  }

  function cardAction() {
    if (game?.phase === "defend") return canMove("defend") ? "defend" : null;
    if (game?.phase === "attack" || game?.phase === "throw") {
      return canMove("attack") ? "attack" : null;
    }
    return null;
  }

  function showCard(card, description) {
    const slot = element("span", "durak-slot");
    const face = cardFace(card);
    face.setAttribute("aria-hidden", "true");
    slot.append(face, element("span", "durak-sr-only", description + " — " + label(card)));
    return slot;
  }

  function renderBoard() {
    if (!game) {
      cards = [];
      hand.replaceChildren();
      table.replaceChildren();
      return;
    }
    opponentCount.textContent = `У бота · ${game.opponent_count}`;
    deckCount.textContent = `В колоде · ${game.deck_count}`;
    stock.hidden = game.deck_count === 0;
    cardFace(game.trump, trumpCard);
    trumpCard.setAttribute("aria-label", label(game.trump));
    trumpCard.setAttribute("role", "img");
    table.replaceChildren(...game.table.map(({ attack, defense }) => {
      const pair = element("li", "durak-pair");
      pair.append(showCard(attack, "Атака"), defense
        ? showCard(defense, "Защита")
        : element("span", "durak-uncovered", "Не отбито"));
      return pair;
    }));
    emptyTable.hidden = game.table.length > 0;
    table.hidden = !emptyTable.hidden;
    emptyHand.hidden = game.hand.length > 0;

    const legal = new Set(game.legal_cards);
    const action = cardAction();
    const verb = game.phase === "throw" ? "Подкинуть" : action === "defend" ? "Отбить" : "Сходить";
    cards = game.hand.map((card) => {
      const node = cardFace(card, button("", () => {
        const currentAction = cardAction();
        if (currentAction && game.legal_cards.includes(card.id)) move(currentAction, card.id);
      }, "casino-card durak-hand-card"));
      node.dataset.cardId = String(card.id);
      node.disabled = !action || !legal.has(card.id);
      node.setAttribute("aria-label", node.disabled ? label(card) : `${verb} — ${label(card)}`);
      return node;
    });
    hand.replaceChildren(...cards);
  }

  function statusText() {
    if (pending) return "Ждём ответ от игры…";
    if (reading || (!ready && !notice)) return "Загружаем партию…";
    if (!ready) return notice;
    if (confirming) return "Текущая партия завершится. Начать заново?";
    if (!game) return [notice, "Начните партию с ботом."].filter(Boolean).join(" ");

    const hints = {
      attack: "Ваш ход. Выберите карту для атаки.",
      defend: cards.some((card) => !card.disabled)
        ? "Защищайтесь. Выберите карту, чтобы отбиться, или возьмите карты со стола."
        : "Возьмите карты со стола.",
      throw: cards.some((card) => !card.disabled)
        ? "Подкиньте карту того же достоинства или завершите ход."
        : "Завершите атаку.",
    };
    const status = game.message || (results[game.status] ?? hints[game.phase]);
    return [notice, status].filter(Boolean).join(" ");
  }

  function render(updateBoard = false) {
    const focused = document.activeElement;
    const handIndex = cards.indexOf(focused);
    const hadControl = handIndex >= 0 || [take, pass, start, retry, cancel, restart].includes(focused);
    if (updateBoard) renderBoard();

    field.hidden = !game;
    start.hidden = confirming || (!ready && !game);
    start.textContent = !game ? "Начать игру" : game.status === "playing" ? "Начать заново" : "Сыграть ещё";
    take.hidden = confirming || !canMove("take");
    pass.hidden = confirming || !canMove("pass");
    retry.hidden = ready || !notice;
    cancel.hidden = restart.hidden = !confirming;
    const blocked = !canAct();
    for (const control of [start, take, pass, cancel, restart]) control.setAttribute("aria-disabled", String(blocked));
    for (const card of cards) card.setAttribute("aria-disabled", String(blocked || confirming || card.disabled));
    retry.setAttribute("aria-disabled", String(reading || pending));

    const text = statusText();
    if (feedback.textContent !== text) feedback.textContent = text;
    if (visible && hadControl && (focused.hidden || (updateBoard && handIndex >= 0))) {
      const playable = cards.filter((card) => !card.disabled);
      const nextCard = playable.find((card) => card.dataset.cardId === focused.dataset.cardId)
        ?? cards.slice(handIndex).find((card) => !card.disabled) ?? playable.at(-1);
      const next = [cancel, retry, focused === cancel ? start : nextCard, take, pass, start]
        .find((node) => node && !node.hidden);
      (next ?? feedback).focus();
    }
  }

  function navigateHand(event) {
    if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    const playable = cards.filter((card) => !card.disabled);
    const index = playable.indexOf(event.target);
    if (index < 0) return;
    let next;
    switch (event.key) {
      case "ArrowLeft": next = playable[index - 1]; break;
      case "ArrowRight": next = playable[index + 1]; break;
      case "Home": next = playable[0]; break;
      case "End": next = playable.at(-1); break;
      default: return;
    }
    event.preventDefault();
    next?.focus();
  }

  function accept(response) {
    const next = response?.game;
    if (next !== null && typeof next?.revision !== "string") throw new Error("Missing Durak state");
    game = next;
    ready = true;
  }

  async function sync(recovery) {
    if (!visible || reading || pending) return;
    const current = view;
    ready = false;
    reading = true;
    render();
    try {
      const response = await request("state", {});
      if (current !== view) return;
      accept(response);
      notice = recovery
        ? (game?.revision === recovery.revision
          ? recovery.message
          : "Показано последнее сохранённое состояние.")
        : "";
    } catch {
      if (current === view) notice = "Не удалось загрузить партию. Обновите состояние, чтобы продолжить.";
    } finally {
      if (current === view) {
        reading = false;
        render(ready);
      }
    }
  }

  async function mutate(endpoint, fields) {
    if (!canAct()) return;
    const current = view;
    const revision = game?.revision;
    pending = true;
    notice = "";
    render();
    let recovery;
    try {
      accept(await request(endpoint, fields));
    } catch {
      ready = false;
      recovery = { revision, message: endpoint === "start"
        ? "Не удалось начать партию."
        : "Не удалось подтвердить ход." };
    }
    pending = false;
    confirming = false;
    if (!visible) return;
    if (current !== view || recovery) {
      void sync(recovery);
      return;
    }
    render(true);
    if (!document.hidden) window.GameAudio?.play(endpoint === "start" ? "cards.deal" : "cards.place");
  }

  function move(action, card) {
    if (!canAct() || confirming || !canMove(action)) return;
    const fields = { revision: game.revision, action };
    if (card !== undefined) fields.card = card;
    void mutate("move", fields);
  }

  function onStart() {
    if (!canAct() || confirming) return;
    if (game?.status === "playing") {
      confirming = true;
      render();
    } else {
      void mutate("start", {});
    }
  }

  function cancelRestart() {
    if (!canAct() || !confirming) return;
    confirming = false;
    render();
  }

  render();
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
      view++;
      reading = false;
      confirming = false;
    },
  };
}

(window.CasinoGames ||= {}).durak = function ({ root, request }) {
  const heading = document.createElement("h1");
  heading.textContent = "Дурак";
  const header = document.createElement("header");
  header.className = "casino-header";
  const modes = document.createElement("div");
  modes.className = "casino-toolbar casino-modes";
  modes.setAttribute("role", "group");
  modes.setAttribute("aria-label", "Режим игры");
  header.append(heading, modes);
  root.replaceChildren(header);

  let visible = false;
  const tabs = [
    ["Открытые столы", window.createDurakOnline],
    ["С ботом", createDurakSolo],
  ].filter(([, mount]) => typeof mount === "function").map(([label, mount]) => {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = label;
    const panel = document.createElement("section");
    panel.className = "durak-mode-panel";
    panel.hidden = true;
    modes.append(button);
    root.append(panel);
    const tab = { button, panel, mount, view: null };
    button.addEventListener("click", () => {
      if (visible && tab !== current) select(tab);
    });
    return tab;
  });
  let current = tabs[0];

  function select(tab) {
    if (tab !== current) current.view?.hide();
    current = tab;
    for (const { button, panel } of tabs) {
      const selected = panel === current.panel;
      button.setAttribute("aria-pressed", String(selected));
      panel.hidden = !selected;
    }
    current.view ??= current.mount({ root: current.panel, request });
    current.view.show();
  }

  return {
    show() {
      if (visible) return;
      visible = true;
      select(current);
    },
    hide() {
      if (!visible) return;
      visible = false;
      current.view?.hide();
    },
  };
};
