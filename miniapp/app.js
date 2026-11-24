// Все игры используют одну Discord Activity и общую сессию.
// Связь с Discord — по протоколу Embedded App SDK (сообщения postMessage окну Discord),
// ходы и проверка слов — на сервере бота (/api/...), поэтому загаданное слово страница не знает.
"use strict";

const LENGTH = 5;
const ROWS = 6;
// Всё, что отличается у двух игр: клавиатура и тексты
const GAMES = {
  de: {
    name: "Wortjagd", what: "немецкое",
    keys: [
      ["q", "w", "e", "r", "t", "z", "u", "i", "o", "p", "ü"],
      ["a", "s", "d", "f", "g", "h", "j", "k", "l", "ö", "ä"],
      ["enter", "y", "x", "c", "v", "b", "n", "m", "back"],
    ],
  },
  ru: {
    name: "5 букв", what: "русское",
    keys: [
      ["й", "ц", "у", "к", "е", "н", "г", "ш", "щ", "з", "х", "ъ"],
      ["ф", "ы", "в", "а", "п", "р", "о", "л", "д", "ж", "э"],
      ["enter", "я", "ч", "с", "м", "и", "т", "ь", "б", "ю", "back"],
    ],
  },
};
let lang = null;            // какая игра открыта: de или ru
let LETTERS = new Set();    // буквы этой игры

const params = new URLSearchParams(location.search);
const boardEl = document.getElementById("board");
const keyboardEl = document.getElementById("keyboard");
const statusEl = document.getElementById("status");

let session = null;   // пропуск от сервера бота
let activityProfile = null;
let state = null;     // состояние партии с сервера
let typed = "";       // что игрок сейчас набирает
let busy = false;     // ждём ответа сервера
let activeGame = null;
let rocket = null;
let casesView = null;
const casesRoot = document.getElementById("cases");
let reloadWords = false;
const casinoTitles = {
  mines: "Минное поле", poker: "Техасский холдем", slots: "Слоты", blackjack: "Блэкджек",
  roulette: "Рулетка", coinflip: "Орёл или решка", durak: "Дурак",
};
const casinoViews = new Map();

const gameSwitch = document.getElementById("game-switch");
const gameLobby = document.getElementById("game-lobby");
const gameLinks = [...gameSwitch.querySelectorAll("button"), ...gameLobby.querySelectorAll("button[data-game]")];
const wordsRetry = document.getElementById("words-retry");

// ---------- Связь с Discord (минимальный Embedded App SDK) ----------
const OP = { HANDSHAKE: 0, FRAME: 1, CLOSE: 2, HELLO: 3 };
// Handshake compatibility with the Discord Embedded App SDK 2.5 protocol.
const EMBEDDED_SDK_VERSION = "2.5.0";
const MOBILE_SDK_VERSION_MINIMUM = 250;
const discordWindow = window.parent.opener || window.parent;
// Match Discord's production Activity origins, including its opaque native bridge.
const discordOrigins = new Set([
  "https://discord.com", "https://discordapp.com", "https://ptb.discord.com",
  "https://ptb.discordapp.com", "https://canary.discord.com", "https://canary.discordapp.com",
  "https://staging.discord.co", "https://pax.discord.com", location.origin, "null",
]);
const referrerOrigin = document.referrer ? new URL(document.referrer).origin : null;
// READY can arrive through the native bridge. Its origin is not the RPC destination.
const discordTarget = referrerOrigin !== "null" && discordOrigins.has(referrerOrigin) ? referrerOrigin : "*";
const pending = new Map();
let onReady = null;

