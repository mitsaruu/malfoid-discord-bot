const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const test = require("node:test");
const { runInNewContext } = require("node:vm");

const source = readFileSync(join(__dirname, "durak_online.js"), "utf8");

function fixture(avatar, { reduced = false } = {}) {
  const calls = [];
  const revoked = [];
  const animations = [];
  let poll;
  let now = 0;
  let avatarCount = 0;
  class Element extends EventTarget {
    children = [];
    dataset = {};
    attributes = {};
    style = { setProperty(name, value) { this[name] = value; } };
    value = "";
    checked = false;
    constructor(tag) { super(); this.tag = tag; }
    append(...children) {
      for (const child of children) {
        child.remove();
        child.parent = this;
      }
      this.children.push(...children);
      if (this.tag === "select" && !this.value) this.value = this.children[0].value;
    }
    replaceChildren(...children) {
      for (const child of this.children) child.parent = null;
      this.children = [];
      this.append(...children);
    }
    remove() {
      if (this.parent) this.parent.children = this.parent.children.filter(child => child !== this);
      this.parent = null;
    }
    setAttribute(name, value) { this.attributes[name] = value; }
    getAttribute(name) { return this.attributes[name]; }
    get parentElement() { return this.parent; }
    querySelector() { return null; }
    focus() { document.activeElement = this; }
    click() {
      if (this.disabled) return;
      this.focus();
      this.dispatchEvent(new Event("click"));
    }
    getBoundingClientRect() {
      const points = { deck: [20, 70], discard: [270, 210], "opponent-hand": [150, 40],
        "opponent-card": [150, 40], "own-hand": [95, 290],
        "board-center": [150, 150], "table-attack": [110, 125], "table-defense": [122, 139], avatar: [150, 20] };
      const [left, top] = this.dataset.cardId && this.tag === "button"
        ? [105 + (this.parent?.parent?.children.indexOf(this.parent) ?? 0) * 55, 290]
        : points[this.dataset.anchor] ?? [0, 0];
      const [width, height] = this.dataset.anchor === "own-hand" ? [280, 80] : [40, 55];
      return { left, top, width, height, right: left + width, bottom: top + height };
    }
    animate(keyframes, options) {
      let finish;
      let cancel;
      const finished = new Promise((resolve, reject) => {
        finish = resolve;
        cancel = () => reject(new Error("cancelled"));
      });
      animations.push({ element: this, keyframes, options, finish });
      return { finished, cancel };
    }
  }
  const document = Object.assign(new EventTarget(), { createElement: tag => new Element(tag) });
  const root = new Element("main");
  const window = { confirm: () => true, matchMedia: () => ({ matches: reduced }) };
  const url = { createObjectURL: () => `blob:avatar-${++avatarCount}`, revokeObjectURL: value => revoked.push(value) };
  runInNewContext(source, {
    window, document, URL: url, performance: { now: () => now },
    setTimeout: fn => { poll = fn; }, clearTimeout() {},
  });
  const view = window.createDurakOnline({ root, avatar, request: (action, data) => new Promise((resolve, reject) => {
    calls.push({ action, data: JSON.parse(JSON.stringify(data)), resolve, reject });
  }) });
  const descendants = (node, includeHidden) => {
    if (node.hidden && !includeHidden) return [];
    return [node, ...node.children.flatMap(child => descendants(child, includeHidden))];
  };
  const find = (predicate, { includeHidden = false } = {}) => descendants(root, includeHidden).find(predicate);
  const control = label => find(element => element.tag === "button" && element.textContent === label);
  const labelled = label => find(element => element.attributes["aria-label"] === label);
  const flush = () => new Promise(setImmediate);
  return { view, calls, control, labelled, find, revoked, flush, animations, poll: () => poll(),
    advance(ms) { now += ms; },
    sendCreate() { find(element => element.tag === "form").dispatchEvent(new Event("submit")); },
    async respond(snapshot, call = calls.at(-1)) { call.resolve(snapshot); await flush(); },
  };
}

const host = { id: "10", name: "Ведущий", cards: 6 };
const guest = { id: "20", name: "Гость", cards: 6 };
const transferRoom = { id: "room", mode: "transfer", host: "10", capacity: 4, players: [host] };
const throwRoom = { ...transferRoom, id: "throw-room", mode: "throw" };
const snapshot = (room = null, rooms = []) => ({ you: "20", rooms, room });

