const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const test = require("node:test");
const { runInNewContext } = require("node:vm");

function fixture(request, { image = async () => null, clock = { Date, setTimeout, clearTimeout } } = {}) {
  class Element extends EventTarget {
    hidden = false;
    value = "";
    dataset = {};
    children = [];
    style = { setProperty() {}, removeProperty() {} };
    setAttribute() {}
    removeAttribute() {}
    replaceChildren(...children) { this.children = children; }
    append(...children) { this.children.push(...children); }
    remove() {}
    focus() { document.activeElement = this; }
  }
  const nodes = new Map();
  const document = Object.assign(new EventTarget(), {
    getElementById(id) {
      if (!nodes.has(id)) nodes.set(id, new Element());
      return nodes.get(id);
    },
    createElement() { return new Element(); },
    body: new Element(),
    visibilityState: "visible",
  });
  const window = Object.assign(new EventTarget(), {
    matchMedia: () => Object.assign(new EventTarget(), { matches: false }),
  });
  const source = readFileSync(join(__dirname, "ai-chat.js"), "utf8");
  const createView = runInNewContext(source + "\ncreateAiChat", {
    document, window, AbortController, URL, crypto, ...clock,
    FileReader: class {
      readAsDataURL() {
        this.result = "data:image/png;base64,YnN0";
        this.onload();
      }
    },
  });
  const view = createView({ request, image, profile: null });
  return { view, get: id => document.getElementById(id) };
}

const settle = () => new Promise(resolve => setImmediate(resolve));
const quota = { limit: 3, remaining: 1, reset_at: Math.floor(Date.now() / 1000) + 60 };

test("a failed upload keeps the draft and preview, and retries with the original ID at zero quota", async () => {
  const calls = [];
  const { view, get } = fixture(async (path, body) => {
    if (path === "api/ai") {
      return { chats: [], quota: { ...quota, remaining: calls.length ? 0 : 1 } };
    }
    assert.equal(path, "api/ai/send");
    calls.push(JSON.parse(JSON.stringify(body)));
    if (calls.length === 1) throw Object.assign(new Error("Send failed"), { status: 429 });
    return {
      chat: { id: "created", title: "Prompt" },
      turns: [{ id: "turn", prompt: "Prompt", answer: "Answer", image: false }],
      quota: { ...quota, remaining: 0 },
    };
  });
  view.show();
  await settle();
  const prompt = get("ai-prompt");
  const preview = get("ai-preview");
  const input = get("ai-file");
  prompt.value = "Prompt";
  prompt.dispatchEvent(new Event("input"));
  input.files = [new File(["bst"], "image.png", { type: "image/png" })];
  input.dispatchEvent(new Event("change"));
  await settle();
  const form = get("ai-composer");
  form.dispatchEvent(new Event("submit", { cancelable: true }));
  await settle();
  assert.equal(prompt.value, "Prompt");
  assert.equal(preview.hidden, false);
  assert.equal(get("ai-send").disabled, false);
  form.dispatchEvent(new Event("submit", { cancelable: true }));
  await settle();
  assert.equal(calls.length, 2);
  assert.ok(calls[0].request_id);
  assert.deepEqual(calls[1], calls[0]);
  assert.deepEqual(calls[1].image, { mime: "image/png", data: "YnN0" });
  assert.equal(prompt.value, "");
  assert.equal(preview.hidden, true);
  assert.equal(get("ai-thread").children.length, 1);
  view.disconnect("Disconnected");
});

test("an image-only send saves the turn and loads its private attachment", async () => {
  const images = [];
  let sent;
  const { view, get } = fixture(async (path, body) => {
    if (path === "api/ai") return { chats: [], quota };
    sent = body;
    return {
      chat: { id: "created", title: "Image" },
      turns: [{ id: "uploaded", prompt: "", answer: "Answer", image: true }],
      quota: { ...quota, remaining: 0 },
    };
  }, { image: async id => { images.push(id); return new Blob(["bst"], { type: "image/png" }); } });
  view.show();
  await settle();
  const input = get("ai-file");
  input.files = [new File(["bst"], "image.heic", { type: "image/heic" })];
  input.dispatchEvent(new Event("change"));
  assert.equal(get("ai-preview").hidden, true);
  assert.match(get("ai-status").textContent, /PNG, JPEG и WebP/);
  input.files = [new File(["bst"], "image.png", { type: "image/png" })];
  input.dispatchEvent(new Event("change"));
  assert.equal(get("ai-send").disabled, false);
  get("ai-composer").dispatchEvent(new Event("submit", { cancelable: true }));
  await settle();
  assert.equal(sent.prompt, "");
  assert.equal(sent.image.mime, "image/png");
  assert.deepEqual(images, ["uploaded"]);
  view.hide();
  view.disconnect("Disconnected");
});

test("an open chat refreshes exhausted quota automatically at the server reset time", async () => {
  const reset = Date.UTC(2026, 10, 25, 21);
  let currentTime = reset - 5000;
  let timer;
  const { view, get } = fixture(async path => {
    assert.equal(path, "api/ai");
    const remaining = currentTime < reset ? 0 : 3;
    return { chats: [], quota: { limit: 3, remaining, reset_at: (reset + (remaining ? 86400000 : 0)) / 1000 } };
  }, {
    clock: {
      Date: { now: () => currentTime },
      setTimeout: (callback, delay) => { timer = { callback, delay }; return timer; },
      clearTimeout: () => { timer = null; },
    },
  });
  view.show();
  await settle();
  const prompt = get("ai-prompt");
  prompt.value = "Draft";
  prompt.dispatchEvent(new Event("input"));
  assert.equal(get("ai-send").disabled, true);
  assert.match(get("ai-quota").textContent, /00:00 МСК/);
  assert.equal(timer.delay, 6000);
  currentTime += timer.delay;
  await timer.callback();
  assert.equal(get("ai-send").disabled, false);
  assert.equal(prompt.value, "Draft");
  assert.equal(get("ai-quota").textContent, "Осталось 3 из 3");
  view.disconnect("Disconnected");
});