window.addEventListener("message", (event) => {
  if (event.source !== discordWindow || !discordOrigins.has(event.origin) || !Array.isArray(event.data)) return;
  const [opcode, data] = event.data;
  if (opcode === OP.CLOSE) {
    const reason = "Discord закрыл соединение" + (data?.message ? ": " + data.message : ".");
    for (const waiter of pending.values()) {
      clearTimeout(waiter.timer);
      waiter.reject(new Error(reason));
    }
    pending.clear();
    session = null;
    window.GameAudio?.stopGame();
    window.GameAudio?.setLobby(false);
    if (activeGame === "rocket" && rocket) rocket.disconnect(reason);
    else if (activeGame === "cases" && casesView) casesView.disconnect(reason);
    else if (casinoViews.has(activeGame)) {
      casinoViews.get(activeGame).hide();
      const root = document.getElementById(activeGame);
      root.textContent = reason;
    }
    else if (activeGame === "words") setStatus(reason, "error");
    else gameLobby.querySelector("h1").textContent = reason;
    for (const button of gameLinks) button.disabled = true;
    return;
  }
  if (opcode !== OP.FRAME || !data) return;
  if (data.cmd === "DISPATCH") {
    if (data.evt === "READY") onReady?.();
    return;
  }
  const waiter = pending.get(data.nonce);
  if (!waiter || (data.evt !== "ERROR" && data.cmd !== waiter.cmd)) return;
  pending.delete(data.nonce);
  clearTimeout(waiter.timer);
  if (data.evt === "ERROR") waiter.reject(new Error((data.data && data.data.message) || "ошибка Discord"));
  else waiter.resolve(data.data);
});

function discordCommand(cmd, args) {
  const nonce = crypto.randomUUID();
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(nonce);
      reject(new Error("Discord не ответил на " + cmd));
    }, 15000);
    pending.set(nonce, { cmd, resolve, reject, timer });
    try {
      discordWindow.postMessage([OP.FRAME, { cmd, args, nonce }], discordTarget);
    } catch (error) {
      clearTimeout(timer);
      pending.delete(nonce);
      reject(error);
    }
  });
}

function connectToDiscord(clientId) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      onReady = null;
      reject(new Error("Discord не подтвердил запуск Activity. Закрой игру и снова вызови /games."));
    }, 15000);
    onReady = () => {
      clearTimeout(timer);
      onReady = null;
      resolve();
    };
    const handshake = {
      v: 1, encoding: "json", client_id: clientId, frame_id: params.get("frame_id"),
    };
    const mobileVersion = Number.parseInt(params.get("mobile_app_version"), 10);
    if (params.get("platform") === "desktop" || mobileVersion >= MOBILE_SDK_VERSION_MINIMUM) {
      handshake.sdk_version = EMBEDDED_SDK_VERSION;
    }
    discordWindow.postMessage([OP.HANDSHAKE, handshake], discordTarget);
  });
}

// ---------- Запросы к серверу бота ----------
async function api(path, body, { signal = AbortSignal.timeout(path === "api/token" ? 40000 : 15000) } = {}) {
  const options = { cache: "no-store", signal, headers: { "Content-Type": "application/json" } };
  if (session) options.headers.Authorization = "Bearer " + session;
  if (body !== undefined) {
    options.method = "POST";
    options.body = JSON.stringify(body);
  }
  const response = await fetch(path, options);
  const data = await response.json().catch(() => {
    if (signal?.aborted) throw signal.reason;
    return {};
  });
  if (!response.ok) {
    const error = new Error(data.error || "Сервер бота не ответил (" + response.status + ")");
    error.status = response.status;
    throw error;
  }
  return data;
}

async function loadGameAvatar(path, signal = AbortSignal.timeout(8000)) {
  if (!session) return null;
  const response = await fetch(path, {
    headers: { Authorization: `Bearer ${session}` }, signal,
  });
  return response.ok ? response.blob() : null;
}

const gameAvatar = user => loadGameAvatar(`api/game-avatar/${encodeURIComponent(user)}.png`);

// ---------- Экран загрузки ----------
function loading(text, percent) {
  document.getElementById("splash-text").textContent = text;
  document.getElementById("progress").style.width = percent + "%";
}

function loadingFailed(text) {
  const el = document.getElementById("splash-text");
  el.textContent = text;
  el.className = "splash-text error";
}

function hideSplash() {
  document.getElementById("progress").style.width = "100%";
  setTimeout(() => {
    document.getElementById("splash").classList.add("hide");
    if (session) window.GameAudio?.menuReady();
  }, 350);
}

// ---------- Отрисовка ----------
function setStatus(text, kind) {
  statusEl.textContent = text;
  statusEl.className = "status" + (kind ? " " + kind : "");
}

