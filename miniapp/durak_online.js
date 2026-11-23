"use strict";

const DurakTable = (() => {
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

  const amounts = new Intl.NumberFormat("ru-RU");
  const suits = {
    "♣": ["clubs", "трефы"], "♦": ["diamonds", "бубны"], "♥": ["hearts", "червы"], "♠": ["spades", "пики"],
  };
  const ranks = { J: "валет", Q: "дама", K: "король", A: "туз" };
  const actionLabels = {
    attack: "Подкинуть", defend: "Отбить", transfer: "Перевести", take: "Взять", pass: "Бито", cheat: "Обменять карту",
  };
  const lastLabels = { attack: "Ход", defend: "Отбой", transfer: "Перевод", take: "Взять", pass: "Пас", discard: "Бито" };
  const cardFlight = 210;
  const signalFlight = 300;

  function makeCard(card, onClick) {
    const text = `${card.rank}${card.suit}`;
    const label = `${ranks[card.rank] ?? card.rank}, ${suits[card.suit]?.[1] ?? card.suit}`;
    const face = onClick ? button(text, onClick, "durak-card durak-card--held") : node("span", text, "durak-card");
    face.dataset.suit = suits[card.suit]?.[0];
    face.dataset.cardId = String(card.id);
    face.setAttribute("aria-label", label);
    if (!onClick) face.setAttribute("role", "img");
    return face;
  }

  function makeBack() {
    const back = node("span", "", "durak-card durak-card--back");
    back.setAttribute("aria-hidden", "true");
    return back;
  }

  function anchor(element, name) {
    element.dataset.anchor = name;
    return element;
  }

  function initial(player) {
    return Array.from(player.name.trim())[0]?.toLocaleUpperCase() ?? "?";
  }

  function avatarFace(player) {
    return node("span", initial(player), "game-player-avatar");
  }

  function showAvatar(face, blob) {
    const url = URL.createObjectURL(blob);
    const image = node("img");
    image.alt = "";
    image.hidden = true;
    image.addEventListener("load", () => { image.hidden = false; });
    image.src = url;
    face.append(image);
    return url;
  }

  function create({ move, avatar, controls = [], labels = {} }) {
    const element = node("section", "", "durak-table-screen");
    element.setAttribute("aria-label", "Игровой стол");
    const arena = node("div", "", "durak-arena");
    const felt = node("div", "", "durak-felt");
    const opponents = node("ul", "", "durak-opponents");
    opponents.setAttribute("aria-label", "Соперники");
    const board = anchor(node("div", "", "durak-round-board"), "board-center");
    board.setAttribute("role", "group");
    board.setAttribute("aria-label", "Карты на столе");
    const dock = node("div", "", "durak-dock");
    const hint = node("p", "", "durak-hint");
    hint.setAttribute("role", "status");
    hint.tabIndex = -1;
    const railControls = node("div", "", "durak-rail-controls");
    const bankSlot = node("div", "", "durak-bank-slot");
    const rules = node("details", "", "durak-rules");
    const ruleContent = node("div", "", "durak-rule-content");
    rules.append(node("summary", "Правила"), ruleContent);
    railControls.append(rules, ...controls);
    const ownSeat = node("div", "", "durak-own-seat");
    const hand = anchor(node("div", "", "durak-online-hand"), "own-hand");
    hand.setAttribute("role", "group");
    hand.setAttribute("aria-label", "Карты в руке");
    const actions = node("div", "", "durak-online-actions");
    const piles = node("div", "", "durak-piles");
    const motionLayer = node("div", "", "durak-motion-layer");
    motionLayer.setAttribute("aria-hidden", "true");
    dock.append(hint, ownSeat, hand, actions);
    arena.append(felt, opponents, piles, bankSlot, board, railControls);
    element.append(arena, dock, motionLayer);

    const reduced = window.matchMedia?.("(prefers-reduced-motion: reduce)");
    const animations = new Map();
    reduced?.addEventListener?.("change", cancelMotion);
    window.addEventListener?.("resize", cancelMotion);
    let layout;
    let game;
    let you;
    let blocked = true;
    let selected = null;
    let lastId = 0;
    let renderedRules = null;
    const actionButtons = new Map();

    function seat(player, own, active) {
      const wrapper = node(own ? "div" : "li", "", `durak-table-seat${own ? " durak-table-seat--self" : ""}`);
      const profile = node("div", "", "durak-seat-profile");
      const provided = avatar?.(player, active);
      const face = provided ?? avatarFace(player);
      anchor(face, "avatar");
      if (active) {
        wrapper.dataset.turn = "true";
        wrapper.setAttribute("aria-current", "true");
        if (!provided) face.dataset.active = "true";
      }
      const count = player.cards ?? 0;
      const label = node("span", player.name, "durak-seat-name");
      label.title = own && player.name !== "Вы" ? `${player.name} (вы)` : player.name;
      const tally = node("span", String(count), "durak-seat-count");
      tally.setAttribute("aria-label", `Карт в руке: ${count}`);
      const last = node("span", game.last_action?.player === player.id
        ? lastLabels[game.last_action.kind] ?? "" : "", "durak-last-action");
      profile.append(face, label, tally, last);
      wrapper.append(profile);
      const backs = [];
      let stack;
      if (!own) {
        stack = anchor(node("div", "", "durak-opponent-hand"), "opponent-hand");
        stack.setAttribute("aria-hidden", "true");
        for (let index = 0; index < Math.min(6, count); index++) {
          const back = anchor(makeBack(), "opponent-card");
          back.style.setProperty("--fan", index - (Math.min(6, count) - 1) / 2);
          backs.push(back);
          stack.append(back);
        }
        wrapper.append(stack);
      }
      return { wrapper, face, stack, backs };
    }

    function pile(count, name, className, trump) {
      const group = node("div", "", `durak-pile ${className}`);
      group.setAttribute("role", "group");
      const cards = node("div", "", "durak-pile-cards");
      if (trump) {
        const face = makeCard(trump);
        face.className += " durak-pile-trump";
        face.setAttribute("aria-label", `Козырь · ${trump.rank}${trump.suit}`);
        cards.append(face);
      }
      const stack = anchor(makeBack(), className === "durak-stock" ? "deck" : "discard");
      const tally = node("strong", "", "durak-pile-count");
      function setCount(value) {
        group.setAttribute("aria-label", `${name}: ${value}`);
        stack.className = `durak-card durak-card--back ${value ? "durak-pile-stack" : "durak-pile-empty"}`;
        tally.textContent = String(value);
      }
      setCount(count);
      cards.append(stack);
      group.append(cards, node("span", name, "durak-pile-name"), tally);
      return { group, stack, setCount };
    }

    function renderBoard(playing) {
      return [0, 1, 2, 3, 4, 5].map(index => {
        const pair = game.table[index];
        const slot = node("div", "", "durak-round-pair");
        if (pair) {
          slot.setAttribute("role", "group");
          slot.setAttribute("aria-label", `Пара ${index + 1}`);
        } else {
          slot.setAttribute("aria-hidden", "true");
          if (playing && index === game.table.length && index < (game.attack_limit ?? 6)) slot.dataset.next = "true";
        }
        const position = kind => {
          const holder = anchor(node("div", "", `durak-board-anchor durak-board-${kind}`), `table-${kind}`);
          holder.dataset.index = String(index);
          if (pair?.[kind]) {
            const card = pair[kind];
            const face = makeCard(card);
            face.setAttribute("aria-label", `${kind === "attack" ? "Атака" : "Защита"} · ${card.rank}${card.suit}`);
            holder.append(face);
            return { card, holder, face };
          }
          holder.setAttribute("aria-hidden", "true");
          return { holder };
        };
        const attack = position("attack");
        const defense = position("defense");
        slot.append(attack.holder, defense.holder);
        return { slot, attack, defense };
      });
    }

    function setBlocked(value) {
      blocked = value;
      for (const [id, { face, legal }] of layout?.hand ?? []) {
        face.disabled = !legal.length;
        face.setAttribute("aria-disabled", String(blocked || face.disabled));
        if (legal.length > 1 || ["transfer", "cheat"].includes(legal[0])) {
          face.setAttribute("aria-pressed", String(selected === id));
        }
      }
      for (const [action, control] of actionButtons) {
        const legal = game.legal_cards[action];
        control.disabled = Boolean(legal && !legal.includes(selected));
        control.setAttribute("aria-disabled", String(blocked || control.disabled));
      }
    }

    function navigate(event, buttons) {
      if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
      const playable = buttons.filter(face => !face.disabled);
      const index = playable.indexOf(event.target);
      if (index < 0) return;
      const next = {
        ArrowLeft: playable[index - 1], ArrowRight: playable[index + 1], Home: playable[0], End: playable.at(-1),
      };
      if (!Object.hasOwn(next, event.key)) return;
      event.preventDefault();
      next[event.key]?.focus();
    }

    hand.addEventListener("keydown", event => {
      if (!blocked && layout) navigate(event, [...layout.hand.values()].map(({ face }) => face));
    });

    function update(state, { animate = false, deal = false, swap } = {}) {
      const canAnimate = animate && !reduced?.matches && !document.hidden && typeof motionLayer.animate === "function";
      const old = canAnimate && !deal && layout ? measure(layout) : null;
      cancelMotion();
      const focused = document.activeElement;
      const ownFocus = [...(layout?.hand.values() ?? [])].find(({ face }) => face === focused);
      const oldAction = [...actionButtons.keys()].find(action => actionButtons.get(action) === focused);
      const oldScroll = hand.scrollLeft ?? 0;
      if (deal || game?.revision !== state.game.revision) selected = null;
      const previousGame = deal ? null : game;
      game = state.game;
      you = state.you;
      const waitingForTaker = game.table.some(pair => !pair.defense);
      const legalActions = state.playing ? game.actions : [];
      const seats = new Map();
      for (const player of state.players) {
        seats.set(player.id, seat(player, player.id === you, state.playing && game.turn === player.id));
      }
      const own = seats.get(you);
      const clockwise = state.players.findIndex(player => player.id === you);
      const farSeats = [...state.players.slice(clockwise + 1), ...state.players.slice(0, clockwise)].map(player => seats.get(player.id).wrapper);
      opponents.replaceChildren(...farSeats);
      ownSeat.replaceChildren(own.wrapper);
      const stock = pile(game.deck_count, "Колода", "durak-stock", game.trump);
      const discard = pile(game.discard_count ?? 0, "Отбой", "durak-discard");
      piles.replaceChildren(stock.group, discard.group);
      let bank;
      if (game.bank > 0) {
        const sum = amounts.format(game.bank);
        bank = node("div", "", "durak-bank");
        bank.setAttribute("role", "group");
        bank.setAttribute("aria-label", `Банк: ${sum} монет`);
        bank.append(node("span", "", "durak-bank-chips"), node("span", "Банк", "durak-pile-name"), node("strong", sum));
      }
      bankSlot.replaceChildren(...(bank ? [bank] : []));
      const pairs = renderBoard(state.playing);
      board.replaceChildren(...pairs.map(({ slot }) => slot));
      if (state.rules.key !== renderedRules) {
        renderedRules = state.rules.key;
        ruleContent.replaceChildren(...state.rules.content());
      }
      if (hint.textContent !== game.message) hint.textContent = game.message;
      if (state.palette && state.palette.parentElement !== element) element.append(state.palette);

      const handCards = new Map();
      const spread = node("div", "", "durak-hand-spread");
      for (const [index, card] of game.hand.entries()) {
        const legal = legalActions.filter(action => game.legal_cards[action]?.includes(card.id));
        const position = node("div", "", "durak-hand-position");
        position.dataset.motion = "hand";
        const face = makeCard(card, () => {
          if (blocked || !legal.length) return;
          if (legal.length === 1 && !["transfer", "cheat"].includes(legal[0])) {
            move(legal[0], card.id);
            return;
          }
          selected = card.id;
          setBlocked(false);
          legal.map(action => actionButtons.get(action)).find(control => control && !control.disabled)?.focus();
        });
        if (legal.length) {
          face.dataset.playable = "true";
          const description = face.getAttribute("aria-label");
          const label = legal.length > 1 || ["transfer", "cheat"].includes(legal[0])
            ? "Выбрать" : legal[0] === "attack" && !game.table.length ? "Сходить" : actionLabels[legal[0]];
          face.setAttribute("aria-label", `${label} · ${description}`);
        }
        const fan = Math.max(-3, Math.min(3, index - (game.hand.length - 1) / 2));
        position.style.setProperty("--fan", fan);
        position.style.setProperty("--lift", Math.abs(fan));
        position.append(face);
        spread.append(position);
        handCards.set(card.id, { card, face, position, legal });
      }
      hand.replaceChildren(game.hand.length ? spread : node("p", "В руке нет карт.", "durak-empty-hand"));
      hand.scrollLeft = deal ? 0 : oldScroll;
      actionButtons.clear();
      for (const action of legalActions) {
        const label = labels[action] ?? (action === "pass" && waitingForTaker ? "Пас" : actionLabels[action]);
        if (!label || (Object.hasOwn(game.legal_cards, action) && !game.legal_cards[action].length)) continue;
        if (["attack", "defend"].includes(action) &&
          ![...handCards.values()].some(({ legal }) => legal.includes(action) && legal.length > 1)) continue;
        const control = button(label, () => {
          if (!blocked && !control.disabled) move(action, game.legal_cards[action] ? selected : undefined);
        });
        control.dataset.action = action;
        if (action === "transfer") control.title = "Выберите карту того же достоинства.";
        if (action === "cheat") control.title = "Выберите карту для тайного обмена с колодой.";
        actionButtons.set(action, control);
      }
      actions.replaceChildren(...actionButtons.values());
      layout = { hand: handCards, pairs, seats, stock: stock.stack, discard: discard.stack, setStockCount: stock.setCount };
      setBlocked(blocked);
      if (ownFocus || oldAction) {
        const next = [...handCards.values()].map(({ face }) => face).filter(face => !face.disabled);
        const oldControl = actionButtons.get(oldAction);
        const control = (oldControl?.disabled ? undefined : oldControl)
          ?? next.find(face => face.dataset.cardId === ownFocus?.face.dataset.cardId)
          ?? next[0] ?? [...actionButtons.values()].find(control => !control.disabled) ?? hint;
        (control.disabled ? hint : control).focus?.({ preventScroll: true });
      }

      if (deal) lastId = 0;
      const events = (game.events ?? []).filter(({ id }) => id > lastId);
      const gap = events.length && events[0].id > lastId + 1;
      lastId = Math.max(lastId, events.at(-1)?.id ?? 0);
      if (canAnimate && !gap && (old || deal)) animateChanges(old, measure(layout), events, deal, previousGame, swap);
    }

    function point(element) {
      if (!element) return;
      const box = element.getBoundingClientRect();
      if (!box.width || !box.height) return;
      return {
        x: box.left + box.width / 2, y: box.top + box.height / 2,
        width: element.offsetWidth || box.width, height: element.offsetHeight || box.height,
        angle: parseFloat(window.getComputedStyle?.(element).rotate) || 0,
      };
    }

    function measure({ hand: handCards, pairs, seats, stock, discard }) {
      const viewport = hand.getBoundingClientRect();
      const own = new Map([...handCards].map(([id, entry]) => {
        const position = point(entry.face);
        const target = position && { ...position, x: Math.max(viewport.left + position.width / 2,
          Math.min(viewport.right - position.width / 2, position.x)) };
        return [id, { ...entry, point: position, target }];
      }));
      return {
        stock: point(stock), discard: point(discard), hand: point(hand), board: point(board), own,
        pairs: pairs.map(({ attack, defense }) => [attack, defense].map(entry => ({
          ...entry, point: point(entry.holder),
        }))),
        seats: new Map([...seats].map(([id, { face, stack, backs }]) => [id, {
          face: point(face), stack: point(stack), backs: backs.map(face => ({ face, point: point(face) })),
        }])),
      };
    }

    function play(element, frames, options, cleanup, complete) {
      const animation = element.animate(frames, { easing: "cubic-bezier(.2,.7,.25,1)", ...options });
      const settle = (completed = false) => {
        if (!animations.delete(animation)) return;
        cleanup?.();
        if (completed) complete?.();
      };
      animations.set(animation, settle);
      animation.finished.then(() => settle(true), () => settle());
    }

    function cancelMotion() {
      if (animations.size) layout?.setStockCount(game.deck_count);
      for (const [animation, settle] of animations) {
        animation.cancel();
        settle();
      }
    }

    function fly(kind, content, from, to, delay, { landing, duration = cardFlight, complete } = {}) {
      if (!from || !to) {
        complete?.();
        return;
      }
      const origin = motionLayer.getBoundingClientRect();
      const ghost = node("div", "", "durak-flight");
      ghost.dataset.motion = kind;
      ghost.style.left = `${from.x - origin.left}px`;
      ghost.style.top = `${from.y - origin.top}px`;
      ghost.style.width = `${to.width}px`;
      ghost.style.height = `${to.height}px`;
      if (kind === "discard") {
        const flip = node("div", "", "durak-flight-flip");
        flip.append(content, makeBack());
        ghost.append(flip);
        play(flip, [{ transform: "rotateY(0deg)" }, { transform: "rotateY(180deg)" }], {
          delay: delay + 65, duration: 130, fill: "forwards",
        });
      } else {
        ghost.append(content);
      }
      motionLayer.append(ghost);
      const signal = content.className === "durak-flight-label";
      const start = `translate(0px, 0px) rotate(${from.angle}deg) scale(${from.width / to.width}, ${from.height / to.height})`;
      const finish = `translate(${to.x - from.x}px, ${to.y - from.y}px) rotate(${to.angle}deg)`;
      if (landing) landing.style.opacity = "0";
      play(ghost, [{ transform: start, opacity: signal ? 0.4 : 1 },
        { transform: finish, opacity: signal ? 0 : 1 }], {
        delay, duration, fill: "none",
      }, () => {
        ghost.remove();
        if (landing) landing.style.opacity = "";
      }, complete);
    }

    function animateChanges(old, next, events, deal, previousGame, swap) {
      for (const [id, { point: to, position }] of next.own) {
        const from = old?.own.get(id)?.point;
        if (!from || !to) continue;
        const dx = from.x - to.x;
        const dy = from.y - to.y;
        if (Math.abs(dx) < 1 && Math.abs(dy) < 1) continue;
        play(position, [{ transform: `translate(${dx}px, ${dy}px)` }, { transform: "none" }], { duration: 180 });
      }

      const boardCards = new Map();
      old?.pairs.forEach((pair, index) => pair.forEach((entry, layer) => {
        if (entry.card) boardCards.set(index * 2 + layer, entry);
      }));
      const stock = old?.stock ?? next.stock;
      const arrivals = new Map();
      const draws = new Map();
      const actor = (id, frame) => frame?.seats.get(id);
      const fromHand = (id, card) => old?.own.get(card?.id)?.point && id === you ? old.own.get(card.id).point
        : (actor(id, old)?.backs.at(-1)?.point ?? actor(id, old)?.stack ?? actor(id, old)?.face
          ?? actor(id, next)?.stack ?? actor(id, next)?.face);
      const moves = deal ? events : events.filter(({ kind }) => kind !== "deal");
      const firstDraw = moves.find(({ kind, count }) => (kind === "deal" || kind === "draw") && Number.isInteger(count));
      let shownCount = firstDraw ? firstDraw.count + 1 : game.deck_count;
      if (firstDraw) layout.setStockCount(shownCount);
      for (const { kind, player } of moves) {
        if (kind === "draw") draws.set(player, (draws.get(player) ?? 0) + 1);
      }
      const dealCount = deal ? moves.filter(({ kind }) => kind === "deal").length : 0;
      const dealStep = Math.min(38, 510 / Math.max(1, dealCount - 1));
      let delay = 0;
      let boardSettledAt = 0;
      for (const { kind, card, player, index, count } of moves) {
        const own = player === you;
        const target = actor(player, next);
        switch (kind) {
          case "deal":
          case "draw": {
            const entry = own ? next.own.get(card?.id) : null;
            const key = `${player}/${kind}`;
            const arrived = arrivals.get(key) ?? 0;
            arrivals.set(key, arrived + 1);
            const backIndex = kind === "deal" ? arrived : Math.max(0, (target?.backs.length ?? 0) - draws.get(player)) + arrived;
            const back = target?.backs[backIndex];
            const to = own ? entry?.target ?? next.hand
              : back?.point ?? target?.backs.at(-1)?.point ?? target?.stack ?? target?.face;
            fly(kind, makeBack(), stock, to, delay, {
              landing: own ? entry?.face : back?.face,
              complete: () => {
                if (!Number.isInteger(count) || count >= shownCount) return;
                shownCount = count;
                layout.setStockCount(count);
              },
            });
            delay += kind === "deal" ? dealStep : 55;
            break;
          }
          case "attack":
          case "defend":
          case "transfer": {
            if (!card) break;
            const layer = Number(kind === "defend");
            const entry = next.pairs[index]?.[layer];
            const to = entry?.point ?? next.board;
            const landing = entry?.card?.id === card.id ? entry.face : undefined;
            fly(kind, makeCard(card), fromHand(player, card), to, delay, { landing });
            boardSettledAt = delay + cardFlight;
            boardCards.set(index * 2 + layer, { card, point: to, reachedAt: boardSettledAt });
            delay += 95;
            break;
          }
          case "discard":
          case "take": {
            delay = Math.max(delay, boardSettledAt);
            const leaving = [...boardCards].sort(([a], [b]) => a - b).slice(0, count ?? boardCards.size);
            for (const [offset, [key, entry]] of leaving.entries()) {
              const destination = own && kind === "take" ? next.own.get(entry.card.id) : null;
              const to = kind === "discard" ? next.discard : own ? destination?.target ?? next.hand
                : target?.backs.at(-1)?.point ?? target?.stack ?? target?.face;
              const launch = delay + offset * 11;
              const reached = entry.reachedAt ?? 0;
              if (launch > reached) {
                fly("board-hold", makeCard(entry.card), entry.point, entry.point, reached, { duration: launch - reached });
              }
              fly(kind, makeCard(entry.card), entry.point, to, launch, { landing: destination?.face });
              boardCards.delete(key);
            }
            delay += cardFlight + Math.max(0, leaving.length - 1) * 11;
            break;
          }
          case "pass": {
            delay = Math.max(delay, boardSettledAt);
            const covered = boardCards.size > 0 && [...boardCards.keys()].every(key => key % 2 || boardCards.has(key + 1));
            fly(kind, node("span", covered ? "Бито" : "Пас", "durak-flight-label"),
              actor(player, old)?.face ?? target?.face, next.board, delay, { duration: signalFlight });
            boardSettledAt = delay + signalFlight;
            delay += 100;
            break;
          }
        }
      }
      const action = game.last_action;
      if (action?.kind === "take" &&
          (previousGame?.last_action?.kind !== "take" || previousGame.last_action.player !== action.player) &&
          !moves.some(({ kind }) => kind === "take") && old) {
        fly("take", node("span", "Взять", "durak-flight-label"),
          actor(action.player, old)?.face, next.board, delay, { duration: signalFlight });
      }
      if (swap !== undefined && old) {
        const outgoing = old.own.get(swap);
        const incoming = [...next.own.values()].find(({ card }) => !old.own.has(card.id));
        if (outgoing && incoming) {
          fly("cheat", makeCard(outgoing.card), outgoing.point, stock, 0);
          fly("cheat", makeCard(incoming.card), stock, incoming.target, 115, { landing: incoming.face });
        }
      }
    }

    function reset() {
      cancelMotion();
      actionButtons.clear();
      actions.replaceChildren();
      game = layout = undefined;
      lastId = 0;
      selected = null;
      rules.open = false;
    }

    return { element, update, setBlocked, cancelMotion, reset };
  }

  function rulebook(mode, throwers = "all") {
    const rules = [
      node("strong", mode === "transfer" ? "Переводной дурак" : "Подкидной дурак"),
      node("p", "Отбивайте карты старшей картой той же масти или козырем. Козырь бьётся только старшим козырем."),
      node("p", "Подкидывайте карты того же достоинства, что уже есть на столе. Если отбиться не получается, заберите карты."),
    ];
    if (throwers === "neighbors") rules.push(node("p", "Подкидывают только атакующий и соседи защищающегося."));
    if (mode === "transfer") rules.push(node("p", "До первой защиты можно перевести атаку картой того же достоинства."));
    return rules;
  }

  return { node, button, avatarFace, showAvatar, amounts, rulebook, create };
})();

