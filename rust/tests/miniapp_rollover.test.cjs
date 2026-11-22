const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const vm = require('node:vm');
const test = require('node:test');
const assert = require('node:assert/strict');

// Run the production polling function, with only its HTTP/DOM boundary replaced.
const app = readFileSync(join(__dirname, '../../miniapp/app.js'), 'utf8');
const polling = app.slice(app.indexOf('let refreshingPlayers = false;'), app.indexOf('// ---------- Справка ----------'));

test('an unanswered Discord command times out and removes its waiter', async () => {
  let timer;
  const context = vm.createContext({
    crypto: { randomUUID: () => 'test-nonce' }, pending: new Map(),
    discordWindow: { postMessage() {} }, discordOrigin: 'https://discord.com', OP: { FRAME: 1 },
    setTimeout: callback => { timer = callback; }, clearTimeout() {},
  });
  vm.runInContext(app.slice(app.indexOf('function discordCommand('), app.indexOf('function connectToDiscord(')), context);
  const result = vm.runInContext('discordCommand("AUTHORIZE", {})', context);
  assert.equal(typeof timer, 'function');
  timer();
  await assert.rejects(result, /Discord не ответил/);
  assert.equal(context.pending.size, 0);
});
function fixture(api) {
  const calls = [];
  const context = vm.createContext({
    session: 'test', document: { hidden: false }, lang: 'ru', busy: false,
    state: { day: '2026-11-15', finished: true }, typed: 'КОШКА', playersKey: 'old',
    api, renderPlayers: () => calls.push('players'), render: () => calls.push('board'),
    showGameStatus: () => calls.push('status'),
  });
  vm.runInContext(polling, context);
  return { context, calls, refresh: () => vm.runInContext('refreshPlayers()', context) };
}

test('an open finished game rolls over and clears yesterday input', async () => {
  const paths = [];
  const f = fixture(async path => {
    paths.push(path);
    return path === 'api/players'
      ? { day: '2026-11-16', lang: 'ru' }
      : { state: { day: '2026-11-16', finished: false } };
  });
  await f.refresh();
  assert.deepEqual(paths, ['api/players', 'api/state']);
  assert.equal(f.context.state.day, '2026-11-16');
  assert.equal(f.context.state.finished, false);
  assert.equal(f.context.typed, '');
  assert.equal(f.context.busy, false);
  assert.deepEqual(f.calls, ['players', 'board', 'status']);
});

test('a poll cannot overwrite a guess currently in flight', async () => {
  const f = fixture(async path => {
    assert.equal(path, 'api/players');
    return { day: '2026-11-16', lang: 'ru' };
  });
  f.context.busy = true;
  await f.refresh();
  assert.equal(f.context.state.day, '2026-11-15');
  assert.equal(f.context.typed, 'КОШКА');
  assert.deepEqual(f.calls, ['players']);
});

test('a response from a previous language is ignored', async () => {
  const f = fixture(async () => ({ day: '2026-11-16', lang: 'de' }));
  await f.refresh();
  assert.deepEqual(f.calls, []);
  assert.equal(f.context.state.day, '2026-11-15');
});

test('network failure releases guards so the next poll can recover', async () => {
  let fail = true;
  const f = fixture(async path => {
    if (path === 'api/players') return { day: '2026-11-16', lang: 'ru' };
    if (fail) throw new Error('offline');
    return { state: { day: '2026-11-16', finished: false } };
  });
  await f.refresh();
  assert.equal(f.context.busy, false);
  assert.equal(f.context.typed, 'КОШКА');
  fail = false;
  await f.refresh();
  assert.equal(f.context.state.day, '2026-11-16');
});

test('a guess crossing midnight carries its old date and clears stale input', async () => {
  const f = fixture(async (path, body) => {
    assert.equal(path, 'api/guess');
    assert.equal(body.day, '2026-11-15');
    return { state: { day: '2026-11-16', finished: false, rows: [] }, error: 'Новый день' };
  });
  f.context.state = { day: '2026-11-15', finished: false, rows: [[]] };
  f.context.LENGTH = 5;
  f.context.setStatus = text => f.calls.push(text);
  f.context.shake = () => {};
  const press = app.slice(app.indexOf('async function press(key)'), app.indexOf('window.addEventListener("keydown"'));
  vm.runInContext(press, f.context);
  await vm.runInContext('press("enter")', f.context);
  assert.equal(f.context.state.rows.length, 0);
  assert.equal(f.context.typed, '');
  assert.equal(f.context.busy, false);
  assert.deepEqual(f.calls, ['board', 'Новый день']);
});

test('switching games hides the previous view and stops refreshing word data', async () => {
  const panels = Object.fromEntries(
    ['app', 'rocket', 'poker', 'blackjack', 'mines', 'slots', 'roulette', 'coinflip']
      .map(id => [id, { hidden: true }]),
  );
  const calls = [];
  const mount = (game) => ({ request }) => ({
    show() { request('state'); },
    hide() { calls.push(`hide ${game}`); },
  });
  const document = { body: { dataset: { game: 'lobby' } }, documentElement: {}, getElementById: id => panels[id] };
  const context = vm.createContext({
    document, activeGame: null, session: 'test', lang: 'ru', reloadWords: false,
    casinoViews: new Map(),
    helpEl: {}, playersEl: {}, gameLobby: { hidden: false }, gameSwitch: { querySelectorAll: () => [] },
    rocket: { show() { calls.push('show rocket'); }, hide() { calls.push('hide rocket'); } },
    window: { CasinoGames: { roulette: mount('roulette'), coinflip: mount('coinflip') } },
    api: (path) => { calls.push(path); return Promise.resolve({}); },
  });
  vm.runInContext(polling, context);
  vm.runInContext(app.slice(app.indexOf('const casinoTitles'), app.indexOf('const casinoViews')), context);
  vm.runInContext(app.slice(app.indexOf('function openGame('), app.indexOf('\nfor (const button of gameLinks', app.indexOf('function openGame('))), context);
  await vm.runInContext('refreshPlayers()', context);
  vm.runInContext('openGame("rocket"); openGame("roulette"); openGame("coinflip"); openGame("rocket");', context);
  await vm.runInContext('refreshPlayers()', context);
  assert.deepEqual(calls, ['show rocket', 'hide rocket', 'api/games/roulette/state', 'hide roulette',
    'api/games/coinflip/state', 'hide coinflip', 'show rocket']);
  assert.equal(panels.roulette.hidden, true);
  assert.equal(panels.coinflip.hidden, true);
  assert.equal(panels.rocket.hidden, false);
  assert.equal(panels.app.hidden, true);
  assert.equal(context.gameLobby.hidden, true);
  vm.runInContext('openGame("lobby")', context);
  await vm.runInContext('refreshPlayers()', context);
  assert.equal(context.gameLobby.hidden, false);
  assert.equal(document.body.dataset.game, 'lobby');
  assert.equal(panels.rocket.hidden, true);
  assert.equal(calls.at(-1), 'hide rocket');
});
