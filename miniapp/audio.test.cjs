const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const { runInNewContext } = require('node:vm');
const test = require('node:test');
const assert = require('node:assert/strict');

const source = () => readFileSync(join(__dirname, 'audio.js'), 'utf8');

function fixture(saved) {
  const listeners = new Map();
  const events = [];
  let state = saved;
  const makeNode = (id) => ({
    value: '', disabled: false,
    setAttribute(name, value) { this[name] = value; },
    addEventListener(name, callback) { listeners.set(`${id}:${name}`, callback); },
  });
  const toggle = makeNode('toggle');
  const volume = makeNode('volume');
  const param = () => ({ value: 0, setValueAtTime() {}, exponentialRampToValueAtTime() {}, setTargetAtTime() {} });
  const node = () => ({ connect() {}, disconnect() {}, gain: param(), frequency: param() });
  class AudioContext {
    state = 'suspended';
    currentTime = 3;
    sampleRate = 8000;
    resume() { this.state = 'running'; return Promise.resolve(); }
    createGain() { return node(); }
    createBiquadFilter() { return node(); }
    createBuffer() { return { getChannelData: () => new Float32Array(2) }; }
    createOscillator() { return this.source(); }
    createBufferSource() { return this.source(); }
    source() {
      const source = { ...node(), stopAt: undefined, start(when) { this.startAt = when; },
        stop(when = 0) { this.stopAt = when; }, };
      events.push(source);
      return source;
    }
  }
  const document = { hidden: false, getElementById: (id) => id === 'game-audio-toggle' ? toggle : volume,
    addEventListener: (name, callback) => listeners.set(name, callback) };
  const window = { AudioContext };
  runInNewContext(source(), {
    window, document, performance: { now: () => 100 },
    localStorage: { getItem: () => state, setItem: (_, value) => { state = value; } },
  });
  return { audio: window.GameAudio, listeners, events, document, toggle, volume, saved: () => JSON.parse(state) };
}

test('only a real click enables effects and delayed game sounds are cancelled on view switch', async () => {
  const f = fixture();
  const event = { isTrusted: true, target: { closest: () => null } };
  f.audio.play('mines.safe');
  f.listeners.get('click')({ ...event, isTrusted: false });
  f.audio.play('mines.safe');
  assert.equal(f.events.length, 0);
  await f.listeners.get('click')(event);
  f.audio.play('cards.deal', 120);
  f.audio.play('mines.safe');
  assert.ok(f.events.length >= 2);
  assert.equal(f.events[0].startAt, 3.12);
  f.audio.stopGame();
  assert.ok(f.events.every(({ stopAt }) => stopAt === 0));
});

test('mute and volume survive reopening and muting stops a reel loop', async () => {
  const f = fixture('{"enabled":true,"volume":55}');
  assert.equal(f.volume.value, '55');
  await f.listeners.get('click')({ isTrusted: true, target: { closest: () => null } });
  const stop = f.audio.startLoop('slots.spin');
  assert.equal(typeof stop, 'function');
  assert.ok(f.events.some(({ loop }) => loop));
  await f.listeners.get('toggle:click')();
  assert.ok(f.events.every(({ stopAt }) => stopAt === 0));
  assert.equal(f.toggle['aria-pressed'], 'false');
  assert.deepEqual(f.saved(), { enabled: false, volume: 55 });
  const before = f.events.length;
  f.audio.play('roulette.launch');
  assert.equal(f.events.length, before);
  stop();
});

test('touch and disabled controls stay silent on hover, and hiding stops active audio', async () => {
  const f = fixture();
  const button = { disabled: false, getAttribute: () => null };
  const event = { isTrusted: true, target: { closest: () => button } };
  await f.listeners.get('click')({ ...event, target: { closest: () => null } });
  f.listeners.get('pointerover')({ ...event, pointerType: 'touch' });
  button.disabled = true;
  f.listeners.get('pointerover')({ ...event, pointerType: 'mouse' });
  assert.equal(f.events.length, 0);
  button.disabled = false;
  f.listeners.get('pointerover')({ ...event, pointerType: 'mouse' });
  f.listeners.get('pointerover')({ ...event, pointerType: 'mouse' });
  assert.equal(f.events.length, 1);
  f.volume.value = '32';
  f.listeners.get('volume:input')();
  assert.deepEqual(f.saved(), { enabled: true, volume: 32 });
  f.audio.play('mines.safe');
  f.audio.play('mines.blast');
  assert.notEqual(f.events.at(-1).buffer, f.events.at(-2).buffer);
  f.document.hidden = true;
  f.listeners.get('visibilitychange')();
  assert.ok(f.events.every(({ stopAt }) => stopAt === 0));
  f.audio.play('roulette.tick');
  assert.equal(f.events.length, 3);
});
