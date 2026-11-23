"use strict";

function createDurakSolo({ root, request, profile }) {
  const { node, button, create, rulebook, avatarFace, showAvatar } = DurakTable;
  const results = { won: "Вы выиграли.", lost: "Бот выиграл.", draw: "Ничья." };
  const title = node("h2", "Подкидной дурак", "durak-sr-only");
  title.id = "durak-title";
  root.setAttribute("aria-labelledby", title.id);

  const name = profile?.name?.trim() || "Вы";
  let portrait;
  let portraitUrl;
  const table = create({
    move,
    labels: { take: "Взять карты", pass: "Завершить атаку" },
    avatar(player, active) {
      if (player.id !== "you") return;
      portrait.dataset.active = String(active);
      return portrait;
    },
  });
  const feedback = node("p", "", "casino-feedback durak-feedback");
  feedback.setAttribute("role", "status");
  feedback.tabIndex = -1;
  const start = button("Начать игру", onStart, "casino-button");
  const retry = button("Обновить состояние", () => sync(), "casino-button");
  const cancel = button("Продолжить игру", cancelRestart);
  const restart = button("Да, начать заново", () => mutate("start", {}), "casino-button");
  const toolbar = node("div", "", "casino-toolbar durak-actions");
  toolbar.append(start, retry, cancel, restart);
  root.replaceChildren(title, table.element, feedback, toolbar);

  let visible = false;
  let view = 0;
  let reading = false;
  let pending = false;
  let ready = false;
  let confirming = false;
  let game = null;
  let notice = "";

  function canAct() {
    return visible && ready && !pending && !reading;
  }

  function tableState() {
    const playing = game.status === "playing";
    const action = game.phase === "defend" ? "defend" : "attack";
    const hints = {
      attack: "Ваш ход. Выберите карту для атаки.",
      defend: game.legal_cards.length ? "Отбейте карту или заберите карты со стола." : "Заберите карты со стола.",
      throw: game.legal_cards.length ? "Подкиньте карту или завершите атаку." : "Завершите атаку.",
    };
    return {
      game: {
        ...game, turn: playing ? "you" : "",
        legal_cards: { [action]: game.legal_cards },
        message: game.message || (results[game.status] ?? hints[game.phase]),
      },
      players: [
        { id: "you", name, cards: game.hand.length },
        { id: "bot", name: "Бот", cards: game.opponent_count },
      ],
      you: "you",
      playing,
      rules: { key: "throw", content: () => rulebook("throw") },
    };
  }

  function render(motion) {
    const focused = document.activeElement;
    const blocked = !canAct();
    table.element.hidden = !game;
    table.setBlocked(blocked || confirming);
    if (motion && game) table.update(tableState(), motion);
    if (motion && !game) table.reset();

    start.hidden = confirming || !ready;
    start.textContent = !game ? "Начать игру" : game.status === "playing" ? "Начать заново" : "Сыграть ещё";
    retry.hidden = ready || !notice;
    cancel.hidden = restart.hidden = !confirming;
    for (const control of [start, cancel, restart]) control.setAttribute("aria-disabled", String(blocked));
    retry.setAttribute("aria-disabled", String(reading || pending));

    let message = notice;
    if (pending) message = "Ждём ответ от игры…";
    else if (reading || (!ready && !notice)) message = "Загружаем партию…";
    else if (confirming) message = "Текущая партия завершится. Начать заново?";
    else if (!game && !notice) message = "Начните партию с ботом.";
    if (feedback.textContent !== message) feedback.textContent = message;
    if ([start, retry, cancel, restart].includes(focused) && focused.hidden) {
      [cancel, retry, start, feedback].find(control => !control.hidden)?.focus();
    }
  }

  function accept(response) {
    const next = response?.game;
    if (next !== null && typeof next?.revision !== "string") throw new Error("Missing Durak state");
    game = next;
    ready = true;
  }

  async function sync(recovery) {
    if (!visible || reading || pending) return;
    const current = view;
    ready = false;
    reading = true;
    render();
    try {
      const response = await request("state", {});
      if (current !== view) return;
      accept(response);
      notice = recovery
        ? (game?.revision === recovery.revision
          ? recovery.message
          : "Показано последнее сохранённое состояние.")
        : "";
    } catch {
      if (current === view) notice = "Не удалось загрузить партию. Обновите состояние, чтобы продолжить.";
    } finally {
      if (current === view) {
        reading = false;
        render(ready ? {} : undefined);
      }
    }
  }

  async function mutate(endpoint, fields) {
    if (!canAct()) return;
    const current = view;
    const revision = game?.revision;
    pending = true;
    notice = "";
    render();
    let recovery;
    try {
      accept(await request(endpoint, fields));
    } catch {
      ready = false;
      recovery = { revision, message: endpoint === "start"
        ? "Не удалось начать партию."
        : "Не удалось подтвердить ход." };
    }
    pending = false;
    confirming = false;
    if (!visible) return;
    if (current !== view || recovery) {
      void sync(recovery);
      return;
    }
    render({ animate: true, deal: endpoint === "start" });
    if (!document.hidden) window.GameAudio?.play(endpoint === "start" ? "cards.deal" : "cards.place");
  }

  function move(action, card) {
    if (!canAct() || confirming || game?.status !== "playing" || !game.actions.includes(action)) return;
    const fields = { revision: game.revision, action };
    if (card !== undefined) fields.card = card;
    void mutate("move", fields);
  }

  function onStart() {
    if (!canAct() || confirming) return;
    if (game?.status === "playing") {
      confirming = true;
      render();
    } else {
      void mutate("start", {});
    }
  }

  function cancelRestart() {
    if (!canAct() || !confirming) return;
    confirming = false;
    render();
  }

  async function loadAvatar(current, face) {
    try {
      const blob = await profile.avatar();
      if (blob && current === view && visible) portraitUrl = showAvatar(face, blob);
    } catch {
      // Initials stay visible when the image is unavailable.
    }
  }

  document.addEventListener?.("visibilitychange", () => {
    if (document.hidden) table.cancelMotion();
  });
  render();
  return {
    show() {
      if (visible) return;
      visible = true;
      ready = false;
      notice = "";
      portrait = avatarFace({ name });
      if (profile?.avatar) void loadAvatar(view, portrait);
      render();
      void sync();
    },
    hide() {
      visible = false;
      view++;
      reading = false;
      confirming = false;
      table.cancelMotion();
      if (portraitUrl) URL.revokeObjectURL(portraitUrl);
      portraitUrl = undefined;
    },
  };
}

