const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const test = require("node:test");
const { runInNewContext } = require("node:vm");

function fixture(game, { reduced = false, audio = true, avatar = false } = {}) {
  const calls = [];
  const sounds = [];
  const cues = [];
  const animations = [];
  const frames = new Map();
  const timers = new Map();
  const intervals = new Map();
  let time = 0;
  let nextId = 0;

  class Element extends EventTarget {
    children = [];
    style = { setProperty() {} };
    dataset = {};
    classList = { add() {}, toggle() {} };
    #value = "";
    get value() { return this.#value; }
    set value(value) { this.#value = String(value); }
    append(...children) {
      for (const child of children) {
        const node = typeof child === "string" ? Object.assign(new Element(), { textContent: child }) : child;
        node.parent = this;
        this.children.push(node);
      }
    }
    prepend(child) {
      child.parent = this;
      this.children.unshift(child);
    }
    replaceChildren(...children) {
      this.children = [];
      this.append(...children);
    }
    remove() { this.parent.children = this.parent.children.filter((child) => child !== this); }
    setAttribute() {}
    getContext() { return null; }
    animate(keyframes, options) {
      const animation = {
        keyframes, options,
        currentTime: 0,
        effect: { getComputedTiming: () => ({ progress: animation.progress }) },
      };
      animation.finished = new Promise((resolve, reject) => {
        animation.finish = resolve;
        animation.cancel = () => reject(new Error("Animation canceled"));
      });
      animation.finished.catch(() => {});
      animations.push(animation);
      return animation;
    }
  }

  const root = new Element();
  const document = Object.assign(new EventTarget(), {
    hidden: false,
    createElement: (tag) => Object.assign(new Element(), { tag }),
  });
  const motion = Object.assign(new EventTarget(), { matches: reduced });
  const loadAvatar = avatar ? async () => null : undefined;
  const window = Object.assign(new EventTarget(), {
    matchMedia: () => motion,
    getComputedStyle: () => ({ rowGap: "2px" }),
    createGameAvatars: avatar ? (loader) => {
      assert.equal(loader, loadAvatar);
      return {
        image(id, name, active = false) {
          return Object.assign(new Element(), { className: "game-player-avatar", id, name, active });
        },
        clear() {},
      };
    } : undefined,
    GameAudio: audio ? {
      play(name, delay = 0) {
        sounds.push(name);
        cues.push({ name, time: time + delay });
      },
      startLoop(name) {
        sounds.push(name);
        return () => sounds.push(name + ":end");
      },
    } : undefined,
  });
  const file = { slots: "casino_slots.js", blackjack: "table_blackjack.js" }[game] ?? `${game}.js`;
  runInNewContext(readFileSync(join(__dirname, file), "utf8"), {
    document, window, matchMedia: window.matchMedia, performance: { now: () => time },
    crypto: { randomUUID: () => "wager" },
    requestAnimationFrame(fn) {
      frames.set(++nextId, fn);
      return nextId;
    },
    cancelAnimationFrame: (id) => frames.delete(id),
    setTimeout: (fn) => timers.set(++nextId, fn) && nextId,
    clearTimeout: (id) => timers.delete(id),
    setInterval: (fn) => intervals.set(++nextId, fn) && nextId,
    clearInterval: (id) => intervals.delete(id),
  });
  const view = window.CasinoGames[game]({
    root, avatar: loadAvatar,
    request: (action, fields) => new Promise((resolve, reject) => calls.push({ action, fields, resolve, reject })),
  });

  function descendants(node) {
    return [node, ...node.children.flatMap(descendants)];
  }
  const nodes = (className) => descendants(root).filter((node) => node.className === className);
  const flush = () => new Promise(setImmediate);
  return {
    animations, sounds, cues, calls, frames, timers, intervals, view, nodes, flush,
    node: (name) => nodes(name)[0],
    async respond(action, data) {
      assert.equal(calls.at(-1).action, action);
      calls.at(-1).resolve(data);
      await flush();
    },
    async poll(data) {
      const [id, fn] = timers.entries().next().value;
      timers.delete(id);
      fn();
      await this.respond("state", data);
    },
    frame(at) {
      time = at;
      for (const [id, fn] of [...frames]) {
        if (frames.delete(id)) fn(time);
      }
    },
    time(at) { time = at; },
    visibility(hidden) {
      document.hidden = hidden;
      document.dispatchEvent(new Event("visibilitychange"));
    },
    reduce() {
      motion.matches = true;
      motion.dispatchEvent(Object.assign(new Event("change"), { matches: true }));
    },
    page(event) { window.dispatchEvent(new Event(event)); },
  };
}

const pocket = 360 / 37;
const openRound = { id: "round", remaining_ms: 1000, your_bets: [], bet_count: 0, player_count: 0 };
const receipt = (number, net) => ({
  id: "round", number, refunded: number === null, your_bets: [],
  players: [{ id: "2", name: "Other", net: 35 },
    ...(net === undefined ? [] : [{ id: "1", name: "Viewer", net }])],
});
const rouletteState = (round = null, last_result = null) =>
  ({ user_id: "1", round_id: "round", round, last_result, balance: 500, min_bet: 10, max_bet: 500, max_bets: 10 });
const roundBet = (user_id, name, space, amount) => ({ user_id, name, space, amount });
const rotation = (wheel) => Number(wheel.style.transform?.match(/rotate\((.+)deg\)/)?.[1] ?? 0);
const near = (actual, expected) => assert.ok(Math.abs(actual - expected) < 1e-8, `${actual} ≈ ${expected}`);

function assertPocket(ball, wheel, index) {
  assert.equal(ball.hidden, false);
  const angle = (rotation(wheel) + index * pocket) * Math.PI / 180;
  near(parseFloat(ball.style.left), 50 + 34.2 * Math.sin(angle));
  near(parseFloat(ball.style.top), 50 - 34.2 * Math.cos(angle));
}

test("roulette keeps individual stakes visible and reveals their payouts only after landing", async () => {
  const f = fixture("roulette", { avatar: true });
  const bets = [roundBet("2", "Алиса", "red", 10), roundBet("2", "Алиса", "3", 15)];
  const settled = {
    ...receipt(3),
    bets: [...bets.map((bet, i) => ({ ...bet, payout: [20, 540][i] })),
      { ...roundBet("3", "Борис", "black", 30), payout: 0 }],
  };
  const textOf = (className) => f.nodes(className).map((node) => String(node.textContent));

  f.view.show();
  await f.respond("state", rouletteState({ ...openRound, bets }));
  assert.equal(f.node("roulette-bets-panel").children[0].textContent, "Ставки раунда");
  assert.equal(f.node("roulette-layout").children[0], f.node("roulette-bets-panel"));
  assert.deepEqual(f.nodes("game-player-avatar").map(({ id, name }) => [id, name]), [["2", "Алиса"], ["2", "Алиса"]]);
  assert.deepEqual(textOf("roulette-bet-position"), ["Красное", "3"]);
  assert.deepEqual(textOf("roulette-bet-amount"), ["10", "15"]);
  assert.deepEqual(textOf("roulette-bet-payout"), []);

  await f.poll(rouletteState(null, settled));
  assert.deepEqual(textOf("roulette-bet-position"), ["Красное", "3", "Чёрное"]);
  f.frame(2799);
  assert.deepEqual(textOf("roulette-bet-payout"), []);
  f.frame(2800);
  assert.equal(f.node("roulette-result-number").textContent, 3);
  assert.deepEqual(textOf("roulette-bet-payout"), ["Выплата 20", "Выплата 540", "Выплата 0"]);
  await f.poll(rouletteState(null, settled));
  assert.equal(f.nodes("roulette-bet-payout").length, 3);

  const next = { ...openRound, id: "next", bets: [roundBet("4", "Вера", "0", 25)] };
  await f.poll(rouletteState(next, settled));
  assert.deepEqual(textOf("roulette-bet-position"), ["0"]);
  assert.deepEqual(textOf("roulette-bet-amount"), ["25"]);
  assert.deepEqual(textOf("roulette-bet-payout"), []);
  assert.equal(f.node("roulette-result-number").textContent, "—");
});

test("roulette does not put a previous spin's payouts on an already open round", async () => {
  const f = fixture("roulette");
  f.view.show();
  await f.respond("state", rouletteState({ ...openRound, bets: [roundBet("1", "Алиса", "3", 10)] }));
  const settled = { ...receipt(3, 350), bets: [{ ...roundBet("1", "Алиса", "3", 10), payout: 360 }] };
  await f.poll(rouletteState(null, settled));
  const next = { ...openRound, id: "next", bets: [roundBet("2", "Борис", "black", 20)] };
  await f.poll(rouletteState(next, settled));
  f.frame(2800);
  assert.equal(f.node("roulette-bet-position").textContent, "Чёрное");
  assert.equal(f.nodes("roulette-bet-payout").length, 0);
  assert.equal(f.node("roulette-result-number").textContent, "—");
  assert.ok(f.sounds.includes("roulette.land"));
});

test("roulette spins at a steady speed before the first state reply and during betting", async () => {
  const f = fixture("roulette");
  const wheel = f.node("roulette-wheel");
  const ball = f.node("roulette-ball");
  f.view.show();
  f.frame(250);
  near(rotation(wheel), .25 * pocket);
  f.frame(500);
  near(rotation(wheel), .5 * pocket);
  assert.equal(ball.hidden, true);
  assert.deepEqual(f.sounds, []);
  await f.respond("state", rouletteState(null, receipt(32)));
  assert.equal(ball.hidden, true, "an old receipt is not a launch");
  f.frame(1000);
  near(rotation(wheel), pocket);
  f.frame(3200);
  near(rotation(wheel), 3.2 * pocket);
  assert.deepEqual(f.sounds, ["roulette.tick", "roulette.tick"], "late frames skip stale sounds");
  await f.poll(rouletteState(openRound, receipt(32)));
  f.frame(4000);
  near(rotation(wheel), 4 * pocket);
  assert.equal(ball.hidden, true);

  f.visibility(true);
  assert.equal(f.frames.size, 0);
  f.frame(10000);
  f.visibility(false);
  f.frame(11000);
  near(rotation(wheel), 5 * pocket);
  f.view.hide();
  assert.equal(f.frames.size + f.timers.size + f.intervals.size, 0);
});

test("roulette resumes ticking at the next rendered pocket after a partial turn", () => {
  const f = fixture("roulette");
  f.view.show();
  f.frame(250);
  f.visibility(true);
  f.time(10000);
  f.visibility(false);
  f.frame(10749);
  assert.deepEqual(f.sounds, []);
  f.frame(10750);
  near(rotation(f.node("roulette-wheel")), pocket);
  assert.deepEqual(f.sounds, ["roulette.tick"]);
});

test("roulette launches from the last painted angle and hides the landed ball at the first idle step", async () => {
  const f = fixture("roulette");
  const wheel = f.node("roulette-wheel");
  const ball = f.node("roulette-ball");
  f.view.show();
  await f.respond("state", rouletteState(openRound));
  f.frame(39880);
  const rendered = rotation(wheel);
  assert.ok(rendered > 39 * pocket && rendered < 40 * pocket);
  f.time(39990);
  await f.poll(rouletteState(null, receipt(3)));
  near(rotation(wheel), rendered);
  assert.equal(ball.hidden, false);
  assert.equal(f.sounds.at(-1), "roulette.launch");
  f.frame(39990);
  near(rotation(wheel), rendered);
  f.frame(42790);
  assertPocket(ball, wheel, 35);
  assert.equal(f.node("roulette-result-number").textContent, 3);
  assert.deepEqual(f.sounds.slice(-2), ["roulette.launch", "roulette.land"]);
  f.frame(43670);
  assertPocket(ball, wheel, 35);
  f.frame(43790);
  assert.equal(ball.hidden, true);
  assert.equal(f.sounds.at(-1), "roulette.tick");
  f.frame(53790);
  assert.equal(ball.hidden, true);
});

test("roulette result ticks follow rendered pocket crossings and thin out as the wheel slows", async () => {
  const f = fixture("roulette");
  const wheel = f.node("roulette-wheel");
  f.view.show();
  await f.respond("state", rouletteState(openRound));
  await f.poll(rouletteState(null, receipt(0)));
  const ticks = [];
  for (let time = 10; time < 2800; time += 10) {
    const before = Math.floor(rotation(wheel) / pocket + 1e-9);
    const count = f.cues.length;
    f.frame(time);
    const after = Math.floor(rotation(wheel) / pocket + 1e-9);
    const emitted = f.cues.slice(count);
    assert.ok(emitted.length <= 1, "no catch-up burst");
    for (const cue of emitted) {
      assert.equal(cue.name, "roulette.tick");
      assert.equal(cue.time, time, "no queued ticks");
      assert.ok(after > before, "the rendered wheel crossed a pocket");
      assert.ok(!ticks.length || time - ticks.at(-1) >= 70, "tick clips do not stack");
      ticks.push(time);
    }
  }
  assert.ok(ticks.filter((time) => time <= 1000).length > ticks.filter((time) => time >= 1800).length);
  assert.ok(ticks.some((time) => time >= 1800));
  f.frame(2800);
  assert.equal(f.sounds.at(-1), "roulette.land");
});

for (const [net, outcome] of [[35, "roulette.win"], [-1, "roulette.loss"], [0], [undefined]]) {
  test(`roulette lands once and plays the viewer outcome for net ${net}`, async () => {
    const f = fixture("roulette");
    const result = receipt(0, net);
    f.view.show();
    await f.respond("state", rouletteState(openRound));
    await f.poll(rouletteState(null, result));
    await f.poll(rouletteState(null, result));
    assert.deepEqual(f.sounds, ["roulette.launch"]);
    f.frame(2799);
    assert.notEqual(f.node("roulette-result-number").textContent, 0);
    f.frame(2800);
    assertPocket(f.node("roulette-ball"), f.node("roulette-wheel"), 0);
    await f.poll(rouletteState(null, result));
    assert.deepEqual(f.sounds.filter((name) => name !== "roulette.tick"),
      ["roulette.launch", "roulette.land", ...(outcome ? [outcome] : [])]);
    assert.equal(f.cues.find(({ name }) => name === "roulette.land").time, 2800);
    if (outcome) assert.ok(f.cues.at(-1).time >= 2800);
  });
}

test("roulette recovers a confirmed first wager after a lost reply and sounds it once", async () => {
  const f = fixture("roulette");
  f.view.show();
  await f.respond("state", rouletteState());
  f.nodes("roulette-cell roulette-cell--number")[0].dispatchEvent(new Event("click"));
  f.node("roulette-form").dispatchEvent(new Event("submit", { cancelable: true }));
  assert.equal(f.calls.at(-1).action, "bet");
  assert.equal(f.calls.at(-1).fields.bet, "10");
  f.calls.at(-1).reject(new Error("connection lost"));
  await f.flush();
  const result = { ...receipt(0, 350), your_bets: [{ request_id: "wager", amount: 10, space: "0" }] };
  await f.respond("state", rouletteState(null, result));
  f.frame(2800);
  await f.poll(rouletteState(null, result));
  assert.deepEqual(f.sounds, ["roulette.launch", "roulette.land", "roulette.win"]);
});

test("reduced motion lands immediately on the winning pocket and uses discrete wheel steps", async () => {
  const f = fixture("roulette", { reduced: true });
  const wheel = f.node("roulette-wheel");
  f.view.show();
  await f.respond("state", rouletteState(openRound));
  f.frame(950);
  near(rotation(wheel), 0);
  f.frame(1000);
  near(rotation(wheel), pocket);
  await f.poll(rouletteState(null, receipt(32, -1)));
  assertPocket(f.node("roulette-ball"), wheel, 1);
  assert.equal(f.node("roulette-result-number").textContent, 32);
  assert.deepEqual(f.sounds, ["roulette.tick", "roulette.land", "roulette.loss"]);
  f.frame(2000);
  assert.equal(f.node("roulette-ball").hidden, true);
});

test("suspending roulette settles a live result silently and clears the pending frame", async () => {
  const f = fixture("roulette");
  f.view.show();
  await f.respond("state", rouletteState(openRound));
  await f.poll(rouletteState(null, receipt(0, 35)));
  f.page("pagehide");
  assert.equal(f.frames.size, 0);
  assertPocket(f.node("roulette-ball"), f.node("roulette-wheel"), 0);
  assert.deepEqual(f.sounds, ["roulette.launch"]);
  f.page("pageshow");
  assert.equal(f.frames.size, 1);
});

test("a roulette refund leaves the ball hidden and preserves the idle cadence", async () => {
  const f = fixture("roulette");
  f.view.show();
  await f.respond("state", rouletteState(openRound));
  f.frame(900);
  await f.poll(rouletteState(null, receipt(null, 0)));
  assert.equal(f.node("roulette-ball").hidden, true);
  f.frame(1000);
  near(rotation(f.node("roulette-wheel")), pocket);
  assert.deepEqual(f.sounds, ["roulette.tick"]);
});

test("poker seats render their names and mark the acting player's avatar", async () => {
  const f = fixture("poker", { avatar: true });
  f.view.show();
  await f.respond("state", {
    balance: 500, min_buy_in: 100, max_buy_in: 1000, small_blind: 5, big_blind: 10,
    table: {
      id: "poker-round", status: "playing", street: "preflop", board: [], actor: 1, you: 0,
      pot: 15, remaining_ms: 1000,
      seats: [
        { id: "1", name: "Алиса", stack: 100, rank: null, cards: ["A♥", "3♣"] },
        { id: "2", name: "Борис", stack: 100, rank: null, cards: null },
        { id: null, name: "Бот", stack: 100, rank: null, cards: null },
      ],
    },
  });
  assert.deepEqual(f.nodes("game-player-avatar").map(({ id, name, active }) => [id, name, active]),
    [["1", "Алиса", false], ["2", "Борис", true], [null, "Бот", false]]);
  assert.deepEqual(f.nodes("game-player-identity").map((identity) => identity.children.at(-1).textContent),
    ["Алиса (вы)", "Борис", "Бот"]);
});

test("blackjack marks the avatar of the player with the active hand", async () => {
  const f = fixture("blackjack", { avatar: true });
  const hand = (active) => ({ active, cards: ["9♥", "6♣"], total: 15, bet: 10 });
  f.view.show();
  await f.respond("state", {
    user_id: "1", balance: 500, actions: [],
    table: {
      id: "blackjack-round", phase: "playing", current_user: "2", remaining_ms: 1000,
      dealer: { cards: ["J♥", null], total: 10 },
      players: [
        { id: "1", name: "Алиса", bet: 10, hands: [hand(false)] },
        { id: "2", name: "Борис", bet: 10, hands: [hand(false), hand(true)] },
      ],
    },
  });
  assert.deepEqual(f.nodes("game-player-avatar").map(({ id, name, active }) => [id, name, active]),
    [["1", "Алиса", false], ["2", "Борис", true]]);
  assert.deepEqual(f.nodes("game-player-identity").map((identity) => identity.children.at(-1).textContent),
    ["Алиса (вы)", "Борис"]);
});

const symbols = Array.from({ length: 6 }, (_, index) => ({ icon: String(index), name: String(index), payouts: [] }));
const slotsState = (spin = null) => ({ spin, symbols, paylines: [], balance: 500, min_bet: 1, max_bet: 500 });
const slotsResult = slotsState({
  id: "spin", stake: 1, payout: 0, wins: [],
  grid: [[5, 1, 2, 3, 4], [4, 2, 3, 4, 5], [3, 3, 4, 5, 0]],
});
const winningSlots = {
  ...slotsResult,
  paylines: [{ name: "Top", rows: [0, 0, 0, 0, 0] }],
  spin: {
    ...slotsResult.spin,
    grid: [[5, 5, 5, 3, 4], ...slotsResult.spin.grid.slice(1)],
    payout: 1,
    wins: [{ line: 0, count: 3, symbol: 5, multiplier: 1 }],
  },
};

async function startSlots(options, initial = slotsState()) {
  const f = fixture("slots", options);
  f.view.show();
  await f.respond("state", initial);
  f.node("casino-toolbar").dispatchEvent(new Event("submit", { cancelable: true }));
  return f;
}

test("slots brakes from the rendered reel position, then sounds each individual stop", async () => {
  const f = await startSlots();
  assert.deepEqual(f.sounds, ["slots.spin"]);
  assert.ok(f.animations[0].options.duration < symbols.length * 65);
  f.animations[0].progress = 2.25 / symbols.length;
  await f.respond("spin", slotsResult);
  const braking = f.animations.slice(5);
  assert.equal(braking.length, 5);
  const firstTrack = f.nodes("slots-strip")[0].children.at(-1);
  assert.deepEqual(firstTrack.children.slice(0, 3).map((child) => child.textContent), ["2", "3", "4"]);
  assert.match(braking[0].keyframes[0].transform, /-8\.3333/);
  assert.ok(braking[0].options.duration > 540 && braking[4].options.duration > 1140);
  for (const [index, animation] of braking.entries()) {
    animation.finish();
    await f.flush();
    assert.equal(f.sounds.filter((name) => name === "slots.stop").length, index + 1);
  }
  assert.equal(f.sounds.at(-1), "slots.spin:end");
  assert.ok(!f.sounds.includes("slots.win"));
  assert.deepEqual(f.nodes("slots-strip")[0].children.map((child) => child.textContent), ["5", "4", "3"]);
});

test("slots keeps the loop through the last actual stop, then plays a line win once", async () => {
  const f = await startSlots();
  await f.respond("spin", winningSlots);
  const braking = f.animations.slice(5);
  const [last, ...earlier] = braking;
  for (const animation of earlier) animation.finish();
  await f.flush();
  assert.deepEqual(f.sounds, ["slots.spin", ...Array(4).fill("slots.stop")]);
  last.finish();
  await f.flush();
  assert.deepEqual(f.sounds, ["slots.spin", ...Array(5).fill("slots.stop"), "slots.spin:end", "slots.win"]);
});

test("slots does not sound stops or a win on a repeated spin receipt", async () => {
  const f = await startSlots({}, winningSlots);
  assert.equal(f.calls.at(-1).fields.expected_round, winningSlots.spin.id);
  await f.respond("spin", winningSlots);
  assert.equal(f.animations.length, 5);
  assert.deepEqual(f.sounds, ["slots.spin", "slots.spin:end"]);
});

for (const [name, cancel] of [
  ["view hide", (f) => f.view.hide()],
  ["document hide", (f) => f.visibility(true)],
  ["page suspension", (f) => f.page("pagehide")],
]) {
  test(`slots releases the loop without reel-stop sounds on ${name}`, async () => {
    const f = await startSlots();
    await f.respond("spin", winningSlots);
    f.animations[5].finish();
    cancel(f);
    await f.flush();
    assert.deepEqual(f.sounds, ["slots.spin", "slots.spin:end"]);
    assert.ok(f.nodes("slots-symbol").every((cell) => cell.style.visibility === ""));
  });
}

test("slots stops its loop when the spin request fails", async () => {
  const f = await startSlots();
  f.calls.at(-1).reject(new Error("connection lost"));
  await f.flush();
  assert.deepEqual(f.sounds, ["slots.spin", "slots.spin:end"]);
  await f.respond("state", winningSlots);
  assert.deepEqual(f.sounds, ["slots.spin", "slots.spin:end"]);
  assert.equal(f.nodes("slots-strip")[0].children[0].textContent, "5");
});

test("slots does not restart audio when returning after a partial stop sequence", async () => {
  const f = await startSlots();
  await f.respond("spin", winningSlots);
  f.animations[5].finish();
  await f.flush();
  f.visibility(true);
  f.visibility(false);
  await f.flush();
  assert.deepEqual(f.sounds, ["slots.spin", "slots.stop", "slots.spin:end"]);
  assert.equal(f.nodes("slots-strip")[0].children[0].textContent, "5");
});

test("slots plays a confirmed win with reduced motion and skips the loop and stop sequence", async () => {
  const f = await startSlots({ reduced: true });
  await f.respond("spin", winningSlots);
  assert.equal(f.animations.length, 0);
  assert.deepEqual(f.sounds, ["slots.win"]);
});

test("slots plays the confirmed win when reduced motion ends an active spin", async () => {
  const f = await startSlots();
  await f.respond("spin", winningSlots);
  f.reduce();
  await f.flush();
  assert.deepEqual(f.sounds, ["slots.spin", "slots.spin:end", "slots.win"]);
});

test("slots can present a result with reduced motion and no audio API", async () => {
  const f = await startSlots({ reduced: true, audio: false });
  await f.respond("spin", slotsResult);
  assert.equal(f.animations.length, 0);
  assert.deepEqual(f.nodes("slots-strip")[0].children.map((child) => child.textContent), ["5", "4", "3"]);
});

test("mines only sounds the requested tile, including a confirmed reveal recovered after a timeout", async () => {
  const f = fixture("mines");
  const round = { id: "field", status: "active", cells: Array(25).fill("hidden"), safe_count: 0,
    mine_count: 3, multiplier: 100, stake: 10, remaining_ms: 1000 };
  const state = { balance: 500, min_bet: 1, max_bet: 500, idle_seconds: 900, house_edge_bps: 100 };
  f.view.show();
  await f.respond("state", { ...state, round });
  const tiles = f.nodes("mines-tile");
  tiles[0].dispatchEvent(new Event("click"));
  round.cells[0] = round.cells[1] = "safe";
  await f.respond("reveal", { ...state, round: { ...round, safe_count: 2 } });
  assert.deepEqual(f.sounds, ["mines.safe"]);

  tiles[2].dispatchEvent(new Event("click"));
  f.calls.at(-1).reject(new Error("timeout"));
  await f.flush();
  round.cells[2] = "hit";
  await f.respond("state", { ...state, round: { ...round, status: "lost" } });
  assert.deepEqual(f.sounds, ["mines.safe", "mines.blast"]);
});