test("lobby filters by rules and joins the selected room", async () => {
  const ui = fixture();
  ui.view.show();
  await ui.respond(snapshot(null, [transferRoom, throwRoom]));
  const filter = ui.labelled("Фильтр режима");
  filter.value = "transfer";
  filter.dispatchEvent(new Event("change"));
  ui.control("Войти").click();
  assert.equal(ui.calls.at(-1).action, "online_join");
  assert.deepEqual(ui.calls.at(-1).data, { room_id: "room" });
  await ui.respond(snapshot({ ...transferRoom, status: "waiting", players: [host, guest], game: null }));
  assert.equal(ui.control("Начать игру"), undefined);
  ui.control("Покинуть стол").click();
  assert.equal(ui.calls.at(-1).action, "online_leave");
});

test("host creates a six-seat table and starts only when a second player joins", async () => {
  const ui = fixture();
  ui.view.show();
  await ui.respond(snapshot());
  ui.labelled("Режим стола").value = "transfer";
  ui.labelled("Число игроков").value = "6";
  ui.sendCreate();
  assert.equal(ui.calls.at(-1).action, "online_create");
  assert.deepEqual(ui.calls.at(-1).data, {
    mode: "transfer", capacity: 6, deck: 36, cheating: false, throwers: "all", stake: 0,
  });
  const room = { ...transferRoom, host: "20", capacity: 6, status: "waiting", players: [guest], game: null };
  await ui.respond(snapshot(room));
  assert.equal(ui.control("Начать игру").disabled, true);
  ui.poll();
  await ui.respond(snapshot({ ...room, players: [guest, host] }));
  ui.control("Начать игру").click();
  assert.equal(ui.calls.at(-1).action, "online_start");
});

test("a card legal for defense and transfer lets the player choose explicitly", async () => {
  const ui = fixture();
  ui.view.show();
  ui.view.show();
  const card = { id: 12, rank: "8", suit: "♥" };
  const game = { revision: "r1", hand: [card], trump: card, deck_count: 21, table: [], turn: "20",
    actions: ["defend", "transfer", "take"], legal_cards: { defend: [12], transfer: [12] }, message: "Ваш ход." };
  await ui.respond(snapshot({ ...transferRoom, status: "playing", players: [host, guest], game }));
  ui.control("8♥").click();
  ui.control("Перевести").click();
  assert.equal(ui.calls.at(-1).action, "online_move");
  assert.deepEqual(ui.calls.at(-1).data, { room_id: "room", revision: "r1", action: "transfer", card: 12 });
});

test("the 24-card deck limits seats and room creation sends selected settings", async () => {
  const ui = fixture();
  ui.view.show();
  await ui.respond(snapshot());
  const deck = ui.labelled("Колода");
  const seats = ui.labelled("Число игроков");
  assert.ok(deck, "deck picker is visible");
  seats.value = "6";
  deck.value = "24";
  deck.dispatchEvent(new Event("change"));
  assert.deepEqual(seats.children.map(option => option.value), ["2", "3", "4"]);
  assert.equal(seats.value, "4");
  deck.value = "52";
  deck.dispatchEvent(new Event("change"));
  assert.equal(seats.children.at(-1).value, "6");
  deck.value = "24";
  deck.dispatchEvent(new Event("change"));
  assert.equal(ui.labelled("Ставка в монетах"), undefined);
  ui.labelled("Разрешить шулерство").checked = true;
  ui.labelled("Кто может подкидывать").value = "neighbors";
  assert.equal(ui.labelled("Темп игры"), undefined);
  const free = ui.labelled("Играть бесплатно");
  free.checked = false;
  free.dispatchEvent(new Event("change"));
  const stake = ui.labelled("Ставка в монетах");
  const slider = ui.labelled("Размер ставки");
  assert.equal(stake.value, "10");
  slider.value = "150";
  slider.dispatchEvent(new Event("input"));
  assert.equal(stake.value, "150");
  for (const invalid of ["", "0", "9", "10.5", "1000001"]) {
    stake.value = invalid;
    ui.sendCreate();
    assert.equal(ui.calls.at(-1).action, "online_state");
  }
  stake.value = "1000000";
  stake.dispatchEvent(new Event("input"));
  assert.equal(slider.value, "1000000");
  ui.sendCreate();
  assert.equal(ui.calls.at(-1).action, "online_create");
  assert.deepEqual(ui.calls.at(-1).data, {
    mode: "throw", capacity: 4, deck: 24, cheating: true, throwers: "neighbors", stake: 1000000,
  });
});

