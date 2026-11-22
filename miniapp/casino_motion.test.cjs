const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const test = require("node:test");
const { runInNewContext } = require("node:vm");

function fixture(game, { reduced = false, audio = true } = {}) {
  const calls = [];
  const sounds = [];
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
    classList = { toggle() {} };
    value = "";
    append(...children) {
      for (const child of children) child.parent = this;
      this.children.push(...children);
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
  const window = Object.assign(new EventTarget(), {
    matchMedia: () => motion,
    getComputedStyle: () => ({ rowGap: "2px" }),
    GameAudio: audio ? {
      play: (name) => sounds.push(name),
      startLoop(name) {
        sounds.push(name);
        return () => sounds.push(name + ":end");
      },
    } : undefined,
  });
  const file = game === "roulette" ? "roulette.js" : "casino_slots.js";
  runInNewContext(readFileSync(join(__dirname, file), "utf8"), {
    document, window, matchMedia: window.matchMedia, performance: { now: () => time },
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
    root, request: (action) => new Promise((resolve, reject) => calls.push({ action, resolve, reject })),
  });

  function descendants(node) {
    return [node, ...node.children.flatMap(descendants)];
  }
  const nodes = (className) => descendants(root).filter((node) => node.className === className);
  const flush = () => new Promise(setImmediate);
  return {
    animations, sounds, calls, frames, timers, intervals, view, nodes, flush,
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
const receipt = (number) => ({ id: "round", number, players: [] });
const rouletteState = (round = null, last_result = null) =>
  ({ round_id: "round", round, last_result, balance: 500, min_bet: 1, max_bet: 500, max_bets: 10 });
const rotation = (wheel) => Number(wheel.style.transform?.match(/rotate\((.+)deg\)/)?.[1] ?? 0);
const near = (actual, expected) => assert.ok(Math.abs(actual - expected) < 1e-8, `${actual} ≈ ${expected}`);

function assertPocket(ball, wheel, index) {
  assert.equal(ball.hidden, false);
  const angle = (rotation(wheel) + index * pocket) * Math.PI / 180;
  near(parseFloat(ball.style.left), 50 + 34.2 * Math.sin(angle));
  near(parseFloat(ball.style.top), 50 - 34.2 * Math.cos(angle));
}

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

test("reduced motion lands immediately on the winning pocket and uses discrete wheel steps", async () => {
  const f = fixture("roulette", { reduced: true });
  const wheel = f.node("roulette-wheel");
  f.view.show();
  await f.respond("state", rouletteState(openRound));
  f.frame(950);
  near(rotation(wheel), 0);
  f.frame(1000);
  near(rotation(wheel), pocket);
  await f.poll(rouletteState(null, receipt(32)));
  assertPocket(f.node("roulette-ball"), wheel, 1);
  assert.equal(f.node("roulette-result-number").textContent, 32);
  assert.deepEqual(f.sounds, ["roulette.tick", "roulette.land"]);
  f.frame(2000);
  assert.equal(f.node("roulette-ball").hidden, true);
});

test("suspending roulette settles a live result silently and clears the pending frame", async () => {
  const f = fixture("roulette");
  f.view.show();
  await f.respond("state", rouletteState(openRound));
  await f.poll(rouletteState(null, receipt(0)));
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
  await f.poll(rouletteState(null, receipt(null)));
  assert.equal(f.node("roulette-ball").hidden, true);
  f.frame(1000);
  near(rotation(f.node("roulette-wheel")), pocket);
  assert.deepEqual(f.sounds, ["roulette.tick"]);
});

const symbols = Array.from({ length: 6 }, (_, index) => ({ icon: String(index), name: String(index), payouts: [] }));
const slotsState = (spin = null) => ({ spin, symbols, paylines: [], balance: 500, min_bet: 1, max_bet: 500 });
const slotsResult = slotsState({
  id: "spin", stake: 1, payout: 0, wins: [],
  grid: [[5, 1, 2, 3, 4], [4, 2, 3, 4, 5], [3, 3, 4, 5, 0]],
});

async function startSlots(options) {
  const f = fixture("slots", options);
  f.view.show();
  await f.respond("state", slotsState());
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
  assert.deepEqual(f.nodes("slots-strip")[0].children.map((child) => child.textContent), ["5", "4", "3"]);
});

for (const [name, cancel] of [
  ["view hide", (f) => f.view.hide()],
  ["document hide", (f) => f.visibility(true)],
  ["page suspension", (f) => f.page("pagehide")],
  ["reduced-motion toggle", (f) => f.reduce()],
]) {
  test(`slots releases the loop without reel-stop sounds on ${name}`, async () => {
    const f = await startSlots();
    await f.respond("spin", slotsResult);
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
  await f.respond("state", slotsState());
});

test("slots can present a result with reduced motion and no audio API", async () => {
  const f = await startSlots({ reduced: true, audio: false });
  await f.respond("spin", slotsResult);
  assert.equal(f.animations.length, 0);
  assert.deepEqual(f.nodes("slots-strip")[0].children.map((child) => child.textContent), ["5", "4", "3"]);
});
