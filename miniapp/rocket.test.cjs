const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const source = readFileSync(join(__dirname, 'rocket.js'), 'utf8');
const context = vm.createContext({});
vm.runInContext(source, context);
const createRocketFeed = vm.runInContext('createRocketFeed', context);

function snapshot(flight = null, balance = 500) {
  return { balance, min_bet: 10, max_bet: 1000, flight };
}

function flight(status, overrides = {}) {
  return { id: '9223372036854775807', status, stake: 100, elapsed_ms: 0,
    multiplier: 100, auto: null, payout: null, automatic: false, ...overrides };
}

function transport() {
  const requests = [];
  const timers = new Map();
  let clock = 0;
  let nextTimer = 0;
  return {
    requests, timers,
    request: (path, body) => new Promise((resolve, reject) => requests.push({ path, body, resolve, reject })),
    now: () => clock,
    setTimer: (callback, delay) => {
      timers.set(++nextTimer, { callback, delay });
      return nextTimer;
    },
    clearTimer: (id) => timers.delete(id),
    tick() {
      const [id, { callback, delay }] = timers.entries().next().value;
      timers.delete(id);
      clock += delay;
      callback();
      return delay;
    },
    async reply(request, value) {
      request.resolve(value);
      await new Promise(setImmediate);
    },
    async poll(value) {
      this.tick();
      const request = requests.at(-1);
      assert.equal(request.path, 'api/rocket/state');
      await this.reply(request, value);
    },
    starts: () => requests.filter(({ path }) => path === 'api/rocket/start').map(({ body }) =>
      JSON.parse(JSON.stringify(body))),
  };
}

function fixture(canAutoStart = () => true) {
  const io = transport();
  let current;
  const feed = createRocketFeed({ ...io, canAutoStart, onChange: (view) => { current = view; } });
  return { ...io, feed, current: () => current };
}

