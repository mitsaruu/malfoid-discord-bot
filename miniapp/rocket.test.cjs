const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const source = readFileSync(join(__dirname, 'rocket.js'), 'utf8');
const context = vm.createContext({});
vm.runInContext(source, context);
const { createRocketFeed, rocketTelemetry } = vm.runInContext('({ createRocketFeed, rocketTelemetry })', context);

function snapshot(flight = null, balance = 500) {
  return { balance, min_bet: 10, max_bet: 1000, flight };
}

function flight(status, overrides = {}) {
  return { id: '9223372036854775807', status, stake: 100, elapsed_ms: 0,
    multiplier: 100, auto: null, payout: null, automatic: false, ...overrides };
}

function fixture() {
  const requests = [];
  const timers = new Map();
  const changes = [];
  let clock = 0;
  let nextTimer = 0;
  const feed = createRocketFeed({
    request: (path, body) => new Promise((resolve, reject) => requests.push({ path, body, resolve, reject })),
    onChange: (view) => changes.push(view),
    now: () => clock,
    setTimer: (callback, delay) => {
      timers.set(++nextTimer, { callback, delay });
      return nextTimer;
    },
    clearTimer: (id) => timers.delete(id),
  });
  return {
    feed, requests, timers,
    current: () => changes.at(-1),
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

test('visual telemetry extrapolates briefly from the server time and freezes at server outcomes', () => {
  const flying = flight('flying', { elapsed_ms: 5800, multiplier: 195 });
  assert.equal(rocketTelemetry(flying, 200).multiplier, 200);
  assert.equal(rocketTelemetry(flying, 8000).elapsed, rocketTelemetry(flying, 650).elapsed);
  assert.equal(rocketTelemetry(flight('flying', { elapsed_ms: 19800, multiplier: 985 }), 650).multiplier, 1000);
  assert.equal(rocketTelemetry(flight('countdown', { elapsed_ms: -10 }), 500).elapsed, 0);
  assert.equal(rocketTelemetry(flight('crashed', { multiplier: 146, elapsed_ms: 3250 }), 650).multiplier, 146);
  assert.equal(rocketTelemetry(flight('paid', { multiplier: 185, elapsed_ms: 5600 }), 650).elapsed, 5600);
});
