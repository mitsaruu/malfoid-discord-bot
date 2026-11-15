const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const vm = require('node:vm');
const test = require('node:test');
const assert = require('node:assert/strict');

// Run the production polling function, with only its HTTP/DOM boundary replaced.
const app = readFileSync(join(__dirname, '../../miniapp/app.js'), 'utf8');
const polling = app.slice(app.indexOf('let refreshingPlayers = false;'), app.indexOf('// ---------- Справка ----------'));
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