function buildBoard() {
  boardEl.innerHTML = "";
  for (let r = 0; r < ROWS; r++) {
    const row = document.createElement("div");
    row.className = "row";
    for (let c = 0; c < LENGTH; c++) {
      const tile = document.createElement("div");
      tile.className = "tile";
      row.appendChild(tile);
    }
    boardEl.appendChild(row);
  }
}

function setLanguage(newLang) {
  if (newLang === lang || !GAMES[newLang]) return;
  lang = newLang;
  const game = GAMES[lang];
  LETTERS = new Set(game.keys.flat().filter((key) => key.length === 1));
  if (activeGame === null || activeGame === "words") {
    document.title = game.name;
    document.documentElement.lang = lang;
  }
  document.getElementById("game-name").textContent = game.name;
  document.getElementById("help-what").textContent = game.what;
  for (const button of document.querySelectorAll(".lang")) {
    button.classList.toggle("active", button.dataset.lang === lang);
  }
  buildKeyboard();
}

function buildKeyboard() {
  keyboardEl.innerHTML = "";
  for (const keys of GAMES[lang].keys) {
    const row = document.createElement("div");
    row.className = "krow";
    for (const key of keys) {
      const button = document.createElement("button");
      button.className = "key" + (key.length > 1 ? " wide" : "");
      button.dataset.key = key;
      button.textContent = key === "enter" ? "Ввод" : key === "back" ? "⌫" : key;
      button.addEventListener("click", () => press(key));
      row.appendChild(button);
    }
    keyboardEl.appendChild(row);
  }
}

function render(animateRow) {
  const rows = boardEl.children;
  for (let r = 0; r < ROWS; r++) {
    const tiles = rows[r].children;
    const guess = state && state.rows[r];
    for (let c = 0; c < LENGTH; c++) {
      const tile = tiles[c];
      let letter = "", cls = "tile";
      if (guess) {
        letter = guess.word[c];
        cls += " " + guess.marks[c];
      } else if (state && r === state.rows.length) {
        letter = typed[c] || "";
        if (letter) cls += " filled";
      }
      tile.textContent = letter;
      tile.className = cls;
      tile.style.animationDelay = r === animateRow ? c * 0.12 + "s" : "0s";
      if (guess && r !== animateRow) tile.style.animation = "none";
      else tile.style.animation = "";
    }
  }
  for (const button of keyboardEl.querySelectorAll(".key")) {
    const mark = state && state.letters[button.dataset.key];
    button.className = "key" + (button.dataset.key.length > 1 ? " wide" : "") + (mark ? " " + mark : "");
  }
  if (state) {
    document.getElementById("wallet").hidden = false;
    document.getElementById("balance").textContent = state.balance.toLocaleString("ru-RU");
    document.getElementById("rewards-text").textContent =
      "Слово дня одно на весь сервер, сыграть можно раз в день. Награда за победу с 1-й…" + state.max_guesses +
      "-й попытки: " + state.rewards.join(" / ") + " монет.";
  }
}

function showGameStatus() {
  if (!state) return;
  if (state.won) {
    const tries = state.rows.length;
    setStatus("🎉 " + state.answer.toUpperCase() + " — угадано с " + tries + "-й попытки" +
      (state.reward ? "! +" + state.reward + " монет" : "!"), "win");
  } else if (state.finished) {
    setStatus("😿 Попытки кончились. Слово будет в итогах дня. Новое слово — завтра.", "error");
  } else {
    const left = state.max_guesses - state.rows.length;
    setStatus("Осталось попыток: " + left);
  }
}

function shake() {
  const row = boardEl.children[state.rows.length];
  if (!row) return;
  row.classList.remove("shake");
  void row.offsetWidth;
  row.classList.add("shake");
}

// ---------- Игроки слева сверху: кто сегодня играет и как подбирает слова ----------
const playersEl = document.getElementById("players");
let playersKey = "";  // чтобы не перерисовывать панель, если ничего не поменялось

