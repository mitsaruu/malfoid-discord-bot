const assert = require("node:assert/strict");
const { existsSync, readFileSync } = require("node:fs");
const { join } = require("node:path");
const test = require("node:test");
const { runInNewContext } = require("node:vm");

const card = { id: 0, rank: "6", suit: "♠" };
const other = { id: 20, rank: "8", suit: "♥" };
const trump = { id: 21, rank: "9", suit: "♥" };

function game(changes) {
  return {
    revision: "001/r1", status: "playing", phase: "attack", hand: [card, other],
    trump, deck_count: 24, opponent_count: 6, table: [], legal_cards: [0],
    actions: ["attack"], message: "", events: [], discard_count: 0, bank: 0, ...changes,
  };
}

function fixture(online, profile) {
  const calls = [];
  const flights = [];
  const revoked = [];
  class Element extends EventTarget {
    children = [];
    dataset = {};
    attributes = {};
    style = { setProperty(name, value) { this[name] = value; } };
    append(...children) { this.children.push(...children); }
    replaceChildren(...children) { this.children = children; }
    setAttribute(name, value) { this.attributes[name] = value; }
    getAttribute(name) { return this.attributes[name]; }
    remove() {}
    getBoundingClientRect() { return { left: 0, top: 0, right: 40, width: 40, height: 55 }; }
    animate() {
      if (this.dataset.motion) flights.push(this.dataset.motion);
      return { finished: Promise.resolve(), cancel() {} };
    }
    focus() { document.activeElement = this; }
    click() {
      if (!this.disabled) {
        this.focus();
        this.dispatchEvent(new Event("click"));
      }
    }
  }
  const document = { createElement: (tag) => Object.assign(new Element(), { tag }) };
  const root = document.createElement("section");
  const window = {};
  const URL = { createObjectURL: () => "blob:solo-avatar", revokeObjectURL: value => revoked.push(value) };
  const file = join(__dirname, "durak.js");
  assert.ok(existsSync(file), "The Durak view module is available");
  const shared = readFileSync(join(__dirname, "durak_online.js"), "utf8");
  runInNewContext(shared + "\n" + readFileSync(file, "utf8"), { document, window, URL });
  window.createDurakOnline = online;
  const view = window.CasinoGames.durak({
    root, profile,
    request: (action, fields) => new Promise((resolve, reject) => {
      calls.push({ action, fields: JSON.parse(JSON.stringify(fields)), resolve, reject });
    }),
  });
  function descendants(node) {
    return node.children.flatMap((child) => [child, ...descendants(child)]);
  }
  const buttons = () => descendants(root).filter((node) => node.tag === "button");
  const byText = (text) => buttons().find((node) => node.textContent === text);
  const byCard = (id) => buttons().find((node) => node.dataset.cardId === String(id));
  const flush = () => new Promise(setImmediate);
  return {
    view, calls, document, byText, byCard, flush, flights, revoked,
    find: predicate => descendants(root).find(predicate),
    labelled: label => descendants(root).find(node => node.attributes["aria-label"] === label),
    sent: () => calls.map(({ action, fields }) => [action, fields]),
    async respond(current, call = calls.at(-1)) {
      call.resolve({ game: current });
      await flush();
    },
    async fail() {
      calls.at(-1).reject(new Error("network"));
      await flush();
    },
  };
}

test("the online lobby opens by default and pauses when switching to the bot", () => {
  const events = [];
  const ui = fixture(() => ({ show() { events.push("open"); }, hide() { events.push("close"); } }));
  ui.view.show();
  assert.equal(ui.byText("Открытые столы").attributes["aria-pressed"], "true");
  ui.byText("С ботом").click();
  assert.deepEqual(events, ["open", "close"]);
  assert.deepEqual(ui.sent(), [["state", {}]]);
  ui.byText("Открытые столы").click();
  ui.view.hide();
  assert.deepEqual(events, ["open", "close", "open", "close"]);
});

test("starts and sends the exact revision and legal numeric card once", async () => {
  const ui = fixture();
  ui.view.show();
  await ui.respond(null);
  ui.byText("Начать игру").click();
  await ui.respond(game());
  ui.byCard(20).click();
  ui.byCard(0).click();
  ui.byCard(0).click();
  assert.deepEqual(ui.sent(), [
    ["state", {}], ["start", {}], ["move", { revision: "001/r1", action: "attack", card: 0 }],
  ]);
});

