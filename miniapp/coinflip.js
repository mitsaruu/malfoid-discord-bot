"use strict";

(window.CasinoGames ||= {}).coinflip = function ({ root, request }) {
  const sides = { heads: "Орёл", tails: "Решка" };
  const numbers = new Intl.NumberFormat("ru-RU");
  const motion = window.matchMedia("(prefers-reduced-motion: reduce)");
  let visible = false;
  let view = 0;
  let snapshot = null;
  let side = "heads";
  let reading = false;
  let pending = false;
  let error = "";
  let animation = null;

  function element(tag, className = "", text = "") {
    const node = document.createElement(tag);
    node.className = className;
    node.textContent = text;
    return node;
  }

  const header = element("header", "casino-header");
  const wallet = element("p", "casino-balance");
  header.append(element("h1", "", "Орёл или решка"), wallet);

  const stage = element("div", "coinflip-stage");
  stage.setAttribute("aria-hidden", "true");
  const coin = element("div", "coinflip-coin");
  for (const [key, title] of Object.entries(sides)) {
    const face = element("div", `coinflip-face coinflip-${key}`);
    const mark = element("span", "coinflip-mark", key === "heads" ? "✦" : "1");
    face.append(mark, element("span", "coinflip-stamp", title));
    coin.append(face);
  }
  stage.append(coin);

  const form = element("form", "casino-panel coinflip-controls");
  const choices = element("div", "casino-modes casino-toolbar");
  choices.setAttribute("role", "group");
  choices.setAttribute("aria-label", "Сторона монеты");
  const options = Object.entries(sides).map(([key, title]) => {
    const button = element("button", "", title);
    button.type = "button";
    button.addEventListener("click", () => { side = key; render(); });
    choices.append(button);
    return [key, button];
  });
  const wager = element("div", "casino-toolbar");
  const label = element("label", "", "Ставка в монетах");
  const bet = element("input");
  bet.type = "text";
  bet.inputMode = "numeric";
  bet.autocomplete = "off";
  bet.maxLength = 20;
  bet.required = true;
  label.append(bet);
  const maximum = element("button", "", "Макс.");
  maximum.type = "button";
  maximum.addEventListener("click", () => {
    if (snapshot) bet.value = String(Math.min(snapshot.balance, snapshot.max_bet));
  });
  wager.append(label, maximum);
  const flip = element("button", "casino-button", "Подбросить");
  flip.type = "submit";
  const feedback = element("p", "casino-feedback");
  feedback.setAttribute("role", "status");
  const retry = element("button", "", "Обновить состояние");
  retry.type = "button";
  retry.addEventListener("click", sync);
  form.append(choices, wager, flip, feedback, retry);

  const layout = element("div", "coinflip-layout");
  layout.append(stage, form);
  root.append(header, layout);

  function render() {
    const result = snapshot?.flip;
    const blocked = pending || reading || !snapshot;
    const canBet = snapshot && snapshot.balance >= snapshot.min_bet;
    coin.dataset.face = result?.result ?? "heads";
    wallet.textContent = snapshot ? `Баланс ${numbers.format(snapshot.balance)} 🪙` : "Баланс —";
    for (const [key, button] of options) {
      button.setAttribute("aria-pressed", String(side === key));
      button.disabled = blocked;
    }
    flip.disabled = bet.disabled = maximum.disabled = blocked || !canBet;
    if (snapshot && !bet.value) bet.value = String(snapshot.min_bet);
    retry.hidden = !error || pending || reading;
    const outcome = result ? sides[result.result] + (result.payout
      ? ` · +${numbers.format(result.payout - result.stake)} 🪙`
      : ` · −${numbers.format(result.stake)} 🪙`) : "";
    const message = pending ? "Монета в воздухе…" : reading ? "Загружаем результат…" :
      [error, outcome, snapshot && !canBet ? "Недостаточно монет для ставки." : ""].filter(Boolean).join(" ") ||
      "Угадайте сторону, чтобы удвоить ставку.";
    if (feedback.textContent !== message) feedback.textContent = message;
  }

  async function sync() {
    if (!visible || pending || reading) return;
    const current = view;
    reading = true;
    error = "";
    render();
    try {
      const state = await request("state");
      if (view === current) snapshot = state;
    } catch (problem) {
      if (view === current) {
        snapshot = null;
        error = problem.message || "Не удалось загрузить монету.";
      }
    } finally {
      if (view === current) {
        reading = false;
        render();
      }
    }
  }

  async function toss(result) {
    let audible = !document.hidden;
    if (audible) window.GameAudio?.play("coin.launch");
    if (motion.matches || typeof coin.animate !== "function") return audible;
    const silenceWhenHidden = () => { if (document.hidden) audible = false; };
    const from = snapshot?.flip?.result === "tails" ? 180 : 0;
    const to = result === "tails" ? 1620 : 1440;
    const spin = coin.animate([
      { transform: `translateY(0) rotateY(${from}deg)` },
      { transform: `translateY(-42px) rotateY(${from + 620}deg)`, offset: .45, easing: "cubic-bezier(.3,.1,.4,1)" },
      { transform: `translateY(3px) rotateY(${to}deg)`, offset: .88 },
      { transform: `translateY(0) rotateY(${to}deg)` },
    ], { duration: 920, easing: "ease-out", fill: "forwards" });
    animation = spin;
    document.addEventListener("visibilitychange", silenceWhenHidden);
    try { await spin.finished; }
    catch { /* Переход на другую вкладку отменяет только анимацию. */ }
    finally {
      document.removeEventListener("visibilitychange", silenceWhenHidden);
      coin.dataset.face = result;
      spin.cancel();
      animation = null;
    }
    return audible;
  }

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    if (!visible || pending || reading || !snapshot) return;
    const current = view;
    pending = true;
    error = "";
    const expected = snapshot.flip?.id ?? null;
    render();
    let audibleLanding = false;
    try {
      const state = await request("flip", { bet: bet.value, side, expected_round: expected });
      if (visible && view === current && state.flip.id !== expected) audibleLanding = await toss(state.flip.result);
      snapshot = state;
    } catch (problem) {
      snapshot = null;
      if (visible) {
        try { snapshot = await request("state"); }
        catch { /* Следующая попытка начинается с чтения сохранённого результата. */ }
        error = snapshot && (snapshot.flip?.id ?? null) !== expected
          ? "Показан последний сохранённый результат."
          : problem.message || "Не удалось подтвердить ставку.";
      }
    } finally {
      pending = false;
      if (visible) {
        render();
        if (audibleLanding && view === current && !document.hidden) window.GameAudio?.play("coin.land");
      }
    }
  });
  motion.addEventListener("change", (event) => { if (event.matches) animation?.cancel(); });

  return {
    show() {
      if (visible) return;
      visible = true;
      render();
      void sync();
    },
    hide() {
      visible = false;
      view++;
      reading = false;
      animation?.cancel();
    },
  };
};