function miniBoard(marks) {
  const grid = document.createElement("div");
  grid.className = "mini";
  for (let r = 0; r < ROWS; r++) {
    for (let c = 0; c < LENGTH; c++) {
      const cell = document.createElement("i");
      if (marks[r]) cell.className = marks[r][c];
      grid.appendChild(cell);
    }
  }
  return grid;
}

function renderPlayers(data) {
  const key = JSON.stringify(data.players);
  if (key === playersKey) return;
  playersKey = key;
  playersEl.innerHTML = "";
  for (const player of data.players) {
    const card = document.createElement("div");
    card.className = "player" + (player.me ? " me" : "") + (player.won ? " done" : player.finished ? " lost" : "");
    card.title = player.name + (player.won ? " — угадал(а) за " + player.marks.length :
      player.finished ? " — не угадал(а)" : " — играет");
    const avatar = document.createElement("img");
    avatar.className = "avatar";
    avatar.alt = player.name;
    const fallback = () => {  // нет аватарки — первая буква имени
      const letter = document.createElement("div");
      letter.className = "avatar";
      letter.textContent = (player.name[0] || "?").toUpperCase();
      avatar.replaceWith(letter);
    };
    fetch("api/avatar/" + data.guild + "/" + player.id + ".png", {
      headers: { Authorization: "Bearer " + session },
      signal: AbortSignal.timeout(8000),
    }).then(async response => {
      if (!response.ok) throw new Error("Аватар недоступен");
      const blob = await response.blob();
      if (!avatar.isConnected) return;
      const url = URL.createObjectURL(blob);
      avatar.onload = () => URL.revokeObjectURL(url);
      avatar.onerror = () => { URL.revokeObjectURL(url); fallback(); };
      avatar.src = url;
    }).catch(fallback);
    card.append(avatar, miniBoard(player.marks));
    playersEl.appendChild(card);
  }
}

let refreshingPlayers = false;
function outsideWords() {
  const game = document.body?.dataset.game;
  return game && game !== "words";
}
async function refreshPlayers() {
  if (!session || document.hidden || refreshingPlayers || outsideWords()) return;
  refreshingPlayers = true;
  try {
    const data = await api("api/players");
    if (outsideWords()) return;
    if (data.lang && data.lang !== lang) return;
    renderPlayers(data);
    // Открытая игра тоже переходит к новому слову после московской полуночи.
    if (!busy && state && data.day && data.day !== state.day) {
      busy = true;
      try {
        state = (await api("api/state")).state;
        typed = "";
        playersKey = "";
        render(-1);
        showGameStatus();
      } finally {
        busy = false;
      }
    }
  } catch (error) {
    // не страшно — попробуем в следующий раз
  } finally {
    refreshingPlayers = false;
  }
}

// ---------- Справка ----------
const helpEl = document.getElementById("help");
document.getElementById("help-button").addEventListener("click", () => { helpEl.hidden = false; });
document.getElementById("help-close").addEventListener("click", () => { helpEl.hidden = true; });
helpEl.addEventListener("click", (event) => { if (event.target === helpEl) helpEl.hidden = true; });

// ---------- Ввод ----------
async function press(key) {
  if (!state || busy || state.finished) return;
  if (key === "back") {
    typed = typed.slice(0, -1);
  } else if (key === "enter") {
    if (typed.length < LENGTH) {
      setStatus("Нужно " + LENGTH + " букв", "error");
      shake();
      return;
    }
    busy = true;
    try {
      const previousDay = state.day;
      const data = await api("api/guess", { word: typed, day: previousDay });
      const before = state.rows.length;
      state = data.state;
      if (data.error) {
        if (state.day !== previousDay) {
          typed = "";
          playersKey = "";
          render(-1);
        }
        setStatus(data.error, "error");
        shake();
      } else {
        typed = "";
        render(before);
        showGameStatus();
        refreshPlayers();
      }
    } catch (error) {
      setStatus(error.message, "error");
    } finally {
      busy = false;
    }
    return;
  } else if (LETTERS.has(key) && typed.length < LENGTH) {
    typed += key;
  }
  render(-1);
}