(window.CasinoGames ||= {}).durak = function ({ root, request, avatar, profile }) {
  const heading = document.createElement("h1");
  heading.textContent = "Дурак";
  const header = document.createElement("header");
  header.className = "casino-header";
  const modes = document.createElement("div");
  modes.className = "casino-toolbar casino-modes";
  modes.setAttribute("role", "group");
  modes.setAttribute("aria-label", "Режим игры");
  header.append(heading, modes);
  root.replaceChildren(header);

  let visible = false;
  const tabs = [
    ["Открытые столы", window.createDurakOnline],
    ["С ботом", createDurakSolo],
  ].filter(([, mount]) => typeof mount === "function").map(([label, mount]) => {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = label;
    const panel = document.createElement("section");
    panel.className = "durak-mode-panel";
    panel.hidden = true;
    modes.append(button);
    root.append(panel);
    const tab = { button, panel, mount, view: null };
    button.addEventListener("click", () => {
      if (visible && tab !== current) select(tab);
    });
    return tab;
  });
  let current = tabs[0];

  function select(tab) {
    if (tab !== current) current.view?.hide();
    current = tab;
    for (const { button, panel } of tabs) {
      const selected = panel === current.panel;
      button.setAttribute("aria-pressed", String(selected));
      panel.hidden = !selected;
    }
    current.view ??= current.mount({ root: current.panel, request, avatar, profile });
    current.view.show();
  }

  return {
    show() {
      if (visible) return;
      visible = true;
      select(current);
    },
    hide() {
      if (!visible) return;
      visible = false;
      current.view?.hide();
    },
  };
};