window.createDurakTable = DurakTable.create;

window.createDurakOnline = function ({ root, request, avatar }) {
  const { node, button, avatarFace, showAvatar, amounts, rulebook } = DurakTable;
  const modes = { throw: "Подкидной", transfer: "Переводной" };
  const seatOptions = [2, 3, 4, 5, 6].map(count => [String(count), `${count} ${count < 5 ? "игрока" : "игроков"}`]);
  const deckLabels = { 24: "24 карты", 36: "36 карт", 52: "52 карты" };
  const emojis = ["👍", "😂", "😮", "👏", "😎", "😤"];
  const minStake = 10;
  const maxStake = 1_000_000;

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
    labelled("Кто может подкидывать", throwers),
    labelled("Разрешить шулерство", cheating, "durak-create-toggle"),
    labelled("Играть бесплатно", free, "durak-create-toggle"), stakeFields, submit,
  );
  create.append(node("summary", "Создать своё лобби"), form);
  lobby.append(tools, list, create);

  const roomPanel = node("section", "", "casino-panel durak-online-room");
  const note = node("p", "", "casino-feedback");
  note.setAttribute("role", "status");
  const palette = node("div", "", "durak-reaction-palette");
  palette.setAttribute("role", "group");
  palette.setAttribute("aria-label", "Отправить реакцию");
  palette.hidden = true;
  for (const emoji of emojis) palette.append(button(emoji, () => { void react(emoji); }));
  root.replaceChildren(lobby, roomPanel, note);

  let visible = false;
  let generation = 0;
  let timer;
  let pending = false;
  let reading = false;
  let snapshot = null;
  let notice = "";
  let ready = false;
  let reactionTarget = null;
  let reactionPending = false;
  let nextReactionAt = 0;
  let avatarRoom = null;
  const avatarCache = new Map();
  const seats = new Map();

  function leaveRoom() {
    const room = snapshot?.room;
    if (!room || (room.status === "playing" && !window.confirm("Выход завершит текущую партию. Покинуть стол?"))) return;
    void act("online_leave", { room_id: room.id });
  }
  const leave = button("Покинуть стол", leaveRoom);
  const table = DurakTable.create({
    move: (action, card) => move(snapshot.room, action, card),
    avatar: (player, active) => reactionAvatar(snapshot.room, player, active),
    controls: [leave],
  });
  let tableRoom = null;

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

  function reactionAvatar(room, player, active = false) {
    const trigger = button("", () => {
      reactionTarget = reactionTarget === player.id ? null : player.id;
      updatePalette();
    }, "durak-avatar");
    trigger.setAttribute("aria-label", `Реакции · ${player.name}`);
    const reaction = node("span", "", "durak-player-reaction");
    reaction.setAttribute("aria-hidden", "true");
    reaction.hidden = true;
    const face = roomAvatarFace(room, player);
    face.dataset.active = String(active);
    trigger.append(face, reaction);
    seats.set(player.id, { trigger, reaction });
    return trigger;
  }

  function renderWaitingRoom(room, you) {
    const header = node("div", "", "casino-table-status");
    header.append(node("h2", "Стол"), node("span", `${room.players.length}/${room.capacity}`, "casino-timer"));
    const players = node("ul", "", "durak-player-list");
    seats.clear();
    for (const player of room.players) {
      const seat = node("li");
      seat.append(reactionAvatar(room, player), node("span", `${player.name}${player.id === you ? " (вы)" : ""}`));
      players.append(seat);
    }
    const controls = node("div", "", "casino-toolbar");
    if (room.host === you) {
      const start = button("Начать игру", () => act("online_start", { room_id: room.id }), "casino-button");
      start.disabled = pending || !ready || room.players.length < 2;
      controls.append(start);
    }
    const exit = button("Покинуть стол", leaveRoom);
    exit.disabled = pending || !ready;
    controls.append(exit);
    const hint = room.players.length < 2 ? "Для начала игры нужны как минимум два участника."
      : room.host === you ? "Можно начать игру или дождаться других участников." : "Ждём, пока ведущий начнёт игру.";
    roomPanel.replaceChildren(header, settingsBadges(room), players, palette, node("p", hint, "durak-empty"), controls);
  }

  function render({ updateTable = false, animate = false, deal = false, swap } = {}) {
    const room = snapshot?.room;
    lobby.hidden = Boolean(room);
    roomPanel.hidden = !room;
    roomPanel.dataset.table = String(Boolean(room?.game));
    submit.disabled = pending || !ready;
    note.textContent = notice;
    const blocked = pending || !ready;
    leave.disabled = blocked;
    leave.textContent = room?.status === "finished" ? "Назад к столам" : "Покинуть стол";
    if (room?.game) {
      if (tableRoom !== room.id) {
        table.reset();
        tableRoom = room.id;
        roomPanel.replaceChildren(table.element);
        updateTable = true;
      }
      if (updateTable) {
        const focusedPlayer = [...seats].find(([, { trigger }]) => trigger === document.activeElement)?.[0];
        retainAvatars(room);
        seats.clear();
        if (room.status === "finished") reactionTarget = null;
        table.setBlocked(blocked);
        table.update({
          game: room.game, players: room.players, you: snapshot.you, playing: room.status === "playing", palette,
          rules: { key: `${room.mode}/${room.throwers}`, content: () => rulebook(room.mode, room.throwers) },
        }, { animate, deal, swap });
        seats.get(focusedPlayer)?.trigger.focus({ preventScroll: true });
      } else {
        table.setBlocked(blocked);
      }
    } else {
      if (tableRoom !== null) table.reset();
      tableRoom = null;
      retainAvatars(room);
      seats.clear();
      if (room) renderWaitingRoom(room, snapshot.you);
      else {
        roomPanel.replaceChildren();
        renderLobby(snapshot?.rooms ?? []);
      }
    }
    updateReactions();
    updatePalette();
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

  function roomAvatarFace(room, player) {
    let entry = avatarCache.get(player.id);
    if (!entry) {
      entry = { face: avatarFace(player), requested: false };
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
      entry.url = showAvatar(entry.face, blob);
    } catch {
      // The initials remain visible if the avatar is unavailable.
    }
  }

  function updateReactions() {
    for (const player of snapshot?.room?.players ?? []) {
      const reaction = seats.get(player.id)?.reaction;
      if (!reaction) continue;
      const emoji = snapshot.room.status !== "finished" && emojis.includes(player.reaction) ? player.reaction : "";
      if (reaction.textContent !== emoji) reaction.textContent = emoji;
      reaction.hidden = !emoji;
    }
  }

  function updatePalette() {
    if (!seats.has(reactionTarget)) reactionTarget = null;
    palette.hidden = reactionTarget === null || !ready;
    for (const [id, { trigger }] of seats) {
      trigger.disabled = !ready || snapshot?.room?.status === "finished";
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

  function roomContent(key, value) {
    if (key !== "reaction") return value;
  }

  function accept(next, swap) {
    if (!Array.isArray(next?.rooms) || typeof next.you !== "string") throw new Error("Не удалось загрузить столы.");
    const previous = snapshot?.room;
    const animate = ready && previous?.id === next.room?.id;
    const deal = animate && previous?.status === "waiting" && next.room?.status === "playing";
    const changed = !ready || snapshot?.you !== next.you || (next.room || snapshot?.room
      ? JSON.stringify(snapshot?.room, roomContent) !== JSON.stringify(next.room, roomContent)
      : JSON.stringify(snapshot?.rooms) !== JSON.stringify(next.rooms));
    snapshot = next;
    ready = true;
    if (!visible) return;
    if (changed) render({ updateTable: true, animate, deal, swap });
    else updateReactions();
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
      const result = await request(endpoint, data);
      pending = false;
      accept(result, data.action === "cheat" ? data.card : undefined);
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
    if (!room?.game) return;
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
      table.cancelMotion();
    } else {
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
      ready = false;
      table.cancelMotion();
      retainAvatars(null);
      updatePalette();
    },
  };
};