test("can take after covering part of the attack", async () => {
  const ui = fixture();
  ui.view.show();
  await ui.respond(game({
    phase: "defend", actions: ["defend", "take"], legal_cards: [20],
    table: [{ attack: card, defense: trump }, { attack: { ...card, id: 1, suit: "♣" }, defense: null }],
  }));
  ui.byText("Взять карты").click();
  assert.deepEqual(ui.sent().at(-1), ["move", { revision: "001/r1", action: "take" }]);
});

test("can throw in using attack or end the attack using pass", async () => {
  const ui = fixture();
  ui.view.show();
  await ui.respond(game({ phase: "throw", actions: ["attack", "pass"] }));
  ui.byCard(0).click();
  assert.equal(ui.sent().at(-1)[1].action, "attack");
  await ui.respond(game({ revision: "r2", phase: "throw", actions: ["pass"], legal_cards: [] }));
  ui.byText("Завершить атаку").click();
  assert.deepEqual(ui.sent().at(-1), ["move", { revision: "r2", action: "pass" }]);
});

test("a rejected move blocks stale actions until state recovery succeeds", async () => {
  const ui = fixture();
  ui.view.show();
  await ui.respond(game());
  ui.byCard(0).click();
  await ui.fail();
  assert.deepEqual(ui.sent().at(-1), ["state", {}]);
  await ui.fail();
  ui.byCard(0).click();
  assert.equal(ui.calls.length, 3);
  ui.byText("Обновить состояние").click();
  await ui.respond(game({ revision: "fresh", phase: "defend", actions: ["defend", "take"] }));
  ui.byCard(0).click();
  assert.deepEqual(ui.sent().at(-1), ["move", { revision: "fresh", action: "defend", card: 0 }]);
});

test("returning during a move waits for it before refreshing", async () => {
  const ui = fixture();
  ui.view.show();
  await ui.respond(game());
  ui.byCard(0).click();
  ui.view.hide();
  ui.view.show();
  assert.equal(ui.calls.length, 2);
  await ui.respond(game({ revision: "settled" }));
  assert.deepEqual(ui.sent().at(-1), ["state", {}]);
  await ui.respond(game({ revision: "reopened" }));
  ui.byCard(0).click();
  assert.equal(ui.sent().at(-1)[1].revision, "reopened");
});

test("an earlier state read cannot overwrite the reopened view", async () => {
  const ui = fixture();
  ui.view.show();
  const initial = ui.calls[0];
  ui.view.hide();
  ui.view.show();
  await ui.respond(game({ revision: "current" }));
  await ui.respond(null, initial);
  ui.byCard(0).click();
  assert.equal(ui.sent().at(-1)[1].revision, "current");
});

test("restarting an active game requires confirmation", async () => {
  const ui = fixture();
  ui.view.show();
  await ui.respond(game());
  ui.byText("Начать заново").click();
  assert.equal(ui.calls.length, 1);
  ui.byText("Продолжить игру").click();
  ui.byText("Начать заново").click();
  ui.byText("Да, начать заново").click();
  assert.deepEqual(ui.sent().at(-1), ["start", {}]);
});

test("the solo table deals and moves cards using the shared event stream", async () => {
  const ui = fixture();
  ui.view.show();
  await ui.respond(null);
  ui.byText("Начать игру").click();
  const deal = [
    { id: 1, kind: "deal", player: "you", card, count: 25 },
    { id: 2, kind: "deal", player: "bot", count: 24 },
  ];
  await ui.respond(game({ events: deal, turn: "you" }));
  assert.ok(ui.labelled("Колода: 24"));
  assert.deepEqual(ui.flights, ["deal", "deal"]);
  ui.byCard(0).click();
  await ui.respond(game({
    revision: "001/r2", phase: "throw", actions: ["pass"], legal_cards: [], hand: [other], turn: "you",
    table: [{ attack: card, defense: trump }],
    events: [...deal, { id: 3, kind: "attack", player: "you", index: 0, card },
      { id: 4, kind: "defend", player: "bot", index: 0, card: trump }],
  }));
  assert.deepEqual(ui.flights, ["deal", "deal", "attack", "defend"]);
});

test("the solo seat uses the authenticated name and releases the avatar on hide", async () => {
  const ui = fixture(undefined, { name: "Влад", avatar: async () => new Blob(["avatar"]) });
  ui.view.show();
  await ui.respond(game());
  const portrait = ui.find(node => node.className === "game-player-avatar" && node.dataset.active === "true");
  assert.ok(portrait);
  assert.equal(portrait.textContent, "В");
  assert.equal(portrait.children[0].src, "blob:solo-avatar");
  assert.ok(ui.find(node => node.className === "durak-seat-name" && node.textContent === "Влад"));
  ui.view.hide();
  assert.deepEqual(ui.revoked, ["blob:solo-avatar"]);
});
