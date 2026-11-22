const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const { runInNewContext } = require('node:vm');
const test = require('node:test');
const assert = require('node:assert/strict');

const source = () => readFileSync(join(__dirname, 'audio.js'), 'utf8');

function fixture(saved) {
  const listeners = new Map();
  const events = [];
  const targets = [];
  let state = saved;
  const makeNode = (id) => ({
    value: '', disabled: false,
    setAttribute(name, value) { this[name] = value; },
    addEventListener(name, callback) { listeners.set(`${id}:${name}`, callback); },
  });
  const toggle = makeNode('toggle');
  const volume = makeNode('volume');
  const param = () => ({ value: 0, setValueAtTime() {}, cancelScheduledValues() {},
    setTargetAtTime(...args) { targets.push(args); } });
  const node = () => ({ connect() {}, disconnect() {}, gain: param(), frequency: param() });
  class AudioContext {
    state = 'suspended';
    currentTime = 3;
    sampleRate = 8000;
    resume() { this.state = 'running'; return Promise.resolve(); }
    createGain() { return node(); }
    createBuffer(_, length) {
      const channel = new Float32Array(length);
      return { getChannelData: () => channel, duration: length / this.sampleRate };
    }
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
  return { audio: window.GameAudio, listeners, events, targets, document, toggle, volume, saved: () => JSON.parse(state) };
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

test('menu waits for audio unlock, plays the intro once and crossfades a single bed', async () => {
  const f = fixture();
  const event = { isTrusted: true, target: { closest: () => null } };
  f.audio.setLobby(true);
  f.audio.menuReady();
  assert.equal(f.events.length, 0);
  await f.listeners.get('click')(event);
  const [intro, bed] = f.events;
  assert.equal(intro.loop, false);
  assert.equal(bed.loop, true);
  assert.ok(bed.startAt > intro.startAt);
  assert.ok(bed.buffer.duration >= 10);
  const pad = bed.buffer.getChannelData(0);
  assert.ok(Math.abs(pad[0] - pad.at(-1)) < .015, 'the loop joins without a hard edge');
  f.audio.stopGame();
  f.audio.setLobby(false);
  assert.equal(f.targets.at(-1)[0], 0);
  assert.notEqual(bed.stopAt, 0);
  f.audio.setLobby(true);
  assert.ok(f.targets.at(-1)[0] > 0);
  assert.equal(f.events.length, 2);
});

test('keyboard focus and activation have one distinct cue each, and fast repeated sounds are limited', async () => {
  const f = fixture();
  const button = { disabled: false, getAttribute: () => null };
  const event = { isTrusted: true, target: { closest: () => button } };
  await f.listeners.get('click')({ ...event, target: { closest: () => null } });
  f.listeners.get('keydown')({ isTrusted: true, key: 'Tab' });
  f.listeners.get('focusin')(event);
  await f.listeners.get('click')(event);
  assert.equal(f.events.length, 2);
  assert.notEqual(f.events[0].buffer, f.events[1].buffer);
  for (let i = 0; i < 20; i++) f.audio.play('roulette.tick');
  assert.ok(f.events.length < 10);
});

test('flight loop begins on unlock, adjusts intensity, and fades at the end', async () => {
  const f = fixture();
  const stop = f.audio.startLoop('rocket.flight');
  assert.equal(f.events.length, 0);
  await f.listeners.get('click')({ isTrusted: true, target: { closest: () => null } });
  const flight = f.events[0];
  assert.ok(flight.loop);
  const before = f.targets.at(-1)[0];
  stop.setIntensity(1);
  assert.ok(f.targets.at(-1)[0] > before);
  stop();
  assert.equal(f.targets.at(-1)[0], 0);
  assert.ok(flight.stopAt > flight.startAt);
  f.document.hidden = true;
  f.listeners.get('visibilitychange')();
  assert.equal(flight.stopAt, 0);
});

test('safe and mine outcomes differ at the same volume and have soft edges', async () => {
  const f = fixture();
  await f.listeners.get('click')({ isTrusted: true, target: { closest: () => null } });
  f.audio.play('mines.safe');
  f.audio.play('mines.blast');
  const [safe, blast] = f.events.map(event => event.buffer.getChannelData(0));
  for (const clip of [safe, blast]) {
    assert.equal(clip[0], 0);
    assert.ok(Math.abs(clip.at(-1)) < .001);
    assert.ok(clip.some(sample => Math.abs(sample) > .02));
    assert.ok(clip.every(sample => Math.abs(sample) < .24));
  }
  const tail = safe.slice(-Math.floor(safe.length / 2));
  const safeCrossings = tail.reduce((count, sample, index) => count + (index > 0 && sample * tail[index - 1] < 0), 0);
  assert.ok(safeCrossings > 40, 'the wooden safe-cell cue retains a distinct tonal tail');
  assert.notDeepEqual(safe, blast);
});