test("lobby and room show game settings and retain defaults for older snapshots", async () => {
  const ui = fixture();
  ui.view.show();
  const settings = { deck: 24, cheating: true, throwers: "neighbors", speed: "fast", stake: 150 };
  const enabled = { ...transferRoom, ...settings };
  await ui.respond(snapshot(null, [enabled, throwRoom]));
  const customLabels = ["24 карты", "С шулерством", "Подкидывают соседи", "Ставка 150 монет"];
  for (const label of [...customLabels, "36 карт", "Честная игра", "Подкидывают все", "Бесплатно"]) {
    assert.ok(ui.find(element => element.textContent === label), label);
  }
  ui.control("Войти").click();
  await ui.respond(snapshot({ ...enabled, status: "waiting", players: [host, guest], game: null }));
  for (const label of customLabels) assert.ok(ui.find(element => element.textContent === label), label);
});

const privateCard = { id: 12, rank: "8", suit: "♥" };
const trump = { id: 15, rank: "9", suit: "♠" };
const playing = {
  ...transferRoom, status: "playing", speed: "fast", players: [host, guest],
  game: { revision: "r1", hand: [privateCard], trump, deck_count: 21, table: [],
    turn: "20", actions: ["attack"], legal_cards: { attack: [12] }, message: "Ваш ход.", events: [], discard_count: 0, bank: 0 },
};

const events = {
  deal: [
    { id: 1, kind: "deal", player: "10", count: 35 },
    { id: 2, kind: "deal", player: "20", card: privateCard, count: 34 },
  ],
  attack: { id: 3, kind: "attack", player: "20", card: privateCard, index: 0 },
  defend: { id: 4, kind: "defend", player: "10", card: trump, index: 0 },
};

function flights(ui, kind) {
  return ui.animations.filter(({ element }) => element.dataset.motion === kind);
}

test("leaving a table with an available action and joining another one resets its controls", async () => {
  const ui = fixture();
  const table = { ...playing, game: { ...playing.game, actions: ["pass"] } };
  const other = { ...playing, id: "second-table" };
  ui.view.show();
  await ui.respond(snapshot(table));
  assert.ok(ui.control("Бито"));
  ui.control("Покинуть стол").click();
  await ui.respond(snapshot(null, [other]));
  ui.control("Войти").click();
  await ui.respond(snapshot(other));
  assert.equal(ui.control("Бито"), undefined);
  assert.equal(ui.control("8♥").disabled, false);
});

test("a fresh start deals backs into both hands and reveals the player's card on landing", async () => {
  const ui = fixture();
  const started = { ...playing, game: { ...playing.game, events: events.deal, deck_count: 34 } };
  ui.view.show();
  await ui.respond(snapshot({ ...playing, status: "waiting", game: null }));
  ui.poll();
  await ui.respond(snapshot(started));
  const dealt = flights(ui, "deal");
  assert.equal(dealt.length, 2);
  assert.equal(dealt[0].element.children[0].textContent, "");
  assert.equal(dealt[1].element.children[0].textContent, "");
  const face = ui.control("8♥");
  assert.equal(face.style.opacity, "0");
  assert.ok(ui.labelled("Колода: 36"));
  assert.ok(dealt[1].options.delay > dealt[0].options.delay);
  assert.equal(dealt[1].element.style.left, "40px");
  assert.match(dealt[1].keyframes.at(-1).transform, /translate\(85px, 220px\)/);
  assert.ok(ui.find(element => element.dataset.anchor === "opponent-hand"));
  dealt[0].finish();
  await ui.flush();
  assert.ok(ui.labelled("Колода: 35"));
  dealt[1].finish();
  await ui.flush();
  assert.ok(ui.labelled("Колода: 34"));
  assert.equal(face.style.opacity, "");

  ui.view.hide();
  ui.view.show();
  await ui.respond(snapshot(started));
  assert.equal(flights(ui, "deal").length, 2);
});

