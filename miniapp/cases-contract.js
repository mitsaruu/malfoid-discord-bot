"use strict";

function createCasesContract({ root, request, showPetArt, balanceVersion, updateAccount, canMutate, onSettled }) {
  const owned = root.querySelector("#contract-owned");
  const template = root.querySelector("#contract-pet-template");
  const stake = root.querySelector("#contract-stake");
  const valueRange = root.querySelector("#contract-value-range");
  const submit = root.querySelector("#contract-sign");
  const status = root.querySelector("#contract-status");
  const retry = root.querySelector("#contract-retry");
  const result = root.querySelector("#contract-result");
  const money = new Intl.NumberFormat("ru-RU");
  const signedMoney = new Intl.NumberFormat("ru-RU", { signDisplay: "exceptZero" });
  const selected = new Set();
  let pets = [];
  let sprite;
  let view;
  let inventoryRead = 0;
  let quoteRead;
  let quoteTimer;
  let quote;
  let ready = false;
  let busy = false;

  const selection = () => pets.filter((pet) => selected.has(pet.id));
  const stakes = () => selection().map(({ id, level }) => ({ id, level }));
  const enabled = () => ready && !busy && canMutate();
  const coins = (value) => `${money.format(BigInt(value))} монет`;
  const range = ([min, max]) => min === max ? coins(min) : `${money.format(BigInt(min))}–${coins(max)}`;

  function renderTerms() {
    const entries = selection();
    stake.textContent = entries.length ? coins(entries.reduce((sum, entry) => sum + BigInt(entry.sale_price), 0n)) : "—";
    valueRange.textContent = quote ? range(quote.range) : "—";
    submit.disabled = !enabled() || !quote;
  }

  function renderPets() {
    if (!pets.length) {
      const empty = document.createElement("p");
      empty.textContent = ready ? "На складе пока нет питомцев." : "Загружаем питомцев…";
      owned.replaceChildren(empty);
      return;
    }
    owned.replaceChildren(...pets.map((entry) => {
      const card = template.content.firstElementChild.cloneNode(true);
      card.dataset.pet = entry.id;
      card.disabled = !enabled();
      card.setAttribute("aria-pressed", String(selected.has(entry.id)));
      card.querySelector(".case-pet-name").textContent = entry.pet.name;
      card.querySelector(".upgrade-pet-value").textContent = `Ур. ${entry.level} · ${coins(entry.sale_price)}`;
      const art = entry.pet.rarity === "secret" && sprite ? `data:image/webp;base64,${sprite}` : undefined;
      showPetArt(card, entry.pet, art);
      card.addEventListener("click", () => {
        if (!enabled()) return;
        if (selected.has(entry.id)) selected.delete(entry.id);
        else selected.add(entry.id);
        result.hidden = true;
        renderPets();
        queueQuote();
        owned.querySelector(`[data-pet="${CSS.escape(entry.id)}"]`)?.focus({ preventScroll: true });
      });
      return card;
    }));
  }

  function clearQuote() {
    clearTimeout(quoteTimer);
    quoteRead?.abort();
    quoteRead = undefined;
    quote = undefined;
  }

  function queueQuote() {
    clearQuote();
    retry.hidden = true;
    renderTerms();
    if (!enabled() || selected.size < 3) {
      status.textContent = "";
      return;
    }
    status.textContent = "Рассчитываем диапазон…";
    const controller = new AbortController();
    quoteRead = controller;
    const order = { stakes: stakes() };
    quoteTimer = setTimeout(async () => {
      try {
        const terms = await request("contract/quote", order, controller.signal);
        if (controller.signal.aborted) return;
        quote = terms;
        status.textContent = "";
      } catch (error) {
        if (controller.signal.aborted) return;
        status.textContent = error.message;
        retry.hidden = false;
      }
      renderTerms();
    }, 150);
  }

  async function load(notice = "") {
    if (!view) return;
    const { signal } = view;
    const version = balanceVersion();
    const read = ++inventoryRead;
    ready = false;
    retry.hidden = true;
    clearQuote();
    status.textContent = "Загружаем питомцев…";
    renderPets();
    renderTerms();
    try {
      const state = await request(null, undefined, signal);
      if (signal.aborted || read !== inventoryRead) return;
      const ownedIds = new Set(state.pets.map((entry) => entry.id));
      for (const id of selected) if (!ownedIds.has(id)) selected.delete(id);
      pets = state.pets.slice().sort((a, b) => {
        const diff = BigInt(b.sale_price) - BigInt(a.sale_price);
        return diff > 0n ? 1 : diff < 0n ? -1 : 0;
      });
      sprite = state.secret_sprite;
      updateAccount(state, version);
      ready = true;
      renderPets();
      queueQuote();
      if (notice) status.textContent = notice;
    } catch (error) {
      if (signal.aborted || read !== inventoryRead) return;
      status.textContent = notice || error.message;
      retry.hidden = false;
    }
  }

  function reveal(response) {
    const difference = BigInt(response.price) - BigInt(response.stake);
    result.dataset.outcome = response.dearer ? "dearer" : "cheaper";
    root.querySelector("#contract-result-outcome").textContent = response.dearer ? "Дороже ставки" : "Дешевле ставки";
    root.querySelector("#contract-result-name").textContent = `${response.pet.name} · ур. ${response.level}`;
    root.querySelector("#contract-result-income").textContent = `Доход · ${response.income}`;
    root.querySelector("#contract-result-price").textContent = `${coins(response.price)} (${signedMoney.format(difference)})`;
    showPetArt(result, response.pet);
    result.hidden = false;
  }

  async function sign() {
    if (!enabled() || !quote) return;
    const origin = view;
    const order = { stakes: stakes(), expected: quote };
    busy = true;
    status.textContent = "Подписываем контракт…";
    renderPets();
    renderTerms();
    let notice = "";
    try {
      const response = await request("contract/sign", order);
      updateAccount(response);
      if (view === origin) {
        selected.clear();
        reveal(response);
      }
    } catch (error) {
      notice = error.status < 500 ? error.message : "Ответ не получен. Проверь склад перед повтором.";
    } finally {
      busy = false;
      onSettled();
      if (view) await load(view === origin ? notice : "");
    }
  }

  submit.addEventListener("click", sign);
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
      view = undefined;
      clearQuote();
      selected.clear();
      pets = [];
      ready = false;
      result.hidden = true;
    },
    reload() { if (view && !busy) load(); },
    isMutating: () => busy,
  };
}
