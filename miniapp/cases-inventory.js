"use strict";

function createCasesInventory({ root, request, showPetArt, balanceVersion, updateAccount, canMutate, onSettled }) {
  const groups = [...root.querySelectorAll(".inventory-group")].map((element) => ({
    element, active: element.dataset.active === "true", grid: element.querySelector("ul"),
    count: element.querySelector(".inventory-group-count"),
  }));
  const count = root.querySelector("#inventory-count");
  const template = root.querySelector("#inventory-pet-template");
  const status = root.querySelector("#inventory-status");
  const retry = root.querySelector("#inventory-retry");
  const actions = root.querySelector("#inventory-actions");
  const selection = root.querySelector("#inventory-selection");
  const collectButton = root.querySelector("#inventory-collect");
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
  let moving = null;
  let capacity;
  let activeLimit;
  let nativeDrag = false;
  let scrollFrame = null;
  let lastScrollFrame;
  let scrollSpeed = 0;

  function stopDragScroll() {
    if (scrollFrame !== null) cancelAnimationFrame(scrollFrame);
    scrollFrame = null;
    lastScrollFrame = undefined;
    scrollSpeed = 0;
  }

  function scrollDuringDrag(time) {
    if (!view || !moving) {
      stopDragScroll();
      return;
    }
    const elapsed = lastScrollFrame === undefined ? 16 : Math.min(time - lastScrollFrame, 32);
    lastScrollFrame = time;
    const delta = Math.sign(scrollSpeed) * Math.ceil(Math.abs(scrollSpeed) * elapsed / 1000);
    const before = window.scrollY;
    window.scrollBy(0, delta);
    if (window.scrollY === before) stopDragScroll();
    else scrollFrame = requestAnimationFrame(scrollDuringDrag);
  }

  function followDragPointer(y) {
    const height = window.innerHeight;
    const edge = Math.min(88, height * .16);
    const top = Math.max(0, (edge - y) / edge);
    const bottom = Math.max(0, (y - (height - edge)) / edge);
    scrollSpeed = y >= 0 && y <= height ? (bottom - top) * 840 : 0;
    if (!scrollSpeed) stopDragScroll();
    else if (scrollFrame === null) scrollFrame = requestAnimationFrame(scrollDuringDrag);
  }

  document.addEventListener("dragover", (event) => {
    if (nativeDrag) followDragPointer(event.clientY);
  });

  function syncControls() {
    const enabled = ready && !mutating && canMutate();
    const source = pets.find((entry) => entry.id === moving);
    const activeCount = pets.filter((entry) => entry.active).length;
    count.textContent = capacity ? `${pets.length} / ${capacity}` : "";
    root.toggleAttribute("data-selling", selling);
    actions.hidden = !pets.length;
    collectButton.disabled = !enabled || selling || !pets.length;
    cancelButton.hidden = !selling;
    cancelButton.disabled = mutating;
    selection.hidden = !selling;
    sellButton.disabled = !enabled || (selling && !selected.size);
    sellButton.textContent = selling && selected.size ? `Продать (${selected.size})` : "Продать";
    if (selling) {
      const total = pets.reduce((sum, entry) => selected.has(entry.id) ? sum + BigInt(entry.sale_price) : sum, 0n);
      selection.textContent = selected.size ? `Выбрано ${selected.size} · ${money.format(total)} монет` : "Выбери питомцев для продажи";
    }

    for (const group of groups) {
      const target = Boolean(source && source.active !== group.active);
      group.count.textContent = group.active && activeLimit ? `${activeCount} / ${activeLimit}` : String(pets.length - activeCount);
      group.element.toggleAttribute("data-drop-target", target && enabled);
    }

    for (const card of root.querySelectorAll(".inventory-pet")) {
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
      card.draggable = enabled && !selling;
      card.toggleAttribute("data-moving", moving === card.dataset.petId);
    }
  }

  function moveTo(active, replace) {
    const source = pets.find((entry) => entry.id === moving);
    if (!source || source.active === active || selling) return;
    stopDragScroll();
    moving = null;
    mutate("move", { id: source.id, active, ...(replace ? { replace } : {}) });
  }

  for (const group of groups) {
    group.element.addEventListener("dragover", (event) => {
      if (!group.element.hasAttribute("data-drop-target")) return;
      event.preventDefault();
      event.dataTransfer.dropEffect = "move";
    });
    group.element.addEventListener("drop", (event) => {
      if (!group.element.hasAttribute("data-drop-target")) return;
      event.preventDefault();
      moveTo(group.active, event.target.closest(".inventory-pet")?.dataset.petId);
    });
  }

  function renderPets() {
    const cards = pets.map((entry) => {
      const card = template.content.firstElementChild.cloneNode(true);
      card.dataset.petId = entry.id;
      card.dataset.rarity = entry.pet.rarity;
      card.dataset.active = String(entry.active);
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
      const dragArea = card.querySelector(".inventory-drag-area");
      let gesture;
      let dragged = false;
      dragArea.addEventListener("pointerdown", (event) => {
        if (event.pointerType !== "touch" || !event.isPrimary || !card.draggable) return;
        gesture = { id: event.pointerId, x: event.clientX, y: event.clientY };
        dragged = false;
        dragArea.setPointerCapture(event.pointerId);
      });
      dragArea.addEventListener("pointermove", (event) => {
        if (gesture?.id !== event.pointerId) return;
        if (!dragged) {
          if (Math.hypot(event.clientX - gesture.x, event.clientY - gesture.y) < 10) return;
          dragged = true;
          moving = entry.id;
          syncControls();
        }
        followDragPointer(event.clientY);
      });
      function endTouch(event) {
        if (gesture?.id !== event.pointerId) return;
        gesture = undefined;
        if (!dragged) return;
        stopDragScroll();
        const drop = event.type === "pointerup" && document.elementFromPoint(event.clientX, event.clientY);
        const group = drop && groups.find(({ element }) => element === drop.closest(".inventory-group"));
        if (group) moveTo(group.active, drop.closest(".inventory-pet")?.dataset.petId);
        moving = null;
        syncControls();
      }
      dragArea.addEventListener("pointerup", endTouch);
      dragArea.addEventListener("pointercancel", endTouch);
      dragArea.addEventListener("lostpointercapture", endTouch);
      card.addEventListener("dragstart", (event) => {
        if (!card.draggable || event.target !== card) return;
        moving = entry.id;
        nativeDrag = true;
        event.dataTransfer.setData("text/plain", entry.id);
        event.dataTransfer.effectAllowed = "move";
        syncControls();
      });
      card.addEventListener("drag", (event) => {
        if (nativeDrag && (event.clientX || event.clientY)) followDragPointer(event.clientY);
      });
      card.addEventListener("dragend", () => {
        nativeDrag = false;
        stopDragScroll();
        moving = null;
        syncControls();
      });
      card.addEventListener("click", () => {
        if (!selling || !ready || mutating || !canMutate()) return;
        if (selected.has(entry.id)) selected.delete(entry.id);
        else selected.add(entry.id);
        syncControls();
      });
      return card;
    });
    for (const group of groups) group.grid.replaceChildren(...cards.filter((card) => card.dataset.active === String(group.active)));
    syncControls();
  }

  async function load(notice = "") {
    if (!view) return;
    stopDragScroll();
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
      capacity = state.capacity;
      activeLimit = state.active_limit;
      secretSprite = state.secret_sprite;
      const ids = new Set(pets.map((entry) => entry.id));
      for (const id of selected) if (!ids.has(id)) selected.delete(id);
      if (!ids.has(moving)) moving = null;
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
    status.textContent = {
      collect: "Собираем доход…", sell: "Продаём питомцев…", upgrade: "Прокачиваем питомца…", move: "Обновляем состав…",
    }[action];
    syncControls();
    let notice;
    try {
      const result = await request(action, fields);
      if (typeof result.balance === "string") updateAccount(result);
      if (action === "sell") {
        const sold = new Set(fields.ids);
        pets = pets.filter((entry) => !sold.has(entry.id));
        selling = false;
        selected.clear();
        renderPets();
        notice = `Продано ${fields.ids.length}. Начислено ${result.payout}.`;
        if (view === currentView) void window.GameAudio?.inventorySold(currentView.signal);
      } else if (action === "collect") {
        notice = `Собрано ${result.payout}.`;
      } else if (action === "move") {
        notice = "Состав обновлён.";
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

  collectButton.addEventListener("click", () => mutate("collect", {}));

  sellButton.addEventListener("click", () => {
    if (!ready || mutating || !canMutate()) return;
    if (selling) {
      if (selected.size) mutate("sell", { ids: [...selected] });
      return;
    }
    selling = true;
    moving = null;
    status.textContent = "";
    syncControls();
    root.querySelector(".inventory-pick")?.focus();
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
      nativeDrag = false;
      stopDragScroll();
      view.abort();
      view = null;
      ready = false;
      selling = false;
      moving = null;
      selected.clear();
      for (const group of groups) group.grid.replaceChildren();
      pets = [];
      secretSprite = null;
    },
    reload() { if (view && !mutating) load(); },
    isMutating: () => mutating,
  };
}