test("an attack flies from its own card, and remaining cards slide into their sorted positions", async () => {
  const ui = fixture();
  const room = { ...playing, game: { ...playing.game, hand: [privateCard, trump], events: events.deal } };
  ui.view.show();
  await ui.respond(snapshot(room));
  assert.equal(ui.animations.length, 0);
  ui.control("8♥").click();
  await ui.respond(snapshot({ ...room, game: { ...room.game, revision: "r2", hand: [trump],
    table: [{ attack: privateCard }], events: [...events.deal, events.attack], actions: [] } }));
  const [attack] = flights(ui, "attack");
  assert.equal(attack.element.style.left, "125px");
  assert.match(attack.keyframes.at(-1).transform, /translate\(5px, -165px\)/);
  assert.match(flights(ui, "hand")[0].keyframes[0].transform, /translate\(55px, 0px\)/);
  ui.poll();
  await ui.respond(snapshot({ ...room, game: { ...room.game, revision: "r2", hand: [trump],
    table: [{ attack: privateCard }], events: [...events.deal, events.attack], actions: [] } }));
  assert.equal(flights(ui, "attack").length, 1);
});

test("pending requests keep the acted card focusable and prevent duplicate keyboard activation", async () => {
  const ui = fixture();
  ui.view.show();
  await ui.respond(snapshot(playing));
  const control = ui.control("8♥");
  control.click();
  assert.equal(control.disabled, false);
  assert.equal(control.attributes["aria-disabled"], "true");
  control.click();
  assert.equal(ui.calls.filter(call => call.action === "online_move").length, 1);
});

test("a polled defense lands on its pair before both public cards fly to the discard pile", async () => {
  const ui = fixture();
  const room = { ...playing, game: { ...playing.game, hand: [], table: [{ attack: privateCard }],
    events: [...events.deal, events.attack], bank: 300 } };
  ui.view.show();
  await ui.respond(snapshot(room));
  assert.ok(ui.labelled("Банк: 300 монет"));
  ui.poll();
  await ui.respond(snapshot({ ...room, game: { ...room.game, revision: "r3", table: [], discard_count: 2,
    events: [...room.game.events, events.defend, { id: 5, kind: "discard", player: "20", count: 2 }] } }));
  const [defense] = flights(ui, "defend");
  assert.match(defense.keyframes.at(-1).transform, /translate\(-28px, 99px\)/);
  const discarded = flights(ui, "discard");
  const faces = discarded.map(({ element }) => element.children[0].children);
  assert.deepEqual(faces.map(([front]) => front.textContent), ["8♥", "9♠"]);
  assert.ok(faces.every(([, back]) => back.className.includes("durak-card--back")));
  const flips = ui.animations.filter(({ element }) => element.className === "durak-flight-flip");
  assert.equal(flips.length, 2);
  assert.ok(flips.every(({ keyframes }) => keyframes.at(-1).transform === "rotateY(180deg)"));
  assert.ok(discarded[0].options.delay >= defense.options.delay + defense.options.duration);
  assert.match(discarded[0].keyframes.at(-1).transform, /translate\(160px, 85px\)/);
  assert.ok(ui.labelled("Отбой: 2"));
});

