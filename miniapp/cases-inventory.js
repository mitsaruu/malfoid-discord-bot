"use strict";

function createCasesInventory({ root, request, showPetArt, balanceVersion, updateAccount, canMutate, onSettled }) {
  const grid = root.querySelector("#inventory-pets");
  const template = root.querySelector("#inventory-pet-template");
  const status = root.querySelector("#inventory-status");
  const retry = root.querySelector("#inventory-retry");
  const actions = root.querySelector("#inventory-actions");
  const selection = root.querySelector("#inventory-selection");
  const sellButton = root.querySelector("#inventory-sell");
  const cancelButton = root.querySelector("#inventory-cancel");
  const money = new Intl.NumberFormat("ru-RU");
  const selected = new Set();
  let view = null;
  let pets = [];
  let secretSprite = null;
  let selling = false;
  let ready = false;
  let mutating = false;
  let readVersion = 0;

  function syncControls() {
    const enabled = ready && !mutating && canMutate();
    root.toggleAttribute("data-selling", selling);
    actions.hidden = !pets.length;
    cancelButton.hidden = !selling;
    cancelButton.disabled = mutating;
    selection.hidden = !selling;
    sellButton.disabled = !enabled || (selling && !selected.size);
    sellButton.textContent = selling && selected.size ? `Продать (${selected.size})` : "Продать";
    if (selling) {
      const total = pets.reduce((sum, entry) => selected.has(entry.id) ? sum + BigInt(entry.sale_price) : sum, 0n);
      selection.textContent = selected.size ? `Выбрано ${selected.size} · ${money.format(total)} монет` : "Выбери питомцев для продажи";
    }

    for (const card of grid.children) {
      const picked = selling && selected.has(card.dataset.petId);
      const pick = card.querySelector(".inventory-pick");
      const upgrade = card.querySelector(".inventory-upgrade");
      pick.disabled = !selling || !enabled;
      if (selling) pick.setAttribute("aria-pressed", String(picked));
      else pick.removeAttribute("aria-pressed");
      card.toggleAttribute("data-selected", picked);
      card.querySelector(".inventory-sale-coin").hidden = !picked;
      upgrade.hidden = selling;
      upgrade.disabled = !enabled || upgrade.dataset.max === "true";
      card.querySelector(".inventory-sale-price").hidden = !selling;
    }
  }

  function renderPets() {
    const cards = pets.map((entry) => {
      const card = template.content.firstElementChild.cloneNode(true);
      card.dataset.petId = entry.id;
      card.dataset.rarity = entry.pet.rarity;
      card.querySelector(".case-pet-name").textContent = entry.pet.name;
      card.querySelector(".case-pet-rarity").textContent = entry.pet.rarity_name;
      card.querySelector(".inventory-income").textContent = entry.income;
      card.querySelector(".inventory-level").textContent = entry.level;
      card.querySelector(".inventory-sale-price > span:last-child").textContent = money.format(BigInt(entry.sale_price));
      showPetArt(card, entry.pet, entry.pet.rarity === "secret" && secretSprite
        ? `data:image/webp;base64,${secretSprite}` : undefined);

      const upgrade = card.querySelector(".inventory-upgrade");
      upgrade.dataset.max = String(!entry.upgrade_price);
      upgrade.textContent = entry.upgrade_price ? `Прокачать · ${entry.upgrade_price}` : "Макс. уровень";
      upgrade.addEventListener("click", () => mutate("upgrade", { id: entry.id, level: entry.level }));
      card.addEventListener("click", () => {
        if (!selling || !ready || mutating || !canMutate()) return;
        if (selected.has(entry.id)) selected.delete(entry.id);
        else selected.add(entry.id);
        syncControls();
      });
      return card;
    });
    grid.replaceChildren(...cards);
    syncControls();
  }

  async function load(notice = "") {
    if (!view) return;
    const signal = view.signal;
    const version = balanceVersion();
    const read = ++readVersion;
    ready = false;
    retry.hidden = true;
    status.textContent = notice || "Загружаем склад…";
    syncControls();
    try {
      const state = await request(null, undefined, signal);
      if (signal.aborted || read !== readVersion) return;
      if (!Array.isArray(state.pets) || typeof state.balance !== "string") {
        throw new Error("Склад не загрузился полностью. Попробуй обновить.");
      }
      pets = state.pets;
      secretSprite = state.secret_sprite;
      const ids = new Set(pets.map((entry) => entry.id));
      for (const id of selected) if (!ids.has(id)) selected.delete(id);
      if (!pets.length) selling = false;
      ready = true;
      updateAccount(state, version);
      renderPets();
      status.textContent = notice || (pets.length ? "" : "На складе пока нет питомцев.");
    } catch (error) {
      if (signal.aborted || read !== readVersion) return;
      status.textContent = notice ? `${notice} Склад не обновился.` : error.message;
      retry.hidden = false;
    }
  }

  async function mutate(action, fields) {
    if (!view || !ready || mutating || !canMutate()) return;
    const currentView = view;
    mutating = true;
    status.textContent = action === "sell" ? "Продаём питомцев…" : "Прокачиваем питомца…";
    syncControls();
    let notice;
    try {
      const result = await request(action, fields);
      updateAccount(result);
      if (action === "sell") {
        const sold = new Set(fields.ids);
        pets = pets.filter((entry) => !sold.has(entry.id));
        selling = false;
        selected.clear();
        renderPets();
        notice = `Продано ${fields.ids.length}. Начислено ${result.payout}.`;
        if (view === currentView) void window.GameAudio?.inventorySold(currentView.signal);
      } else {
        notice = `Питомец «${result.name}» достиг уровня ${result.level}.`;
      }
    } catch (error) {
      notice = error.status ? error.message : "Ответ не получен. Проверь изменения на складе.";
    } finally {
      mutating = false;
      onSettled();
      if (view) await load(notice);
    }
  }

  sellButton.addEventListener("click", () => {
    if (!ready || mutating || !canMutate()) return;
    if (selling) {
      if (selected.size) mutate("sell", { ids: [...selected] });
      return;
    }
    selling = true;
    status.textContent = "";
    syncControls();
    grid.querySelector(".inventory-pick")?.focus();
  });

  cancelButton.addEventListener("click", () => {
    selling = false;
    selected.clear();
    syncControls();
    sellButton.focus();
  });

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
      ready = false;
      selling = false;
      selected.clear();
      grid.replaceChildren();
      pets = [];
      secretSprite = null;
    },
    reload() { if (view && !mutating) load(); },
    isMutating: () => mutating,
  };
}
