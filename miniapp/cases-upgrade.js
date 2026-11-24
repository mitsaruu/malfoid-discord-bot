"use strict";

function createCasesUpgrade({ root, request, showPetArt, balanceVersion, updateAccount, canMutate, onSettled }) {
  const owned = root.querySelector("#upgrade-owned");
  const targets = root.querySelector("#upgrade-targets");
  const sourcePreview = root.querySelector("#upgrade-source-preview");
  const targetPreview = root.querySelector("#upgrade-target-preview");
  const template = root.querySelector("#upgrade-pet-template");
  const wheel = root.querySelector("#upgrade-wheel");
  const needle = wheel.querySelector(".upgrade-needle");
  const chanceLabel = root.querySelector("#upgrade-chance");
  const speed = root.querySelector("#upgrade-speed");
  const fastMode = root.querySelector("#upgrade-fast");
  const submit = root.querySelector("#upgrade-submit");
  const status = root.querySelector("#upgrade-status");
  const retry = root.querySelector("#upgrade-retry");
  const number = new Intl.NumberFormat("ru-RU");
  const percent = new Intl.NumberFormat("ru-RU", { maximumFractionDigits: 2 });
  const reducedMotion = matchMedia("(prefers-reduced-motion: reduce)");
  const stakes = new Set();
  let view = null;
  let ready = false;
  let busy = false;
  let animation;
  let pets = [];
  let choices = [];
  let target;
  let secretSprite;
  let lastChance;
  let read = 0;

  function selectedStakes() {
    return pets.filter((entry) => stakes.has(entry.id));
  }

  function stakeValue(entries) {
    return entries.reduce((sum, entry) => sum + BigInt(entry.sale_price), 0n);
  }

  function chanceFor(from, prize) {
    if (!from || !prize) return 0;
    const to = BigInt(prize.sale_price);
    return from < to ? Number((from * 10_000n) / to) : 0;
  }

  function byPriceDescending(first, second) {
    const left = BigInt(first.sale_price);
    const right = BigInt(second.sale_price);
    return left === right ? 0 : left > right ? -1 : 1;
  }

  function content(entry, showIncome = false) {
    const card = template.content.firstElementChild.cloneNode(true);
    card.querySelector(".case-pet-name").textContent = entry.pet.name;
    card.querySelector(".upgrade-pet-value").textContent =
      `Ур. ${entry.level ?? 1} · ${number.format(BigInt(entry.sale_price))} монет`;
    const income = card.querySelector(".upgrade-pet-income");
    const rate = entry.income_rate ?? entry.income;
    if (showIncome && rate) {
      income.textContent = `Доход · ${rate}`;
      income.hidden = false;
    }
    const sprite = entry.pet.rarity === "secret" && secretSprite ? `data:image/webp;base64,${secretSprite}` : undefined;
    showPetArt(card, entry.pet, sprite);
    return card;
  }

  function showStakes(entries, total) {
    if (entries.length < 2) {
      sourcePreview.replaceChildren(entries.length ? content(entries[0], true) : "Выбери питомцев для ставки");
      return;
    }
    const summary = document.createElement("div");
    summary.className = "upgrade-stake-summary";
    const count = document.createElement("strong");
    count.textContent = `Выбрано: ${entries.length}`;
    const amount = document.createElement("span");
    amount.className = "upgrade-pet-value";
    amount.textContent = `${number.format(total)} монет`;
    summary.append(count, amount);
    sourcePreview.replaceChildren(summary);
  }

  function setChance(chance) {
    wheel.style.setProperty("--upgrade-chance", `${chance / 100}%`);
    chanceLabel.textContent = chance ? `${percent.format(chance / 100)}%` : "—";
  }

  function renderOptions(container, entries, isSelected, key, label, pick, enabled) {
    if (!entries.length) {
      const empty = document.createElement("p");
      empty.textContent = label;
      container.replaceChildren(empty);
      return;
    }
    container.replaceChildren(...entries.map((entry) => {
      const button = document.createElement("button");
      button.type = "button";
      button.dataset.choice = key(entry);
      button.setAttribute("aria-pressed", String(isSelected(entry)));
      button.disabled = !enabled;
      button.append(content(entry));
      button.addEventListener("click", () => {
        pick(entry);
        lastChance = undefined;
        needle.style.transform = "";
        status.textContent = "";
        delete status.dataset.result;
        render();
        container.querySelector(`[data-choice="${CSS.escape(key(entry))}"]`)?.focus({ preventScroll: true });
      });
      return button;
    }));
  }

  function render() {
    const enabled = ready && !busy && canMutate();
    const selected = selectedStakes();
    const total = stakeValue(selected);
    const eligible = selected.length ? choices.filter((entry) => chanceFor(total, entry) > 0) : choices;
    const chance = chanceFor(total, target);
    setChance(chance || lastChance || 0);
    showStakes(selected, total);
    targetPreview.replaceChildren(target ? content(target, selected.length < 2) : "Выбери питомца, которого хочешь получить");
    submit.disabled = !enabled || !chance;
    speed.disabled = busy;
    renderOptions(owned, pets, (entry) => stakes.has(entry.id), (entry) => entry.id, "На складе пока нет питомцев.",
      (entry) => {
        if (stakes.has(entry.id)) stakes.delete(entry.id);
        else stakes.add(entry.id);
        if (stakes.size && !chanceFor(stakeValue(selectedStakes()), target)) target = undefined;
      }, enabled);
    renderOptions(targets, eligible, (entry) => target === entry, (entry) => entry.pet.key, "Нет питомцев дороже выбранной ставки.",
      (entry) => { target = entry; }, enabled);
  }

  async function load(notice = "") {
    if (!view) return;
    const { signal } = view;
    const version = balanceVersion();
    const requestId = ++read;
    ready = false;
    retry.hidden = true;
    status.textContent = notice || "Загружаем питомцев…";
    submit.disabled = true;
    try {
      const state = await request(null, undefined, signal);
      if (requestId !== read || signal.aborted) return;
      if (!Array.isArray(state.pets) || !Array.isArray(state.upgrade_targets) || typeof state.balance !== "string") {
        throw new Error("Не удалось загрузить список питомцев.");
      }
      pets = state.pets.slice().sort(byPriceDescending);
      choices = state.upgrade_targets.slice().sort(byPriceDescending);
      secretSprite = state.secret_sprite;
      const ids = new Set(pets.map((entry) => entry.id));
      for (const id of stakes) if (!ids.has(id)) stakes.delete(id);
      target = choices.find((entry) => entry.pet.key === target?.pet.key);
      if (stakes.size && !chanceFor(stakeValue(selectedStakes()), target)) target = undefined;
      updateAccount(state, version);
      ready = true;
      status.textContent = notice;
      render();
    } catch (error) {
      if (requestId !== read || signal.aborted) return;
      status.textContent = notice || error.message;
      retry.hidden = false;
      render();
    }
  }

  async function spin(roll, fast) {
    const end = (fast ? 360 : 1800) + (roll + 0.5) * 360 / 10_000;
    if (!reducedMotion.matches && typeof needle.animate === "function") {
      animation = needle.animate([
        { offset: 0, transform: "rotate(0deg)", easing: "cubic-bezier(.48,0,.77,.48)" },
        { offset: .29, transform: `rotate(${end * .20}deg)` },
        { offset: .66, transform: `rotate(${end * .78}deg)`, easing: "cubic-bezier(.18,.43,.55,1)" },
        { offset: 1, transform: `rotate(${end}deg)` },
      ], { duration: fast ? 1000 : 6000 });
      try { await animation.finished; } catch { return; }
    }
    needle.style.transform = `rotate(${end}deg)`;
  }

  async function upgrade() {
    const selected = selectedStakes();
    if (!view || !ready || busy || !canMutate() || !chanceFor(stakeValue(selected), target)) return;
    const origin = view;
    const fast = fastMode.checked;
    const order = { stakes: selected.map(({ id, level }) => ({ id, level })), target: target.pet.key };
    busy = true;
    status.textContent = "Крутим колесо…";
    render();
    let notice;
    try {
      const result = await request("transform", order);
      updateAccount(result);
      notice = result.won ? `Успех! Получен питомец «${result.pet.name}».` : "Апгрейд не удался.";
      if (view === origin) {
        setChance(result.chance);
        await spin(result.roll, fast);
      }
      if (view === origin) {
        stakes.clear();
        lastChance = result.chance;
        status.dataset.result = result.won ? "won" : "lost";
      }
    } catch (error) {
      notice = error.status < 500 ? error.message : "Ответ не получен. Проверь список питомцев.";
    } finally {
      busy = false;
      onSettled();
      if (view) await load(view === origin ? notice : "");
    }
  }

  submit.addEventListener("click", upgrade);
  retry.addEventListener("click", () => load());
  return {
    show() {
      if (view) return;
      view = new AbortController();
      load();
    },
    hide() {
      if (!view) return;
      view.abort();
      view = null;
      animation?.cancel();
      ready = false;
      stakes.clear();
      target = undefined;
      lastChance = undefined;
      needle.style.transform = "";
      delete status.dataset.result;
    },
    reload() { if (view && !busy) load(); },
    isMutating: () => busy,
  };
}