test("a pass finishes before discard, and refill ticks the stock as each back lands", async () => {
  const ui = fixture();
  const room = { ...playing, game: { ...playing.game, hand: [], table: [{ attack: privateCard, defense: trump }],
    events: [...events.deal, events.attack, events.defend] } };
  ui.view.show();
  await ui.respond(snapshot(room));
  ui.poll();
  const refill = { id: 21, rank: "J", suit: "♠" };
  await ui.respond(snapshot({ ...room, game: { ...room.game, revision: "r3", table: [], hand: [refill],
    discard_count: 2, deck_count: 19, events: [
      ...room.game.events, { id: 5, kind: "pass", player: "20" },
      { id: 6, kind: "discard", player: "20", count: 2 }, { id: 7, kind: "draw", player: "10", count: 20 },
      { id: 8, kind: "draw", player: "20", card: refill, count: 19 },
    ] } }));
  const [pass] = flights(ui, "pass");
  const discarded = flights(ui, "discard");
  const drawn = flights(ui, "draw");
  assert.equal(pass.element.children[0].textContent, "Бито");
  assert.ok(discarded[0].options.delay >= pass.options.duration);
  assert.ok(drawn[0].options.delay >= discarded.at(-1).options.delay + discarded.at(-1).options.duration);
  assert.deepEqual(drawn.map(({ element }) => element.children[0].textContent), ["", ""]);
  assert.equal(ui.control("J♠").style.opacity, "0");
  assert.ok(ui.labelled("Колода: 21"));
  drawn[0].finish();
  await ui.flush();
  assert.ok(ui.labelled("Колода: 20"));
  drawn[1].finish();
  await ui.flush();
  assert.ok(ui.labelled("Колода: 19"));
  assert.equal(ui.control("J♠").style.opacity, "");
});

test("a transfer uses the new attack slot, and a take sends its cards to the receiving hand", async () => {
  const ui = fixture();
  const room = { ...playing, game: { ...playing.game, hand: [trump], table: [{ attack: privateCard }],
    events: [...events.deal, events.attack] } };
  ui.view.show();
  await ui.respond(snapshot(room));
  ui.poll();
  const transfer = { id: 4, kind: "transfer", player: "20", card: trump, index: 1 };
  await ui.respond(snapshot({ ...room, game: { ...room.game, revision: "r3", hand: [],
    table: [{ attack: privateCard }, { attack: trump }], events: [...room.game.events, transfer] } }));
  assert.equal(flights(ui, "transfer").length, 1);
  ui.poll();
  await ui.respond(snapshot({ ...room, game: { ...room.game, revision: "r4", hand: [privateCard, trump], table: [],
    events: [...room.game.events, transfer, { id: 5, kind: "take", player: "20", count: 2 }] } }));
  const taken = flights(ui, "take");
  assert.deepEqual(taken.map(({ element }) => element.children[0].textContent), ["8♥", "9♠"]);
  assert.match(taken[0].keyframes.at(-1).transform, /translate\(-5px, 165px\)/);
  assert.match(taken[1].keyframes.at(-1).transform, /translate\(50px, 165px\)/);
});

test("reduced motion presents the settled hand without flights", async () => {
  const ui = fixture(undefined, { reduced: true });
  ui.view.show();
  await ui.respond(snapshot({ ...playing, status: "waiting", game: null }));
  ui.poll();
  await ui.respond(snapshot({ ...playing, game: { ...playing.game, events: events.deal } }));
  assert.equal(ui.animations.length, 0);
  assert.ok(ui.control("8♥"));
});

test("the active seat and latest action are marked, and a full attack has no next slot", async () => {
  const ui = fixture();
  ui.view.show();
  const game = { ...playing.game, table: [{ attack: trump }], attack_limit: 1,
    last_action: { kind: "attack", player: "10" } };
  await ui.respond(snapshot({ ...playing, game }));
  const own = ui.labelled("Реакции · Гость");
  assert.equal(own.children[0].dataset.active, "true");
  const actor = ui.labelled("Реакции · Ведущий");
  assert.ok(actor.parent.children.some(node => node.className === "durak-last-action" && node.textContent === "Ход"));
  assert.equal(ui.find(node => node.dataset.next === "true"), undefined);
});

test("private swap requires explicit confirmation using the selected legal card", async () => {
  const ui = fixture();
  ui.view.show();
  const room = { ...playing, cheating: true, game: { ...playing.game, actions: ["cheat"], legal_cards: { cheat: [12] } } };
  await ui.respond(snapshot(room));
  assert.equal(ui.control("Обменять карту")?.disabled, true);
  ui.control("8♥").click();
  assert.equal(ui.calls.at(-1).action, "online_state");
  ui.control("Обменять карту").click();
  assert.equal(ui.calls.at(-1).action, "online_move");
  assert.deepEqual(ui.calls.at(-1).data, { room_id: "room", revision: "r1", action: "cheat", card: 12 });
  await ui.respond(snapshot({ ...room, game: { ...playing.game, revision: "r2", hand: [trump],
    actions: [], legal_cards: {} } }));
  assert.equal(ui.control("Обменять карту"), undefined);
  assert.equal(ui.control("8♥"), undefined);
});

