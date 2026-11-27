const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const test = require("node:test");
const { runInNewContext } = require("node:vm");

function fixture(request, {
  image = async () => null, clock = { Date, setTimeout, clearTimeout }, legacyMarkup = false,
} = {}) {
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
  const choiceIds = new Set(["ai-model", "ai-model-hydrogen", "ai-thinking", "ai-thinking-minimal"]);
  const document = Object.assign(new EventTarget(), {
    getElementById(id) {
      if (legacyMarkup && choiceIds.has(id)) return null;
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

function change(node, value, type = "change") {
  node.value = value;
  node.dispatchEvent(new Event(type));
}

test("a failed upload keeps the draft and preview, and retries with the original ID at zero quota", async t => {
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
  t.after(() => view.disconnect("Disconnected"));
  view.show();
  await settle();
  const prompt = get("ai-prompt");
  const preview = get("ai-preview");
  const input = get("ai-file");
  change(get("ai-model"), "hydrogen");
  change(get("ai-thinking"), "high");
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
  assert.equal(get("ai-model-hydrogen").disabled, true);
  assert.equal(get("ai-send").disabled, false);
  form.dispatchEvent(new Event("submit", { cancelable: true }));
  await settle();
  assert.equal(calls.length, 2);
  assert.ok(calls[0].request_id);
  assert.deepEqual(calls[1], calls[0]);
  assert.equal(calls[1].model, "hydrogen");
  assert.equal(calls[1].thinking, "high");
  assert.deepEqual(calls[1].image, { mime: "image/png", data: "YnN0" });
  assert.equal(prompt.value, "");
  assert.equal(preview.hidden, true);
  assert.equal(get("ai-thread").children.length, 1);
  assert.equal(get("ai-model").value, "hydrogen");
  assert.equal(get("ai-thinking").value, "high");
});

test("an image-only send saves the turn and loads its private attachment", async t => {
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
  t.after(() => view.disconnect("Disconnected"));
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
  assert.equal(sent.model, "auto");
  assert.equal(sent.thinking, "medium");
  assert.deepEqual(images, ["uploaded"]);
  view.hide();
});

test("an open chat refreshes exhausted quota automatically at the server reset time", async t => {
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
  t.after(() => view.disconnect("Disconnected"));
  view.show();
  await settle();
  const prompt = get("ai-prompt");
  change(prompt, "Draft", "input");
  assert.equal(get("ai-model-hydrogen").disabled, true);
  assert.equal(get("ai-send").disabled, false);
  assert.match(get("ai-quota").textContent, /00:00 МСК/);
  assert.equal(timer.delay, 6000);
  currentTime += timer.delay;
  await timer.callback();
  assert.equal(get("ai-model-hydrogen").disabled, false);
  assert.equal(get("ai-send").disabled, false);
  assert.equal(prompt.value, "Draft");
  assert.match(get("ai-quota").textContent, /Hydrogen.*3 из 3/);
});

test("changing either choice starts a new request and minimal is exclusive to Flash-Lite", async t => {
  const calls = [];
  const { view, get } = fixture(async (path, body) => {
    if (path === "api/ai") return { chats: [], quota: { ...quota, remaining: calls.length ? 0 : 1 } };
    calls.push(body);
    throw new Error("Send failed");
  });
  t.after(() => view.disconnect("Disconnected"));
  view.show();
  await settle();
  const model = get("ai-model");
  const thinking = get("ai-thinking");
  const minimal = get("ai-thinking-minimal");
  const send = get("ai-send");
  const submit = async () => {
    get("ai-composer").dispatchEvent(new Event("submit", { cancelable: true }));
    await settle();
  };
  change(get("ai-prompt"), "Draft", "input");
  assert.equal(model.value, "auto");
  assert.equal(thinking.value, "medium");
  assert.equal(minimal.disabled, true);
  change(model, "hydrogen");
  assert.equal(minimal.disabled, true);
  await submit();
  assert.equal(send.disabled, false);
  change(thinking, "low");
  assert.equal(send.disabled, true);
  await submit();
  assert.equal(calls.length, 1);
  change(model, "flash_lite");
  assert.equal(minimal.disabled, false);
  assert.equal(send.disabled, false);
  change(thinking, "minimal");
  await submit();
  assert.equal(calls[1].model, "flash_lite");
  assert.equal(calls[1].thinking, "minimal");
  assert.notEqual(calls[1].request_id, calls[0].request_id);
  change(model, "auto");
  assert.equal(thinking.value, "medium");
  assert.equal(minimal.disabled, true);
  await submit();
  assert.equal(calls[2].model, "auto");
  assert.equal(calls[2].thinking, "medium");
  change(thinking, "low");
  await submit();
  assert.equal(calls[3].model, "auto");
  assert.equal(calls[3].thinking, "low");
  assert.notEqual(calls[3].request_id, calls[2].request_id);
});

test("cached markup sends the legacy payload at zero Hydrogen quota", async t => {
  let sent;
  const { view, get } = fixture(async (path, body) => {
    if (path === "api/ai") return { chats: [], quota: { ...quota, remaining: 0 } };
    sent = body;
    return { chat: { id: "created", title: "Prompt" }, turns: [] };
  }, { legacyMarkup: true });
  t.after(() => view.disconnect("Disconnected"));
  view.show();
  await settle();
  change(get("ai-prompt"), "Draft", "input");
  assert.equal(get("ai-send").disabled, false);
  get("ai-composer").dispatchEvent(new Event("submit", { cancelable: true }));
  await settle();
  assert.equal(sent.prompt, "Draft");
  assert.ok(sent.request_id);
  assert.equal("model" in sent, false);
  assert.equal("thinking" in sent, false);
  assert.equal(get("ai-status").textContent, "");
});