function viewFixture(reducedMotion = false) {
  const drawing = {};
  const gradient = { addColorStop() {} };
  const canvas = {
    setTransform() {}, fillRect() {}, fillText() {}, beginPath() {}, moveTo() {}, lineTo() {},
    fill() {}, save() {}, restore() {}, rotate() {}, quadraticCurveTo() {}, bezierCurveTo() {}, closePath() {},
    clearRect() { delete drawing.ship; delete drawing.curve; delete drawing.marker; delete drawing.burst; },
    translate(x, y) { drawing.ship = [x, y]; },
    arc(x, y, radius) {
      if (radius === 4) drawing.marker ??= [x, y];
      if (radius === 5.25) drawing.burst = [x, y];
    },
    stroke(path) { if (path) drawing.curve = path.points; },
    createLinearGradient() { return gradient; },
  };
  class Path2D {
    constructor(path) { this.points = path ? [...path.points] : []; }
    moveTo(x, y) { this.points.push([x, y]); }
    lineTo(x, y) { this.points.push([x, y]); }
    closePath() {}
  }
  class Element extends EventTarget {
    dataset = {};
    children = [];
    value = '';
    attributes = new Map();
    setAttribute(name, value) { this.attributes.set(name, value); }
    append(...children) { this.children.push(...children); }
    insertBefore(child, reference) { this.children.splice(this.children.indexOf(reference), 0, child); }
    getContext() { return canvas; }
  }
  const html = readFileSync(join(__dirname, 'index.html'), 'utf8');
  const elements = new Map(Array.from(html.matchAll(/\bid="(rocket[^"]*)"/g), ([, id]) => [id, new Element()]));
  Object.assign(elements.get('rocket-chart'), { clientWidth: 350, clientHeight: 220 });
  const form = elements.get('rocket-form');
  const settings = elements.get('rocket-settings');
  form.append(settings, elements.get('rocket-action'));
  const document = Object.assign(new EventTarget(), {
    hidden: false,
    createElement: () => new Element(),
    getElementById: (id) => elements.get(id),
  });
  const motion = Object.assign(new EventTarget(), { matches: reducedMotion });
  const window = Object.assign(new EventTarget(), { matchMedia: () => motion });
  const navigator = { onLine: true };
  const io = transport();
  let paint;
  const browser = vm.createContext({
    document, window, navigator, Path2D, request: io.request,
    performance: { now: io.now }, setTimeout: io.setTimer, clearTimeout: io.clearTimer,
    ResizeObserver: class { observe() {} },
    requestAnimationFrame: (callback) => { paint = callback; return 1; },
    cancelAnimationFrame: () => { paint = null; },
  });
  vm.runInContext(source, browser);
  const view = vm.runInContext('createRocketView(request)', browser);
  const [, panel] = form.children;
  const [setup, status, stop] = panel.children;
  const [, rounds, start] = setup.children;
  return {
    ...io, view, document, window, navigator, elements, settings, setup, status, rounds, start, stop,
    drawing, motion, hasFrame: () => Boolean(paint),
    frame(sinceSample = 0) {
      const callback = paint;
      paint = null;
      callback(io.now() + sinceSample);
    },
  };
}

test('start uses server flight ID and optional auto, and ignores a late state poll', async () => {
  const f = fixture();
  const old = flight('crashed');
  f.feed.show();
  await f.reply(f.requests[0], snapshot(old));
  f.tick();
  const start = f.feed.launch('100', '1.75');
  assert.equal(f.requests[2].path, 'api/rocket/start');
  assert.equal(JSON.stringify(f.requests[2].body), JSON.stringify({ bet: '100', auto: '1.75', expected_id: old.id }));
  const next = flight('countdown', { id: '9223372036854775808', elapsed_ms: -3000, auto: 175 });
  f.requests[2].resolve(snapshot(next, 400));
  await start;
  await f.reply(f.requests[1], snapshot(old));
  assert.equal(f.current().snapshot.flight, next);
  assert.equal(f.current().snapshot.balance, 400);
  assert.ok(f.tick() >= 400);
  assert.equal(f.requests[3].path, 'api/rocket/state');
  assert.equal(f.requests[3].body, undefined);
});

test('cashout is manual, bound to a confirmed flying ID, and sent once while pending', async () => {
  const f = fixture();
  f.feed.show();
  const countdown = flight('countdown', { auto: 150, elapsed_ms: -40 });
  await f.reply(f.requests[0], snapshot(countdown));
  await f.feed.cashout();
  assert.equal(f.requests.length, 1);
  assert.ok(f.tick() <= 650);
  const flying = { ...countdown, status: 'flying', elapsed_ms: 4000, multiplier: 158 };
  await f.reply(f.requests[1], snapshot(flying));
  assert.equal(f.requests.length, 2, 'an auto target never triggers a client cashout');
  const cashout = f.feed.cashout();
  await f.feed.cashout();
  assert.equal(f.requests.length, 3);
  assert.equal(f.requests[2].path, 'api/rocket/cashout');
  assert.equal(f.requests[2].body.id, countdown.id);
  assert.equal(f.current().pending, 'cashout');
  const paid = { ...flying, status: 'paid', payout: 158 };
  f.requests[2].resolve(snapshot(paid, 558));
  await cashout;
  assert.equal(f.current().snapshot.flight, paid);
  assert.equal(f.current().pending, null);
});

test('a timed-out launch resyncs before enabling actions and never repeats the POST', async () => {
  const f = fixture();
  f.feed.show();
  await f.reply(f.requests[0], snapshot());
  const start = f.feed.launch('100', null);
  assert.equal(JSON.stringify(f.requests[1].body), '{"bet":"100","auto":null,"expected_id":null}');
  f.requests[1].reject(Object.assign(new Error('timeout'), { name: 'TimeoutError' }));
  await start;
  assert.equal(f.current().needsSync, true);
  assert.equal(f.requests[2].path, 'api/rocket/state');
  await f.feed.launch('100', null);
  assert.equal(f.requests.length, 3);
  await f.reply(f.requests[2], snapshot(flight('countdown', { elapsed_ms: -1800 }), 400));
  assert.equal(f.current().needsSync, false);
  assert.equal(f.current().snapshot.balance, 400);
});

test('returning to the view requires a fresh sample and drops the old view’s poll', async () => {
  const f = fixture();
  f.feed.show();
  f.feed.hide();
  f.feed.show();
  assert.equal(f.current().needsSync, true);
  await f.reply(f.requests[1], snapshot(flight('refunded')));
  await f.reply(f.requests[0], snapshot(flight('flying'), 400));
  assert.equal(f.current().snapshot.flight.status, 'refunded');
  assert.equal(f.timers.size, 1);
  f.feed.hide();
  assert.equal(f.timers.size, 0);
});

test('auto start is opt-in and spends only the selected additional rounds at the original settings', async () => {
  const f = fixture();
  const first = flight('countdown', { id: 'first', stake: 70, auto: 175 });
  const second = flight('countdown', { id: 'second', stake: 70, auto: 175 });
  const third = flight('countdown', { id: 'third', stake: 70, auto: 175 });
  f.feed.show();
  await f.reply(f.requests[0], snapshot(first));
  assert.equal(f.current().repeat, null);
  f.feed.startRepeat(2);
  await f.poll(snapshot({ ...first, status: 'flying' }));
  assert.deepEqual(f.starts(), []);
  await f.poll(snapshot({ ...first, status: 'crashed' }, 430));
  assert.deepEqual(f.starts(), [], 'the completed round remains visible before the next start');
  await f.poll(snapshot({ ...first, status: 'crashed' }, 430));
  await f.reply(f.requests.at(-1), snapshot(second, 360));
  assert.equal(f.current().repeat.remaining, 1);
  const paid = { ...second, status: 'flying', cashout_multiplier: 175, payout: 122, automatic: true };
  const crashed = { ...paid, status: 'crashed', multiplier: 220 };
  await f.poll(snapshot(paid, 482));
  await f.poll(snapshot(paid, 482));
  assert.equal(f.starts().length, 1, 'automatic payout leaves the next flight queued');
  await f.poll(snapshot(crashed, 482));
  await f.poll(snapshot(crashed, 482));
  await f.reply(f.requests.at(-1), snapshot(third, 412));
  await f.poll(snapshot({ ...third, status: 'crashed' }, 412));
  await f.poll(snapshot({ ...third, status: 'crashed' }, 412));
  assert.deepEqual(f.starts(), [
    { bet: '70', auto: '1.75', expected_id: 'first' },
    { bet: '70', auto: '1.75', expected_id: 'second' },
  ]);
  assert.equal(f.current().repeat, null);
});

test('auto start keeps manual cashout mode and waits for the crash after payout', async () => {
  const f = fixture();
  const flying = flight('flying');
  f.feed.show();
  await f.reply(f.requests[0], snapshot(flying));
  f.feed.startRepeat(1);
  const cashout = f.feed.cashout();
  assert.equal(f.requests.at(-1).path, 'api/rocket/cashout');
  const paid = snapshot({ ...flying, payout: 110, cashout_multiplier: 110 }, 510);
  await f.reply(f.requests.at(-1), paid);
  await cashout;
  assert.deepEqual(f.starts(), []);
  const duplicate = f.feed.cashout();
  assert.equal(f.requests.length, 2, 'a paid flight permits neither cashout nor launch');
  await duplicate;
  const earlyLaunch = f.feed.launch('100', null);
  assert.equal(f.requests.length, 2, 'the next stake is blocked until crash');
  await earlyLaunch;
  const cinematic = snapshot({ ...paid.flight, elapsed_ms: 12739, multiplier: 50000, cinematic_from_ms: 8200 }, 510);
  await f.poll(cinematic);
  await f.poll(cinematic);
  assert.deepEqual(f.starts(), [], 'active polling never spends the next stake');
  const crashed = snapshot({ ...cinematic.flight, status: 'crashed', elapsed_ms: 13339, multiplier: 100000 }, 510);
  await f.poll(crashed);
  assert.deepEqual(f.starts(), [], 'the crash stays visible before the next start');
  await f.poll(crashed);
  assert.deepEqual(f.starts(), [{ bet: '100', auto: null, expected_id: flying.id }]);
});

test('reloading a cashed-out flight shows the fixed payout and keeps launch locked until crash', async () => {
  const f = viewFixture();
  const action = f.elements.get('rocket-action');
  const payoutLabel = f.elements.get('rocket-payout-label');
  const payout = f.elements.get('rocket-payout');
  const liveMultiplier = f.elements.get('rocket-multiplier');
  const clock = f.elements.get('rocket-time');
  const paid = flight('flying', { multiplier: 210, elapsed_ms: 6500, auto: 175,
    payout: 175, cashout_multiplier: 175, automatic: true });
  f.view.show();
  await f.reply(f.requests[0], snapshot(paid, 575));
  assert.equal(f.settings.hidden, true);
  assert.equal(action.attributes.get('aria-disabled'), 'true');
  assert.match(payoutLabel.textContent, /1,75×/);
  assert.equal(payout.textContent, '175 мон.');
  assert.equal(f.stop.hidden, true, 'reload never opts back into auto start');
  f.frame();
  f.frame(400);
  assert.equal(clock.textContent, '6,5 с');
  assert.equal(liveMultiplier.textContent, '2,10×');

  await f.poll(snapshot({ ...paid, status: 'crashed', multiplier: 260, elapsed_ms: 8271 }, 575));
  f.frame();
  assert.equal(liveMultiplier.textContent, '2,60×');
  assert.equal(f.elements.get('rocket-multiplier-caption').textContent, 'Пик полёта');
  assert.equal(action.attributes.get('aria-disabled'), 'false');
  assert.match(payoutLabel.textContent, /1,75×/);
  assert.equal(payout.textContent, '175 мон.');
  assert.deepEqual(f.starts(), []);
});

test('a manual launch supersedes a queued auto start and keeps its own wager settings', async () => {
  const f = fixture();
  const flying = flight('flying');
  f.feed.show();
  await f.reply(f.requests[0], snapshot(flying));
  f.feed.startRepeat(2);
  await f.poll(snapshot({ ...flying, status: 'crashed' }));
  const launch = f.feed.launch('50', '2.5');
  const next = flight('countdown', { id: 'manual', stake: 50, auto: 250 });
  await f.reply(f.requests.at(-1), snapshot(next));
  await launch;
  const crashed = snapshot({ ...next, status: 'crashed' });
  await f.poll(crashed);
  await f.poll(crashed);
  assert.deepEqual(f.starts(), [{ bet: '50', auto: '2.5', expected_id: flying.id }]);
  assert.equal(f.current().repeat, null);
});

test('auto start rejects blank, fractional, and out-of-range round counts', async () => {
  const f = fixture();
  f.feed.show();
  const flying = flight('flying');
  await f.reply(f.requests[0], snapshot(flying));
  for (const rounds of [0, -1, 1.5, 11, NaN]) f.feed.startRepeat(rounds);
  assert.equal(f.current().repeat, null);
  const crashed = snapshot({ ...flying, status: 'crashed' });
  await f.poll(crashed);
  await f.poll(crashed);
  assert.deepEqual(f.starts(), []);
});

for (const [reason, interrupt] of [
  ['explicit stop', (f) => f.feed.stopRepeat()],
  ['leaving the view', (f) => { f.feed.hide(); f.feed.show(); }],
  ['visibility or connectivity changing before the event is delivered', (_f, block) => block()],
  ['a failed state request', async (f) => {
    f.requests.at(-1).reject(new TypeError('network error'));
    await new Promise(setImmediate);
  }],
]) {
  test('auto start requires a new opt-in after ' + reason, async () => {
    let allowed = true;
    const f = fixture(() => allowed);
    const flying = flight('flying');
    const crashed = snapshot({ ...flying, status: 'crashed' });
    f.feed.show();
    await f.reply(f.requests[0], snapshot(flying));
    f.feed.startRepeat(2);
    await f.poll(crashed);
    f.tick();
    const pending = f.requests.at(-1);
    await interrupt(f, () => { allowed = false; });
    await f.reply(pending, crashed);
    if (f.requests.at(-1) !== pending) await f.reply(f.requests.at(-1), crashed);
    allowed = true;
    await f.poll(crashed);
    assert.deepEqual(f.starts(), []);
    assert.equal(f.current().repeat, null);
  });
}

for (const [name, replacement] of [
  ['the tracked flight changes', snapshot(flight('crashed', { id: 'elsewhere' }))],
  ['the round is refunded', snapshot(flight('refunded'))],
  ['the fixed stake exceeds the available balance', snapshot(flight('crashed'), 99)],
]) {
  test('auto start stops when ' + name, async () => {
    const f = fixture();
    f.feed.show();
    await f.reply(f.requests[0], snapshot(flight('flying')));
    f.feed.startRepeat(2);
    await f.poll(replacement);
    await f.poll(replacement);
    assert.deepEqual(f.starts(), []);
    assert.equal(f.current().repeat, null);
  });
}

for (const outcome of ['stop', 'network error']) {
  test('auto start clears the remaining rounds after ' + outcome + ' during a start request', async () => {
    const f = fixture();
    const next = flight('countdown', { id: 'next' });
    const crashed = snapshot(flight('crashed'));
    f.feed.show();
    await f.reply(f.requests[0], snapshot(flight('flying')));
    f.feed.startRepeat(2);
    await f.poll(crashed);
    await f.poll(crashed);
    const start = f.requests.at(-1);
    assert.equal(start.path, 'api/rocket/start');
    if (outcome === 'stop') {
      f.feed.stopRepeat();
      await f.reply(start, snapshot(next));
    } else {
      start.reject(new TypeError('network error'));
      await new Promise(setImmediate);
      assert.equal(f.requests.at(-1).path, 'api/rocket/state');
      await f.reply(f.requests.at(-1), snapshot(next));
    }
    await f.poll(snapshot({ ...next, status: 'crashed' }));
    await f.poll(snapshot({ ...next, status: 'crashed' }));
    assert.equal(f.starts().length, 1);
    assert.equal(f.current().repeat, null);
  });
}

test('the Rocket form shows the additional stake limit and offers Stop while the wager settings are hidden', async () => {
  const f = viewFixture();
  const flying = flight('flying', { stake: 70, auto: 175 });
  const crashed = snapshot({ ...flying, status: 'crashed' });
  f.view.show();
  await f.reply(f.requests[0], snapshot(flying));
  assert.equal(f.settings.hidden, true);
  assert.equal(f.setup.hidden, false);
  assert.equal(f.start.type, 'button');
  assert.equal(f.start.disabled, true);
  f.rounds.value = '2';
  f.rounds.dispatchEvent(new Event('change'));
  assert.match(f.status.textContent, /70 мон.*1,75×.*140 мон/);
  assert.equal(f.start.disabled, false);
  f.start.dispatchEvent(new Event('click'));
  assert.equal(f.stop.hidden, false);
  assert.equal(f.stop.type, 'button');
  await f.poll(crashed);
  f.stop.dispatchEvent(new Event('click'));
  assert.equal(f.stop.hidden, true);
  await f.poll(crashed);
  assert.deepEqual(f.starts(), []);
});

for (const [name, interrupt, resume] of [
  ['tab is hidden', (f) => {
    f.document.hidden = true;
    f.document.dispatchEvent(new Event('visibilitychange'));
  }, (f) => {
    f.document.hidden = false;
    f.document.dispatchEvent(new Event('visibilitychange'));
  }],
  ['page is left', (f) => f.window.dispatchEvent(new Event('pagehide')),
    (f) => f.window.dispatchEvent(new Event('pageshow'))],
  ['browser goes offline', (f) => {
    f.navigator.onLine = false;
    f.window.dispatchEvent(new Event('offline'));
  }, (f) => {
    f.navigator.onLine = true;
    f.window.dispatchEvent(new Event('online'));
  }],
  ['view disconnects', (f) => f.view.disconnect('Disconnected'), (f) => f.view.show()],
]) {
  test('the opt-in is cleared when the ' + name, async () => {
    const f = viewFixture();
    const flying = flight('flying');
    const crashed = snapshot({ ...flying, status: 'crashed' });
    f.view.show();
    await f.reply(f.requests[0], snapshot(flying));
    f.rounds.value = '2';
    f.rounds.dispatchEvent(new Event('change'));
    f.start.dispatchEvent(new Event('click'));
    assert.equal(f.stop.hidden, false);
    await f.poll(crashed);
    f.tick();
    const pending = f.requests.at(-1);
    interrupt(f);
    assert.equal(f.stop.hidden, true);
    await f.reply(pending, crashed);
    resume(f);
    if (f.requests.at(-1) !== pending) await f.reply(f.requests.at(-1), crashed);
    if (f.timers.size) await f.poll(crashed);
    assert.deepEqual(f.starts(), []);
  });
}

test('animation frames move the ship and curve together without predicting through a sudden crash', async () => {
  const f = viewFixture();
  f.view.show();
  await f.reply(f.requests[0], snapshot(flight('flying', { elapsed_ms: 1060, multiplier: 113 })));
  f.frame();
  const initial = f.drawing.ship;
  await f.poll(snapshot(flight('flying', { elapsed_ms: 1560, multiplier: 119 })));
  f.frame();
  assert.deepEqual(f.drawing.ship, initial);
  const readout = f.elements.get('rocket-multiplier');
  assert.equal(readout.textContent, '1,19×');

  f.frame(125);
  const first = f.drawing.ship;
  assert.ok(first[0] > initial[0] && first[1] < initial[1]);
  assert.deepEqual(f.drawing.curve.at(-1), first);
  f.frame(250);
  assert.ok(f.drawing.ship[0] > first[0] && f.drawing.ship[1] < first[1]);
  f.frame(500);
  const confirmed = f.drawing.ship;
  f.frame(900); // Delayed crash response.
  assert.deepEqual(f.drawing.ship, confirmed);
  await f.poll(snapshot(flight('crashed', { elapsed_ms: 1578, multiplier: 120 })));
  f.frame(500);
  assert.ok(f.drawing.marker[0] >= confirmed[0] && f.drawing.marker[1] <= confirmed[1]);
  assert.deepEqual(f.drawing.curve.at(-1), f.drawing.marker);
  assert.equal(readout.textContent, '1,20×');
});

test('reduced motion paints confirmed positions once and reveals the cinematic peak at crash', async () => {
  const f = viewFixture(true);
  const paid = { payout: 200, cashout_multiplier: 200, cinematic_from_ms: 9509 };
  f.view.show();
  await f.reply(f.requests[0], snapshot(flight('flying', { ...paid, elapsed_ms: 14000, multiplier: 53696 }), 600));
  f.frame();
  const initial = f.drawing.ship;
  assert.equal(f.hasFrame(), false);
  await f.poll(snapshot(flight('flying', { ...paid, elapsed_ms: 14500, multiplier: 95675 }), 600));
  f.frame();
  assert.ok(f.drawing.ship[0] > initial[0] && f.drawing.ship[1] < initial[1]);
  assert.equal(f.hasFrame(), false);
  assert.equal(f.elements.get('rocket-multiplier').textContent, '956,75×');
  assert.equal(f.elements.get('rocket-action').attributes.get('aria-disabled'), 'true');
  await f.poll(snapshot(flight('crashed', { ...paid, elapsed_ms: 14538, multiplier: 100000 }), 600));
  f.frame();
  assert.equal(f.elements.get('rocket-multiplier').textContent, '1\u00a0000,00×');
  assert.equal(f.elements.get('rocket-payout').textContent, '200 мон.');
  assert.match(f.elements.get('rocket-payout-label').textContent, /2,00×/);
  assert.equal(f.elements.get('rocket-action').attributes.get('aria-disabled'), 'false');
  assert.equal(f.hasFrame(), false);
});

test('a confirmed cinematic crash reveals the peak immediately while the ship eases into the burst', async () => {
  const f = viewFixture();
  const paid = { payout: 200, cashout_multiplier: 200, cinematic_from_ms: 9509 };
  f.view.show();
  await f.reply(f.requests[0], snapshot(flight('flying', { ...paid, elapsed_ms: 13500, multiplier: 30135 }), 600));
  f.frame();
  const lastPaint = f.drawing.ship;
  await f.poll(snapshot(flight('flying', { ...paid, elapsed_ms: 14000, multiplier: 53696 }), 600));
  f.frame();
  assert.deepEqual(f.drawing.ship, lastPaint);
  f.frame(480);
  const beforeCrash = f.drawing.ship;

  await f.poll(snapshot(flight('crashed', { ...paid, elapsed_ms: 14538, multiplier: 100000 }), 600));
  f.frame();
  assert.equal(f.elements.get('rocket-multiplier').textContent, '1\u00a0000,00×');
  assert.equal(f.elements.get('rocket-payout').textContent, '200 мон.');
  assert.equal(f.drawing.marker, undefined);
  assert.deepEqual(f.drawing.ship, beforeCrash);
  f.frame(60);
  const approaching = f.drawing.ship;
  assert.ok(approaching[0] > beforeCrash[0] && approaching[1] < beforeCrash[1]);
  assert.equal(f.drawing.marker, undefined);
  assert.deepEqual(f.drawing.curve.at(-1), approaching);
  f.frame(160);
  assert.equal(f.drawing.ship, undefined);
  assert.ok(f.drawing.marker[0] > approaching[0] && f.drawing.marker[1] < approaching[1]);
  assert.deepEqual(f.drawing.curve.at(-1), f.drawing.marker);
  assert.deepEqual(f.drawing.burst, f.drawing.marker);
});