window.addEventListener("keydown", (event) => {
  if (activeGame !== "words" || event.target?.closest?.("#game-switch, #words-retry, #game-audio")) return;
  if (event.ctrlKey || event.metaKey || event.altKey) return;
  if (!helpEl.hidden) return;
  let key = event.key.toLowerCase();
  if (key === "ё") key = "е";  // в русской игре ё и е — одна буква
  if (key === "enter") press("enter");
  else if (key === "backspace") press("back");
  else if (LETTERS.has(key)) press(key);
  else return;
  event.preventDefault();
});

async function newGame(body) {
  if (busy || !session) return;
  busy = true;
  try {
    state = (await api("api/new", body)).state;
    if (state.lang !== lang) playersKey = "";
    setLanguage(state.lang);
    typed = "";
    render(-1);
    showGameStatus();
    refreshPlayers();
  } catch (error) {
    setStatus(error.message, "error");
  } finally {
    busy = false;
  }
}

for (const button of document.querySelectorAll(".lang")) {
  // другая игра — сразу её слово дня
  button.addEventListener("click", () => {
    if (button.dataset.lang !== lang) newGame({ lang: button.dataset.lang });
  });
}

function applyWords(next) {
  if (!next) throw new Error("Сервер не вернул слово дня.");
  if (next.day !== state?.day || next.lang !== state?.lang) {
    typed = "";
    playersKey = "";
  }
  state = next;
  setLanguage(state.lang);
  render(-1);
  showGameStatus();
}

async function loadWords() {
  if (!reloadWords || activeGame !== "words" || busy) return;
  reloadWords = false;
  busy = true;
  wordsRetry.hidden = true;
  if (!state) setStatus("Загружаем слово дня…");
  try {
    applyWords((await api("api/state")).state);
    refreshPlayers();
  } catch (error) {
    setStatus(error.message, "error");
    wordsRetry.hidden = false;
  } finally {
    busy = false;
    if (reloadWords) loadWords();
  }
}

function openGame(game) {
  if (game === activeGame) return;
  window.GameAudio?.stopGame();
  if (activeGame === "rocket") rocket?.hide();
  if (activeGame === "cases") casesView?.hide();
  casinoViews.get(activeGame)?.hide();
  activeGame = game;
  document.body.dataset.game = game;
  window.GameAudio?.setLobby(game === "lobby");
  const words = game === "words";
  gameLobby.hidden = game !== "lobby";
  document.getElementById("app").hidden = !words;
  playersEl.hidden = !words;
  document.getElementById("rocket").hidden = game !== "rocket";
  if (casesRoot) casesRoot.hidden = game !== "cases";
  for (const id of Object.keys(casinoTitles)) {
    const view = document.getElementById(id);
    if (view) view.hidden = id !== game;
  }
  helpEl.hidden = true;
  for (const button of gameSwitch.querySelectorAll("button[aria-pressed]")) {
    button.setAttribute("aria-pressed", String(button.dataset.game === game));
  }
  if (game === "lobby") {
    document.title = "Игры Malfoid";
    document.documentElement.lang = "ru";
  } else if (words) {
    document.title = GAMES[lang].name;
    document.documentElement.lang = lang;
    reloadWords = true;
    loadWords();
  } else if (game === "cases") {
    document.title = "Зверушник";
    document.documentElement.lang = "ru";
    if (casesView) casesView.show();
    else document.getElementById("cases-status").textContent = "Не удалось загрузить каталог. Откройте Activity заново.";
  } else if (game === "rocket") {
    document.title = "Ракетка";
    document.documentElement.lang = "ru";
    if (rocket) rocket.show();
    else {
      document.getElementById("rocket-phase").textContent = "Нет соединения";
      document.getElementById("rocket-message").textContent = "Не удалось загрузить игру. Откройте Activity заново.";
      document.getElementById("rocket-action").textContent = "Запуск недоступен";
    }
  } else {
    const root = document.getElementById(game);
    document.title = casinoTitles[game];
    document.documentElement.lang = "ru";
    if (!casinoViews.has(game)) {
      const mount = window.CasinoGames?.[game];
      if (typeof mount !== "function") {
        root.textContent = "Не удалось загрузить игру. Откройте Activity заново.";
        return;
      }
      const request = (action, fields = {}) => {
        if (!session) return Promise.reject(new Error("Соединение с Discord закрыто."));
        return api(`api/games/${game}/${action}`, fields);
      };
      const avatar = game === "durak"
        ? (room, user) => loadGameAvatar(`api/durak/avatar/${encodeURIComponent(room)}/${encodeURIComponent(user)}.png`)
        : gameAvatar;
      casinoViews.set(game, mount({ root, request, avatar, profile: game === "durak" ? activityProfile : null }));
    }
    casinoViews.get(game).show();
  }
}

