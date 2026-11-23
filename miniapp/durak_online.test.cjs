const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const test = require("node:test");
const { runInNewContext } = require("node:vm");

const source = readFileSync(join(__dirname, "durak_online.js"), "utf8");

function fixture() {
  const calls = [];
  let poll;
  class Element extends EventTarget {
    children = [];
    dataset = {};
    attributes = {};
    value = "";
    constructor(tag) { super(); this.tag = tag; }
    append(...children) {
      this.children.push(...children);
      if (this.tag === "select" && !this.value) this.value = this.children[0].value;
    }
    replaceChildren(...children) { this.children = children; }
    setAttribute(name, value) { this.attributes[name] = value; }
    querySelector() { return null; }
    click() { if (!this.disabled) this.dispatchEvent(new Event("click")); }
  }
  const document = Object.assign(new EventTarget(), { createElement: tag => new Element(tag) });
  const root = new Element("main");
  const window = { confirm: () => true };
  runInNewContext(source, { window, document, setTimeout: fn => { poll = fn; }, clearTimeout() {} });
  const view = window.createDurakOnline({ root, request: (action, data) => new Promise((resolve, reject) => {
    calls.push({ action, data: JSON.parse(JSON.stringify(data)), resolve, reject });
  }) });
  const descendants = node => [node, ...node.children.flatMap(descendants)];
  const find = predicate => descendants(root).find(predicate);
  const control = label => find(element => element.tag === "button" && element.textContent === label);
  const labelled = label => find(element => element.attributes["aria-label"] === label);
  const flush = () => new Promise(setImmediate);
  return { view, calls, control, labelled, poll: () => poll(),
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
  assert.deepEqual(ui.calls.at(-1).data, { mode: "transfer", capacity: 6 });
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
