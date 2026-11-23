const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const test = require("node:test");
const { runInNewContext } = require("node:vm");

const source = readFileSync(join(__dirname, "durak_online.js"), "utf8");

function fixture(avatar) {
  const calls = [];
  const revoked = [];
  let poll;
  let tick;
  let now = 0;
  let avatarCount = 0;
  class Element extends EventTarget {
    children = [];
    dataset = {};
    attributes = {};
    value = "";
    checked = false;
    constructor(tag) { super(); this.tag = tag; }
    append(...children) {
      this.children.push(...children);
      if (this.tag === "select" && !this.value) this.value = this.children[0].value;
    }
    replaceChildren(...children) { this.children = children; }
    setAttribute(name, value) { this.attributes[name] = value; }
    querySelector() { return null; }
    focus() {}
    click() { if (!this.disabled) this.dispatchEvent(new Event("click")); }
  }
  const document = Object.assign(new EventTarget(), { createElement: tag => new Element(tag) });
  const root = new Element("main");
  const window = { confirm: () => true };
  const url = { createObjectURL: () => `blob:avatar-${++avatarCount}`, revokeObjectURL: value => revoked.push(value) };
  runInNewContext(source, {
    window, document, URL: url, performance: { now: () => now },
    setTimeout: fn => { poll = fn; }, clearTimeout() {},
    setInterval: fn => { tick = fn; return 1; }, clearInterval: () => { tick = undefined; },
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
  return { view, calls, control, labelled, find, revoked, flush, poll: () => poll(),
    advance(ms) { now += ms; tick?.(); },
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
    mode: "transfer", capacity: 6, deck: 36, cheating: false, throwers: "all", speed: "slow", stake: 0,
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
  ui.labelled("Темп игры").value = "fast";
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
    mode: "throw", capacity: 4, deck: 24, cheating: true, throwers: "neighbors", speed: "fast", stake: 1000000,
  });
});

test("lobby and room show game settings and retain defaults for older snapshots", async () => {
  const ui = fixture();
  ui.view.show();
  const settings = { deck: 24, cheating: true, throwers: "neighbors", speed: "fast", stake: 150 };
  const enabled = { ...transferRoom, ...settings };
  await ui.respond(snapshot(null, [enabled, throwRoom]));
  const customLabels = ["24 карты", "С шулерством", "Подкидывают соседи", "Быстрая · 15 с", "Ставка 150 монет"];
  for (const label of [...customLabels, "36 карт", "Честная игра", "Подкидывают все", "Медленная · 45 с", "Бесплатно"]) {
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
    turn: "20", actions: ["attack"], legal_cards: { attack: [12] }, message: "Ваш ход.", remaining_ms: 15000 },
};

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

test("countdown and reaction polls keep game controls and selected card in place", async () => {
  const ui = fixture();
  ui.view.show();
  const game = { ...playing.game, actions: ["attack", "cheat"], legal_cards: { attack: [12], cheat: [12] } };
  const room = { ...playing, cheating: true, game };
  await ui.respond(snapshot(room));
  const clock = ui.find(element => element.attributes.role === "timer");
  assert.equal(clock?.textContent, "15 с");
  ui.control("8♥").click();
  const handCard = ui.control("8♥");
  const swap = ui.control("Обменять карту");
  assert.equal(handCard.attributes["aria-pressed"], "true");
  ui.advance(2200);
  assert.equal(clock.textContent, "13 с");
  ui.poll();
  await ui.respond(snapshot({ ...room, players: [{ ...host, reaction: "👏" }, guest],
    game: { ...game, remaining_ms: 11500 } }));
  assert.equal(clock.textContent, "12 с");
  assert.equal(ui.control("8♥"), handCard);
  assert.equal(ui.control("Обменять карту"), swap);
  assert.ok(ui.find(element => element.textContent === "👏"));
  ui.advance(11500);
  assert.equal(clock.textContent, "0 с");
  ui.poll();
  await ui.respond(snapshot({ ...room, status: "finished",
    game: { ...game, revision: "r2", remaining_ms: 0, message: "Время вышло." } }));
  assert.ok(clock.hidden);
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
  await ui.respond(snapshot({ ...playing, players: [host, { ...guest, reaction: "👍" }],
    game: { ...playing.game, remaining_ms: 14900 } }));
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