test("reaction polls keep game controls and selected card in place without a countdown", async () => {
  const ui = fixture();
  ui.view.show();
  const game = { ...playing.game, actions: ["attack", "cheat"], legal_cards: { attack: [12], cheat: [12] } };
  const room = { ...playing, cheating: true, game };
  await ui.respond(snapshot(room));
  assert.equal(ui.find(element => element.attributes.role === "timer"), undefined);
  ui.control("8♥").click();
  const handCard = ui.control("8♥");
  const swap = ui.control("Обменять карту");
  assert.equal(handCard.attributes["aria-pressed"], "true");
  ui.poll();
  await ui.respond(snapshot({ ...room, players: [{ ...host, reaction: "👏" }, guest] }));
  assert.equal(ui.control("8♥"), handCard);
  assert.equal(ui.control("Обменять карту"), swap);
  assert.ok(ui.find(element => element.textContent === "👏"));
  ui.poll();
  await ui.respond(snapshot({ ...room, status: "finished",
    game: { ...game, revision: "r2", message: "Партия завершена." } }));
  assert.ok(ui.control("Назад к столам"));
  assert.equal(ui.control("Обменять карту"), undefined);
  assert.equal(ui.find(element => element.textContent === "👏"), undefined);
});

test("an avatar opens the fixed reaction palette and sends only the room and emoji", async () => {
  const ui = fixture();
  ui.view.show();
  await ui.respond(snapshot(playing));
  const avatar = ui.labelled("Реакции · Гость");
  assert.ok(avatar?.children.some(element => element.textContent === "Г"));
  assert.equal(ui.control("😂"), undefined);
  const handCard = ui.control("8♥");
  avatar.click();
  for (const emoji of ["👍", "😂", "😮", "👏", "😎", "😤"]) assert.ok(ui.control(emoji));
  ui.control("👍").click();
  assert.deepEqual({ action: ui.calls.at(-1).action, data: ui.calls.at(-1).data },
    { action: "online_react", data: { room_id: "room", emoji: "👍" } });
  assert.equal(ui.control("8♥"), handCard);
  assert.equal(handCard.disabled, false);
  await ui.respond(snapshot({ ...playing, players: [host, { ...guest, reaction: "👍" }] }));
  assert.equal(ui.calls.at(-1).action, "online_state");
  await ui.respond(snapshot({ ...playing, players: [host, { ...guest, reaction: "👍" }] }));
  assert.ok(ui.find(element => element.textContent === "👍"));
  assert.equal(ui.control("8♥"), handCard);
  avatar.click();
  ui.control("😂").click();
  assert.equal(ui.calls.filter(call => call.action === "online_react").length, 1);
  ui.advance(2000);
  ui.control("😂").click();
  assert.equal(ui.calls.filter(call => call.action === "online_react").length, 2);
});

test("Discord avatar URLs are reused across polls and revoked when seats or the view are left", async () => {
  const requests = [];
  const waiting = { ...transferRoom, status: "waiting", players: [host, guest], game: null };
  const ui = fixture(async (room, player) => {
    requests.push([room, player]);
    return new Blob([player]);
  });
  ui.view.show();
  await ui.respond(snapshot(waiting));
  const image = ui.find(element => element.tag === "img" && element.src === "blob:avatar-1", { includeHidden: true });
  assert.ok(image);
  assert.ok(image.hidden);
  image.dispatchEvent(new Event("load"));
  assert.deepEqual(requests, [["room", "10"], ["room", "20"]]);
  ui.poll();
  await ui.respond(snapshot(waiting));
  assert.equal(ui.find(element => element.src === "blob:avatar-1"), image);
  assert.equal(requests.length, 2);
  ui.poll();
  await ui.respond(snapshot({ ...waiting, players: [guest] }));
  assert.deepEqual(ui.revoked, ["blob:avatar-1"]);
  ui.view.hide();
  assert.deepEqual(ui.revoked, ["blob:avatar-1", "blob:avatar-2"]);
});