for (const button of gameLinks) {
  button.addEventListener("click", () => {
    if (activeGame === button.dataset.game) return;
    openGame(button.dataset.game);
    const navButton = gameSwitch.querySelector('[aria-pressed="true"]');
    if (gameSwitch.scrollWidth > gameSwitch.clientWidth) {
      navButton.scrollIntoView({ block: "nearest", inline: "nearest" });
    }
    const heading = document.getElementById(navButton.getAttribute("aria-controls")).querySelector("h1");
    if (heading) {
      heading.tabIndex = -1;
      heading.focus();
    }
  });
}
wordsRetry.addEventListener("click", () => {
  reloadWords = true;
  loadWords();
});

// ---------- Запуск ----------
async function start() {
  buildBoard();
  setLanguage("ru");
  if (!params.get("frame_id")) {
    loadingFailed("Откройте мини-приложение внутри Discord через /games или меню приложений.");
    return;
  }
  try {
    loading("Загружаем игры…", 20);
    const config = await api("api/config");
    loading("Подключаемся к Discord…", 45);
    await connectToDiscord(config.client_id);
    loading("Входим через Discord…", 70);
    const { code } = await discordCommand("AUTHORIZE", {
      client_id: config.client_id, response_type: "code", state: crypto.randomUUID(), prompt: "none", scope: ["identify"],
    });
    const login = await api("api/token", {
      code, guild_id: params.get("guild_id"), channel_id: params.get("channel_id"),
    });
    session = login.session;
    if (login.user_id && login.name) {
      activityProfile = { name: login.name, avatar: () => gameAvatar(login.user_id) };
    }
    await discordCommand("AUTHENTICATE", { access_token: login.access_token });
    if (typeof createCasesView === "function" && casesRoot) {
      casesView = createCasesView({
        root: casesRoot,
        name: login.name,
        storageKey: login.user_id ? `malfoid:case-opening:${params.get("guild_id")}:${login.user_id}` : null,
        catalogue: (signal) => api("api/cases", undefined, { signal: AbortSignal.any([signal, AbortSignal.timeout(15000)]) }),
        inventoryRequest: (action, fields, signal) => api(
          action ? `api/cases/inventory/${action}` : "api/cases/inventory",
          fields,
          signal ? { signal: AbortSignal.any([signal, AbortSignal.timeout(15000)]) } : undefined,
        ),
        openCase: (order) => api("api/cases/open", order),
        history: (signal) => api("api/cases/history", undefined, { signal: AbortSignal.any([signal, AbortSignal.timeout(15000)]) }),
        avatar: (signal, openingId) => loadGameAvatar(
          openingId ? `api/cases/history/${encodeURIComponent(openingId)}.png` : "api/cases/avatar.png",
          AbortSignal.any([signal, AbortSignal.timeout(8000)]),
        ),
      });
    }
    if (typeof createRocketView === "function") {
      rocket = createRocketView((path, body) => api(path, body, {
        signal: AbortSignal.timeout(path === "api/rocket/start" ? 12000 : 5000),
      }), (user) => loadGameAvatar(`api/rocket/avatar/${encodeURIComponent(user)}.png`), gameAvatar);
    }
    gameSwitch.hidden = false;
    document.getElementById("game-audio").hidden = false;
    openGame(login.game === "words" ? "words" : "lobby");
    hideSplash();
    setInterval(() => {
      loadWords();
      refreshPlayers();
    }, 2000);
  } catch (error) {
    loadingFailed("Не получилось войти: " + error.message);
  }
}

start();
