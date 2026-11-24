const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const source = readFileSync(join(__dirname, 'rocket.js'), 'utf8');
const context = vm.createContext({});
vm.runInContext(source, context);
const createRocketFeed = vm.runInContext('createRocketFeed', context);
const rocketHistoryPeakTier = vm.runInContext('rocketHistoryPeakTier', context);

function snapshot(flight = emptyRound(), balance = 500) {
  return { balance, min_bet: 10, max_bet: 1000, flight };
}

function flight(status, overrides = {}) {
  const elapsed = overrides.elapsed_ms ?? (status === 'countdown' ? -(overrides.next_launch_ms ?? 15000) : 0);
  const nextLaunch = status === 'countdown' ? -elapsed : Math.max(0, 15000 - elapsed);
  const result = { id: '9223372036854775807', status, stake: 100, elapsed_ms: elapsed,
    multiplier: 100, auto: null, payout: null, automatic: false, next_launch_ms: nextLaunch, ...overrides };
  return {
    can_bet: status === 'countdown' && result.stake === null,
    can_cashout: status === 'flying' && result.stake !== null && result.payout === null,
    ...result,
  };
}

function emptyRound(id = '9223372036854775807', status = 'countdown', overrides = {}) {
  return flight(status, { id, stake: null, ...overrides });
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
    advance(ms) { clock += ms; },
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
    scrollLeft = 0;
    scrollWidth = 0;
    clientWidth = 0;
    value = '';
    attributes = new Map();
    setAttribute(name, value) { this.attributes.set(name, value); }
    getAttribute(name) { return this.attributes.get(name); }
    removeAttribute(name) { this.attributes.delete(name); }
    focus() { this.focused = true; }
    append(...children) { this.children.push(...children); }
    replaceChildren(...children) { this.children = children; }
    insertBefore(child, reference) { this.children.splice(this.children.indexOf(reference), 0, child); }
    getBoundingClientRect() { return { left: 0 }; }
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

test('history peak colors change at the exact hundredth', () => {
  const bands = [
    ['red', [100, 129, 130]],
    ['orange', [131, 149]],
    ['yellow', [150, 199]],
    ['dark-green', [200, 399]],
    ['green', [400, 699]],
    ['light-green', [700, 120000]],
  ];
  for (const [tier, peaks] of bands) {
    for (const peak of peaks) assert.equal(rocketHistoryPeakTier(peak), tier, String(peak));
  }
});

test('history shows recorded peaks and player outcomes with new rounds at the right end', async () => {
  const f = viewFixture();
  f.view.show();
  const viewport = f.elements.get('rocket-history-scroll');
  const list = f.elements.get('rocket-history-list');
  const empty = f.elements.get('rocket-history-empty');
  await f.reply(f.requests[0], snapshot());
  assert.equal(viewport.hidden, true);
  assert.equal(empty.hidden, false);

  const cinematic = {
    id: '11', crash: 320, peak: 9000, more_players: true, players: [
      { id: '1', name: 'Лена', outcome: 'won', cashout_multiplier: 281 },
      { id: '3', name: 'Ира', outcome: 'won', cashout_multiplier: 105 },
      { id: '2', name: 'Саша', outcome: 'lost', cashout_multiplier: null },
    ],
  };
  const first = { ...snapshot(), history: [cinematic, { id: '10', crash: 131, peak: 131, players: [] }] };
  await f.poll(first);
  const [older, card] = list.children;
  const [heading, players, more] = card.children;
  assert.equal(heading.children[0].textContent, '90,00×');
  assert.equal(card.dataset.tier, 'light-green');
  assert.equal(heading.children.length, 1);
  assert.deepEqual(players.children.map(({ children: [avatar, factor] }) =>
    [avatar.dataset.outcome, avatar.textContent, factor.textContent]),
    [['won', 'Л', '2,81×'], ['won', 'И', '1,05×'], ['lost', 'С', '✕']]);
  assert.match(players.children[2].children[0].getAttribute('aria-label'), /Саша.*проиграл/);
  assert.equal(more.textContent, '…');
  assert.equal(older.children[0].children[0].textContent, '1,31×');
  assert.equal(older.dataset.tier, 'orange');
  assert.equal(older.children[1].textContent, 'Без ставок');
  assert.equal(viewport.hidden, false);

  await f.poll(first);
  assert.equal(list.children[1], card);
  await f.poll({ ...snapshot(), history: [{ id: '12', crash: 200, peak: 200, players: [] }, cinematic] });
  assert.equal(list.children[1].children[0].children[0].textContent, '2,00×');
  assert.equal(list.children[0], card);
  f.view.hide();
  assert.equal(viewport.hidden, true);
  assert.equal(list.children.length, 0);
});

function recordRocketAudio(view, withIntensity = true) {
  const sounds = [];
  const intensity = [];
  view.window.GameAudio = {
    play(name, delay = 0) { sounds.push([name, delay]); },
    startLoop(name) {
      sounds.push([name, 'start']);
      const stop = () => sounds.push([name, 'stop']);
      if (withIntensity) stop.setIntensity = (value) => intensity.push(value);
      return stop;
    },
  };
  return { sounds, intensity };
}

test('Rocket plays three identical pulses on the displayed countdown', async () => {
  const f = viewFixture();
  const { sounds } = recordRocketAudio(f);
  f.view.show();
  const countdown = (remaining) => snapshot(emptyRound('current', 'countdown', { next_launch_ms: remaining }));
  await f.reply(f.requests[0], countdown(3100));
  f.frame();
  assert.deepEqual(sounds, []);
  f.frame(100);
  f.frame(200);
  assert.equal(f.elements.get('rocket-time').textContent, '2,9 с');
  assert.deepEqual(sounds, [['rocket.count', 0]]);
  await f.poll(countdown(2700));
  f.frame(700);
  f.frame(1700);
  f.frame(2700);
  assert.deepEqual(sounds, Array.from({ length: 3 }, () => ['rocket.count', 0]));
});

test('Rocket skips missed countdown pulses and ignores clock corrections within the same round', async () => {
  const f = viewFixture();
  const { sounds } = recordRocketAudio(f);
  f.view.show();
  const countdown = (remaining) => snapshot(emptyRound('current', 'countdown', { next_launch_ms: remaining }));
  await f.reply(f.requests[0], countdown(3800));
  f.frame();
  f.frame(2900);
  assert.deepEqual(sounds, [['rocket.count', 0]]);
  await f.poll(countdown(2200));
  f.frame();
  f.frame(2000);
  assert.deepEqual(sounds, [['rocket.count', 0]]);
});

test('a late countdown frame leaves the next pulse on its own', async () => {
  const f = viewFixture();
  const { sounds } = recordRocketAudio(f);
  f.view.show();
  await f.reply(f.requests[0], snapshot(emptyRound('current', 'countdown', { next_launch_ms: 3100 })));
  f.frame();
  f.frame(1090); // 2.1 is too late for the 3-second pulse.
  assert.deepEqual(sounds, []);
  f.frame(1100);
  assert.deepEqual(sounds, [['rocket.count', 0]]);
});

test('the spectator flight loop follows the rendered launch and crash burst', async () => {
  const f = viewFixture();
  const { sounds, intensity } = recordRocketAudio(f);
  f.view.show();
  await f.reply(f.requests[0], snapshot(emptyRound('current', 'countdown', { next_launch_ms: 3900 })));
  f.frame();
  await f.poll(snapshot(emptyRound('current', 'flying', { elapsed_ms: 200, multiplier: 105 })));
  assert.deepEqual(sounds, []);
  f.frame();
  const flying = [['rocket.launch', 0], ['rocket.flight', 'start']];
  assert.deepEqual(sounds, flying);
  await f.poll(snapshot(emptyRound('current', 'flying', { elapsed_ms: 1050, multiplier: 130 })));
  for (let elapsed = 0; elapsed <= 600; elapsed += 16) f.frame(elapsed);
  assert.deepEqual(sounds, flying);
  assert.ok(intensity.length > 1 && intensity.length < 8);
  assert.ok(intensity.every((value, i) => value >= 0 && value <= 1 && (!i || value > intensity[i - 1])));

  const crash = snapshot(emptyRound('current', 'crashed', { elapsed_ms: 1100, multiplier: 132, next_launch_ms: 2000 }));
  await f.poll(crash);
  f.frame();
  f.frame(100);
  assert.equal(f.drawing.marker, undefined);
  assert.deepEqual(sounds, flying);
  f.frame(160);
  assert.ok(f.drawing.burst);
  const finished = [...flying, ['rocket.flight', 'stop'], ['rocket.crash', 0]];
  assert.deepEqual(sounds, finished);
  await f.poll(crash);
  f.frame();
  assert.deepEqual(sounds, finished);
});

test('the first displayed crash sounds once when the server skips a very short flight', async () => {
  const f = viewFixture(true);
  const { sounds } = recordRocketAudio(f);
  f.view.show();
  await f.reply(f.requests[0], snapshot(emptyRound('current', 'countdown', { next_launch_ms: 3800 })));
  f.frame();
  await f.poll(snapshot(emptyRound('current', 'crashed', { elapsed_ms: 0 })));
  assert.deepEqual(sounds, []);
  f.frame();
  assert.ok(f.drawing.marker);
  assert.deepEqual(sounds, [['rocket.crash', 0]]);
});

for (const automatic of [false, true]) {
  test('Rocket plays a confirmed ' + (automatic ? 'automatic' : 'manual') + ' cashout once', async () => {
    const f = viewFixture(true);
    const { sounds } = recordRocketAudio(f, false);
    f.view.show();
    await f.reply(f.requests[0], snapshot(flight('flying')));
    f.frame();
    const playing = [['rocket.flight', 'start']];
    assert.deepEqual(sounds, playing);
    const paid = { payout: 110, cashout_multiplier: 110, automatic };
    if (automatic) {
      await f.poll(snapshot(flight('flying', paid), 510));
    } else {
      f.elements.get('rocket-form').dispatchEvent(new Event('submit', { cancelable: true }));
      const cashout = f.requests.at(-1);
      assert.equal(cashout.path, 'api/rocket/cashout');
      f.frame();
      assert.deepEqual(sounds, playing);
      await f.reply(cashout, snapshot(flight('flying', paid), 510));
    }
    f.frame();
    await f.poll(snapshot(flight('crashed', paid), 510));
    f.frame();
    await f.poll(snapshot(flight('crashed', paid), 510));
    f.frame();
    assert.deepEqual(sounds, [...playing, ['rocket.cashout', 0], ['rocket.flight', 'stop'], ['rocket.crash', 0]]);
  });
}

test('a resumed Rocket view starts the current flight loop without replaying hidden cues', async () => {
  const f = viewFixture();
  const { sounds } = recordRocketAudio(f);
  const visibility = (hidden) => {
    f.document.hidden = hidden;
    f.document.dispatchEvent(new Event('visibilitychange'));
  };
  f.view.show();
  await f.reply(f.requests[0], snapshot(flight('countdown', { next_launch_ms: 2600 })));
  f.frame();
  visibility(true);
  visibility(false);
  f.frame();
  await f.reply(f.requests.at(-1), snapshot(flight('countdown', { next_launch_ms: 2400 })));
  f.frame();
  assert.deepEqual(sounds, [['rocket.count', 0]]);
  visibility(true);
  visibility(false);
  const paid = { payout: 110, cashout_multiplier: 110, automatic: true };
  await f.reply(f.requests.at(-1), snapshot(flight('flying', paid), 510));
  f.frame();
  assert.deepEqual(sounds, [['rocket.count', 0], ['rocket.flight', 'start']]);
  visibility(true);
  assert.deepEqual(sounds, [['rocket.count', 0], ['rocket.flight', 'start'], ['rocket.flight', 'stop']]);
  visibility(false);
  f.frame();
  await f.reply(f.requests.at(-1), snapshot(flight('crashed', paid), 510));
  f.frame();
  assert.deepEqual(sounds, [['rocket.count', 0], ['rocket.flight', 'start'], ['rocket.flight', 'stop']]);
});

test('a clock correction after returning to Rocket cannot replay a pulse from that round', async () => {
  const f = viewFixture();
  const { sounds } = recordRocketAudio(f);
  const countdown = (remaining) => snapshot(emptyRound('current', 'countdown', { next_launch_ms: remaining }));
  f.view.show();
  await f.reply(f.requests[0], countdown(2000));
  f.frame();
  f.frame(1000);
  f.view.hide();
  f.view.show();
  await f.reply(f.requests.at(-1), countdown(2000));
  f.frame();
  f.frame(1000);
  assert.deepEqual(sounds, [['rocket.count', 0], ['rocket.count', 0]]);
});

test('Rocket stops a flight loop when the view is left, the page is left, or the connection closes', async () => {
  for (const suspend of [
    (f) => f.view.hide(),
    (f) => f.window.dispatchEvent(new Event('pagehide')),
    (f) => f.view.disconnect('Disconnected'),
    (f) => {
      f.navigator.onLine = false;
      f.window.dispatchEvent(new Event('offline'));
    },
  ]) {
    const f = viewFixture();
    const { sounds } = recordRocketAudio(f, false);
    f.view.show();
    await f.reply(f.requests[0], snapshot(emptyRound('current', 'flying')));
    f.frame();
    suspend(f);
    if (f.hasFrame()) f.frame();
    assert.deepEqual(sounds, [['rocket.flight', 'start'], ['rocket.flight', 'stop']]);
  }
});

test('a spectator can opt into AutoBet using the form before making any wager', async () => {
  const f = viewFixture();
  f.view.show();
  await f.reply(f.requests[0], snapshot(emptyRound('current', 'flying')));
  const action = f.elements.get('rocket-action');
  assert.equal(action.getAttribute('aria-disabled'), 'true');
  assert.equal(f.elements.get('rocket-flight').hidden, true);
  assert.equal(f.settings.hidden, false);
  assert.equal(f.setup.hidden, false);
  assert.equal(f.rounds.value, '1');
  assert.equal(f.start.disabled, false);
  const bet = f.elements.get('rocket-bet');
  bet.value = '70';
  bet.dispatchEvent(new Event('input'));
  const auto = f.elements.get('rocket-auto');
  auto.value = '1,75';
  auto.dispatchEvent(new Event('input'));
  f.rounds.value = '2';
  f.rounds.dispatchEvent(new Event('change'));
  assert.match(f.status.textContent, /70 мон.*1,75×.*140 мон/);
  assert.equal(f.start.disabled, false);
  f.start.dispatchEvent(new Event('click'));
  assert.equal(f.stop.hidden, false);
  assert.deepEqual(f.starts(), []);
  await f.reply(f.requests.at(-1), snapshot(emptyRound('current', 'crashed')));
  await f.poll(snapshot(emptyRound('next')));
  assert.deepEqual(f.starts(), [{ bet: '70', auto: '1.75', expected_id: 'next' }]);
});

test('opening a new scheduled countdown preserves the unsubmitted AutoBet draft', async () => {
  const f = viewFixture();
  f.view.show();
  await f.reply(f.requests[0], snapshot(emptyRound('current')));
  assert.equal(f.start.disabled, false);
  const bet = f.elements.get('rocket-bet');
  const auto = f.elements.get('rocket-auto');
  f.rounds.value = '4';
  f.rounds.dispatchEvent(new Event('change'));
  bet.value = '70';
  bet.dispatchEvent(new Event('input'));
  auto.value = '1,';
  auto.dispatchEvent(new Event('input'));
  await f.poll(snapshot(emptyRound('current', 'flying')));
  await f.poll(snapshot(emptyRound('current', 'crashed')));
  await f.poll(snapshot(emptyRound('next')));
  assert.equal(f.rounds.value, '4');
  assert.equal(bet.value, '70');
  assert.equal(auto.value, '1,');
  assert.equal(f.start.disabled, false);
  auto.value = '1,75';
  auto.dispatchEvent(new Event('input'));
  assert.match(f.status.textContent, /70 мон.*1,75×.*280 мон/);
  assert.deepEqual(f.starts(), [], 'editing the draft does not opt in');
});

test('cashout is available during the opening grace and a loss only appears after the server ends the flight', async () => {
  const f = viewFixture();
  const action = f.elements.get('rocket-action');
  const message = f.elements.get('rocket-message');
  f.view.show();
  await f.reply(f.requests[0], snapshot(flight('flying', { elapsed_ms: 200, multiplier: 100 })));
  assert.equal(action.getAttribute('aria-disabled'), 'false');
  assert.equal(action.textContent, 'Вывести');
  await f.poll(snapshot(flight('crashed', { elapsed_ms: 1200, multiplier: 100, final_multiplier: 100 })));
  assert.equal(action.getAttribute('aria-disabled'), 'true');
  assert.equal(message.textContent, 'Ставка проиграна');
  assert.equal(f.elements.get('rocket-final').textContent, '1,00×');
});

test('a player with a confirmed wager polls at launch without waiting for the normal interval', async () => {
  const f = fixture();
  f.feed.show();
  await f.reply(f.requests[0], snapshot(flight('countdown', { next_launch_ms: 125 })));
  assert.equal(f.tick(), 125);
  assert.equal(f.requests.at(-1).path, 'api/rocket/state');
  await f.reply(f.requests.at(-1), snapshot(flight('flying', { elapsed_ms: 25, multiplier: 100 })));
  f.feed.cashout();
  assert.equal(f.requests.at(-1).path, 'api/rocket/cashout');
});

test('a confirmed loss stays visible after a cashout response is lost', async () => {
  const f = viewFixture();
  f.view.show();
  await f.reply(f.requests[0], snapshot(flight('flying')));
  f.elements.get('rocket-form').dispatchEvent(new Event('submit', { cancelable: true }));
  const cashout = f.requests.at(-1);
  assert.equal(cashout.path, 'api/rocket/cashout');
  cashout.reject(new TypeError('network error'));
  await new Promise(setImmediate);
  await f.reply(f.requests.at(-1), snapshot(flight('crashed', { can_cashout: false, final_multiplier: 100 })));
  const message = f.elements.get('rocket-message').textContent;
  assert.match(message, /Ставка проиграна/);
  assert.match(message, /Ответ на запрос потерян/);
});

test('countdown and crash wait have different clock labels and never open betting from the clock alone', async () => {
  const f = viewFixture();
  const action = f.elements.get('rocket-action');
  const clock = f.elements.get('rocket-time');
  const label = f.elements.get('rocket-time-label');
  f.view.show();
  await f.reply(f.requests[0], snapshot(emptyRound('current', 'crashed', { elapsed_ms: 3000 })));
  f.frame();
  assert.equal(label.textContent, 'До следующего старта');
  assert.equal(clock.textContent, '12,0 с');
  assert.equal(action.getAttribute('aria-disabled'), 'true');
  await f.poll(snapshot(emptyRound('next', 'countdown', { next_launch_ms: 240 })));
  f.frame();
  assert.equal(label.textContent, 'До старта');
  assert.equal(clock.textContent, '0,3 с');
  assert.equal(action.getAttribute('aria-disabled'), 'false');
  f.advance(240);
  f.frame();
  assert.equal(clock.textContent, '0,0 с');
  assert.equal(action.getAttribute('aria-disabled'), 'true');
  f.elements.get('rocket-form').dispatchEvent(new Event('submit', { cancelable: true }));
  assert.deepEqual(f.starts(), []);
});

test('placing a bet binds it to the open round and discards a late state poll', async () => {
  const f = fixture();
  const open = emptyRound();
  f.feed.show();
  await f.reply(f.requests[0], snapshot(open));
  f.tick();
  const start = f.feed.launch('100', '1.75');
  assert.deepEqual(f.starts(), [{ bet: '100', auto: '1.75', expected_id: open.id }]);
  assert.equal(f.current().pending, 'start');
  const joined = flight('countdown', { id: open.id, elapsed_ms: -3000, auto: 175 });
  f.requests[2].resolve(snapshot(joined, 400));
  await start;
  await f.reply(f.requests[1], snapshot(open));
  assert.equal(f.current().snapshot.flight, joined);
  assert.equal(f.current().snapshot.balance, 400);
  await f.feed.launch('100', null);
  assert.equal(f.starts().length, 1);
  assert.ok(f.tick() >= 400);
  assert.equal(f.requests[3].path, 'api/rocket/state');
  assert.equal(f.requests[3].body, undefined);
});

test('cashout requires the server flag, uses the round ID, and is sent once while pending', async () => {
  const f = fixture();
  f.feed.show();
  const countdown = flight('countdown', { auto: 150, elapsed_ms: -40 });
  await f.reply(f.requests[0], snapshot(countdown));
  await f.feed.cashout();
  assert.equal(f.requests.length, 1);
  await f.poll(snapshot(flight('flying', { auto: 150, can_cashout: false })));
  await f.feed.cashout();
  assert.equal(f.requests.length, 2);
  const flying = flight('flying', { auto: 150, elapsed_ms: 1785, multiplier: 158 });
  await f.poll(snapshot(flying));
  assert.equal(f.requests.length, 3, 'an auto target never triggers a client cashout');
  const cashout = f.feed.cashout();
  await f.feed.cashout();
  assert.equal(f.requests.length, 4);
  assert.equal(f.requests[3].path, 'api/rocket/cashout');
  assert.equal(f.requests[3].body.id, countdown.id);
  assert.equal(f.current().pending, 'cashout');
  const paid = flight('flying', { payout: 158, cashout_multiplier: 158 });
  f.requests[3].resolve(snapshot(paid, 558));
  await cashout;
  assert.equal(f.current().snapshot.flight, paid);
  await f.feed.cashout();
  assert.equal(f.requests.length, 4);
});

test('a timed-out manual bet refreshes state and AutoBet skips that round even after opting in', async () => {
  const f = fixture();
  const open = emptyRound();
  f.feed.show();
  await f.reply(f.requests[0], snapshot(open));
  const start = f.feed.launch('100', null);
  assert.deepEqual(f.starts(), [{ bet: '100', auto: null, expected_id: open.id }]);
  f.requests[1].reject(Object.assign(new Error('timeout'), { name: 'TimeoutError' }));
  await start;
  assert.equal(f.current().needsSync, true);
  assert.equal(f.requests[2].path, 'api/rocket/state');
  await f.feed.launch('100', null);
  assert.equal(f.requests.length, 3);
  await f.reply(f.requests[2], snapshot(open));
  f.feed.startRepeat(1, 100, null);
  await f.reply(f.requests[3], snapshot(open));
  assert.equal(f.starts().length, 1);
  await f.poll(snapshot(emptyRound('next')));
  assert.deepEqual(f.starts().at(-1), { bet: '100', auto: null, expected_id: 'next' });
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

test('AutoBet places the selected number of wagers once per eligible round at the opted-in terms', async () => {
  const f = fixture();
  const open = emptyRound('first');
  const first = flight('countdown', { id: 'first', stake: 70, auto: 175 });
  const third = flight('countdown', { id: 'third', stake: 70, auto: 175 });
  f.feed.show();
  await f.reply(f.requests[0], snapshot(open));
  assert.equal(f.current().repeat, null);
  f.feed.startRepeat(2, 70, 175);
  assert.deepEqual(f.starts(), [], 'the cached countdown cannot trigger an automatic POST');
  await f.reply(f.requests.at(-1), snapshot(open));
  await f.reply(f.requests.at(-1), snapshot(first, 430));
  assert.equal(f.current().repeat.remaining, 1);
  await f.poll(snapshot(open, 430));
  assert.equal(f.starts().length, 1, 'a stale same-round snapshot cannot trigger another POST');
  const paid = flight('flying', { id: 'first', stake: 70, auto: 175, payout: 122, cashout_multiplier: 175, automatic: true });
  await f.poll(snapshot(paid, 552));
  await f.poll(snapshot({ ...paid, status: 'crashed' }, 552));
  await f.poll(snapshot(emptyRound('second', 'countdown', { can_bet: false }), 552));
  await f.poll(snapshot(flight('countdown', { id: 'second' }), 552));
  await f.poll(snapshot(emptyRound('second', 'flying'), 552));
  assert.equal(f.starts().length, 1, 'a closed or occupied countdown, flight and crash never submit');
  await f.poll(snapshot(emptyRound('third'), 552));
  await f.reply(f.requests.at(-1), snapshot(third, 482));
  await f.poll(snapshot(emptyRound('fourth'), 482));
  assert.deepEqual(f.starts(), [
    { bet: '70', auto: '1.75', expected_id: 'first' },
    { bet: '70', auto: '1.75', expected_id: 'third' },
  ]);
  assert.equal(f.current().repeat, null);
  assert.equal(f.current().repeatNotice, 'complete');
});

test('AutoBet preserves manual cashout for current and future wagers when no factor is selected', async () => {
  const f = fixture();
  const flying = flight('flying');
  f.feed.show();
  await f.reply(f.requests[0], snapshot(flying));
  f.feed.startRepeat(1);
  const cashout = f.feed.cashout();
  assert.equal(f.requests.at(-1).path, 'api/rocket/cashout');
  const paid = flight('flying', { payout: 110, cashout_multiplier: 110 });
  await f.reply(f.requests.at(-1), snapshot(paid, 510));
  await cashout;
  await f.feed.launch('100', null);
  assert.deepEqual(f.starts(), []);
  await f.poll(snapshot({ ...paid, status: 'crashed' }, 510));
  assert.deepEqual(f.starts(), []);
  await f.poll(snapshot(emptyRound('next'), 510));
  assert.deepEqual(f.starts(), [{ bet: '100', auto: null, expected_id: 'next' }]);
});

test('a boundary snapshot with zero time left cannot trigger a wager', async () => {
  const f = fixture();
  const expired = emptyRound('expired', 'countdown', { next_launch_ms: 0 });
  f.feed.show();
  await f.reply(f.requests[0], snapshot(expired));
  await f.feed.launch('100', null);
  f.feed.startRepeat(1, 100);
  await f.reply(f.requests.at(-1), snapshot(expired));
  assert.deepEqual(f.starts(), []);
  assert.equal(f.current().repeat.remaining, 1);
});

test('reloading a cashed-out flight shows the fixed payout and keeps launch locked until crash', async () => {
  const f = viewFixture();
  const action = f.elements.get('rocket-action');
  const payoutLabel = f.elements.get('rocket-payout-label');
  const payout = f.elements.get('rocket-payout');
  const liveMultiplier = f.elements.get('rocket-multiplier');
  const clock = f.elements.get('rocket-time');
  const payoutDetails = { auto: 175, payout: 175, cashout_multiplier: 175, automatic: true };
  const paid = flight('flying', { ...payoutDetails, multiplier: 210, elapsed_ms: 2900 });
  f.view.show();
  await f.reply(f.requests[0], snapshot(paid, 575));
  assert.equal(f.settings.hidden, true);
  assert.equal(action.attributes.get('aria-disabled'), 'true');
  assert.match(payoutLabel.textContent, /1,75×/);
  assert.equal(payout.textContent, '175 мон.');
  assert.equal(f.stop.hidden, true, 'reload never opts back into auto start');
  f.frame();
  f.frame(400);
  assert.equal(clock.textContent, '2,9 с');
  assert.equal(liveMultiplier.textContent, '2,10×');

  await f.poll(snapshot(flight('crashed', { ...payoutDetails, multiplier: 260, elapsed_ms: 3721 }), 575));
  f.frame();
  assert.equal(liveMultiplier.textContent, '2,60×');
  assert.equal(f.elements.get('rocket-multiplier-caption').textContent, 'Итог полёта');
  assert.equal(action.attributes.get('aria-disabled'), 'true');
  assert.match(payoutLabel.textContent, /1,75×/);
  assert.equal(payout.textContent, '175 мон.');
  assert.deepEqual(f.starts(), []);
});

test('a manual bet supersedes queued AutoBet while its state request is pending', async () => {
  const f = fixture();
  const open = emptyRound('manual');
  f.feed.show();
  await f.reply(f.requests[0], snapshot(open));
  f.feed.startRepeat(2, 100);
  const refresh = f.requests.at(-1);
  const launch = f.feed.launch('50', '2.5');
  const next = flight('countdown', { id: 'manual', stake: 50, auto: 250 });
  await f.reply(f.requests.at(-1), snapshot(next));
  await launch;
  await f.reply(refresh, snapshot(open));
  await f.poll(snapshot(emptyRound('next')));
  assert.deepEqual(f.starts(), [{ bet: '50', auto: '2.5', expected_id: 'manual' }]);
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
    f.feed.show();
    await f.reply(f.requests[0], snapshot(flying));
    f.feed.startRepeat(2);
    const pending = f.requests.at(-1);
    const open = snapshot(emptyRound('next'));
    await interrupt(f, () => { allowed = false; });
    await f.reply(pending, open);
    if (f.requests.at(-1) !== pending) await f.reply(f.requests.at(-1), open);
    allowed = true;
    await f.poll(open);
    assert.deepEqual(f.starts(), []);
    assert.equal(f.current().repeat, null);
  });
}

for (const [name, replacement, notice] of [
  ['the wager is refunded', snapshot(flight('refunded')), 'refunded'],
  ['the fixed stake exceeds the available balance at the next countdown', snapshot(emptyRound('next'), 99), 'stake'],
]) {
  test('auto start stops when ' + name, async () => {
    const f = fixture();
    f.feed.show();
    await f.reply(f.requests[0], snapshot(flight('flying')));
    f.feed.startRepeat(2);
    await f.reply(f.requests.at(-1), replacement);
    await f.poll(snapshot(emptyRound('later')));
    assert.deepEqual(f.starts(), []);
    assert.equal(f.current().repeat, null);
    assert.equal(f.current().repeatNotice, notice);
  });
}

for (const outcome of ['stop', 'network error']) {
  test('auto start clears the remaining rounds after ' + outcome + ' during a start request', async () => {
    const f = fixture();
    const next = flight('countdown', { id: 'next' });
    f.feed.show();
    await f.reply(f.requests[0], snapshot(flight('flying')));
    f.feed.startRepeat(2);
    await f.reply(f.requests.at(-1), snapshot(emptyRound('next')));
    const start = f.requests.at(-1);
    assert.equal(start.path, 'api/rocket/start');
    if (outcome === 'stop') {
      f.feed.stopRepeat();
      await f.reply(start, snapshot(next));
    } else {
      start.reject(new TypeError('network error'));
      await new Promise(setImmediate);
      assert.equal(f.requests.at(-1).path, 'api/rocket/state');
      await f.reply(f.requests.at(-1), snapshot(emptyRound('next')));
    }
    await f.poll(snapshot(emptyRound('next')));
    await f.poll(snapshot(emptyRound('later')));
    assert.equal(f.starts().length, 1);
    assert.equal(f.current().repeat, null);
  });
}

test('the Rocket form shows the additional stake limit and offers Stop while the wager settings are hidden', async () => {
  const f = viewFixture();
  const flying = flight('flying', { stake: 70, auto: 175 });
  f.view.show();
  await f.reply(f.requests[0], snapshot(flying));
  assert.equal(f.settings.hidden, true);
  assert.equal(f.setup.hidden, false);
  assert.equal(f.start.type, 'button');
  assert.equal(f.start.disabled, false);
  f.rounds.value = '2';
  f.rounds.dispatchEvent(new Event('change'));
  assert.match(f.status.textContent, /70 мон.*1,75×.*140 мон/);
  assert.equal(f.start.disabled, false);
  f.start.dispatchEvent(new Event('click'));
  assert.equal(f.stop.hidden, false);
  assert.equal(f.stop.type, 'button');
  f.stop.dispatchEvent(new Event('click'));
  assert.equal(f.stop.hidden, true);
  await f.reply(f.requests.at(-1), snapshot(emptyRound('next')));
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
    f.view.show();
    await f.reply(f.requests[0], snapshot(flying));
    f.rounds.value = '2';
    f.rounds.dispatchEvent(new Event('change'));
    f.start.dispatchEvent(new Event('click'));
    assert.equal(f.stop.hidden, false);
    const pending = f.requests.at(-1);
    const open = snapshot(emptyRound('next'));
    interrupt(f);
    assert.equal(f.stop.hidden, true);
    await f.reply(pending, open);
    resume(f);
    if (f.requests.at(-1) !== pending) await f.reply(f.requests.at(-1), open);
    if (f.timers.size) await f.poll(open);
    assert.deepEqual(f.starts(), []);
  });
}

test('animation frames move the ship and curve together without predicting through a sudden crash', async () => {
  const f = viewFixture();
  f.view.show();
  await f.reply(f.requests[0], snapshot(flight('flying', { elapsed_ms: 480, multiplier: 113 })));
  f.frame();
  const initial = f.drawing.ship;
  await f.poll(snapshot(flight('flying', { elapsed_ms: 980, multiplier: 128 })));
  f.frame();
  assert.deepEqual(f.drawing.ship, initial);
  const readout = f.elements.get('rocket-multiplier');
  assert.equal(readout.textContent, '1,13×');

  f.frame(125);
  const first = f.drawing.ship;
  assert.ok(first[0] > initial[0] && first[1] < initial[1]);
  assert.deepEqual(f.drawing.curve.at(-1), first);
  assert.equal(readout.textContent, '1,16×', 'rounds the confirmed interpolation down');
  f.frame(250);
  assert.ok(f.drawing.ship[0] > first[0] && f.drawing.ship[1] < first[1]);
  assert.equal(readout.textContent, '1,20×');
  f.frame(375);
  assert.equal(readout.textContent, '1,24×');
  f.frame(500);
  const confirmed = f.drawing.ship;
  f.frame(900); // Delayed crash response.
  assert.deepEqual(f.drawing.ship, confirmed);
  assert.equal(readout.textContent, '1,28×', 'a stalled poll cannot increase the coefficient');
  await f.poll(snapshot(flight('crashed', { elapsed_ms: 991, multiplier: 129 })));
  f.frame(500);
  assert.ok(f.drawing.marker[0] >= confirmed[0] && f.drawing.marker[1] <= confirmed[1]);
  assert.deepEqual(f.drawing.curve.at(-1), f.drawing.marker);
  assert.equal(readout.textContent, '1,29×');
});

test('the live readout displays every confirmed hundredth when frames are frequent enough', async () => {
  const f = viewFixture();
  const readout = f.elements.get('rocket-multiplier');
  f.view.show();
  await f.reply(f.requests[0], snapshot(emptyRound('current', 'flying', { elapsed_ms: 480, multiplier: 113 })));
  f.frame();
  await f.poll(snapshot(emptyRound('current', 'flying', { elapsed_ms: 980, multiplier: 128 })));
  const shown = new Set();
  for (let elapsed = 0; elapsed <= 528; elapsed += 16) {
    f.frame(elapsed);
    shown.add(readout.textContent);
  }
  assert.deepEqual([...shown], [
    '1,13×', '1,14×', '1,15×', '1,16×', '1,17×', '1,18×', '1,19×', '1,20×',
    '1,21×', '1,22×', '1,23×', '1,24×', '1,25×', '1,26×', '1,27×', '1,28×',
  ]);
});

test('the chart has the same grace and accelerating growth as Rust', () => {
  const rust = readFileSync(join(__dirname, '../rust/src/rocket.rs'), 'utf8');
  function value(name) {
    return Number(rust.match(new RegExp("const " + name + ": f64 = ([0-9_.]+);"))[1].replace(/_/g, ''));
  }
  const grace = value('GRACE_MS');
  const doubling = value('DOUBLE_MS');
  const power = value('GROWTH_POWER');
  const multiplierAt = vm.runInContext('rocketMultiplierAt', context);
  assert.equal(multiplierAt(grace - 1), 100);
  assert.equal(multiplierAt(grace), 100);
  assert.ok(multiplierAt(grace + 500) < 103);
  assert.equal(multiplierAt(grace + doubling), 200);
  assert.ok(Math.abs(multiplierAt(grace + doubling * 2 ** (1 / power)) - 400) < 1e-8);
  assert.equal(multiplierAt(100000), 100000);
});

test('the round summary keeps the personal cashout apart from the final multiplier', async () => {
  const f = viewFixture();
  const data = snapshot(flight('crashed', { payout: 125, cashout_multiplier: 125, multiplier: 327, final_multiplier: 327 }));
  data.bets = [{ id: '10', name: 'Талия', stake: 100, status: 'paid', multiplier: 125, payout: 125 }];
  f.view.show();
  await f.reply(f.requests[0], data);
  const bet = f.elements.get('rocket-bet-list').children[0];
  assert.equal(bet.children[0].children[0].textContent, 'Талия');
  assert.match(bet.children[0].children[1].textContent, /1,25×.*125/);
  assert.equal(f.elements.get('rocket-final').textContent, '3,27×');
});
test('reduced motion paints confirmed positions once and reveals the confirmed peak at crash', async () => {
  const f = viewFixture(true);
  const paid = { payout: 200, cashout_multiplier: 200 };
  f.view.show();
  await f.reply(f.requests[0], snapshot(flight('flying', { ...paid, elapsed_ms: 36200, multiplier: 22286 }), 600));
  f.frame();
  const initial = f.drawing.ship;
  assert.equal(f.hasFrame(), false);
  await f.poll(snapshot(flight('flying', { ...paid, elapsed_ms: 39000, multiplier: 89144 }), 600));
  f.frame();
  assert.ok(f.drawing.ship[0] > initial[0] && f.drawing.ship[1] < initial[1]);
  assert.equal(f.hasFrame(), false);
  assert.equal(f.elements.get('rocket-multiplier').textContent, '891,44×');
  assert.equal(f.elements.get('rocket-action').attributes.get('aria-disabled'), 'true');
  await f.poll(snapshot(flight('crashed', { ...paid, elapsed_ms: 39700, multiplier: 100000, final_multiplier: 100000 }), 600));
  f.frame();
  assert.equal(f.elements.get('rocket-multiplier').textContent, '1\u00a0000,00×');
  assert.equal(f.elements.get('rocket-payout').textContent, '200 мон.');
  assert.match(f.elements.get('rocket-payout-label').textContent, /2,00×/);
  assert.equal(f.elements.get('rocket-action').attributes.get('aria-disabled'), 'true');
  assert.equal(f.hasFrame(), false);
});

test('a confirmed crash reveals the peak immediately while the ship eases into the burst', async () => {
  const f = viewFixture();
  const paid = { payout: 200, cashout_multiplier: 200 };
  f.view.show();
  await f.reply(f.requests[0], snapshot(flight('flying', { ...paid, elapsed_ms: 36200, multiplier: 22286 }), 600));
  f.frame();
  const lastPaint = f.drawing.ship;
  await f.poll(snapshot(flight('flying', { ...paid, elapsed_ms: 39000, multiplier: 89144 }), 600));
  f.frame();
  assert.deepEqual(f.drawing.ship, lastPaint);
  f.frame(480);
  const beforeCrash = f.drawing.ship;

  await f.poll(snapshot(flight('crashed', { ...paid, elapsed_ms: 39700, multiplier: 100000, final_multiplier: 100000 }), 600));
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
