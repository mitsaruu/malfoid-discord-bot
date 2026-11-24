"use strict";

function createCaseReel(track, pets, renderCard) {
  const weights = { common: 100, rare: 55, epic: 28, legendary: 12, mythic: 3, secret: 0.2 };
  const groups = Object.entries(weights).map(([rarity, weight]) => ({
    pets: pets.filter((pet) => pet.rarity === rarity),
    weight,
  })).filter((group) => group.pets.length);
  const total = groups.reduce((sum, group) => sum + group.weight, 0);
  const reducedMotion = matchMedia("(prefers-reduced-motion: reduce)").matches;
  const duration = 6300;
  const hold = 140;
  const speed = 0.008;
  const startPosition = 6;
  const started = performance.now();
  let previousFrame = started;
  let position = startPosition;
  let frame;
  let finishTimer;
  let fadeTimer;
  let landing = null;
  let stopped = false;

  function fill(through) {
    while (track.children.length <= through) {
      let roll = Math.random() * total;
      const group = groups.find((candidate) => (roll -= candidate.weight) < 0);
      const choices = group?.pets || pets;
      track.append(renderCard(choices[Math.floor(Math.random() * choices.length)]));
    }
  }

  function stop() {
    stopped = true;
    cancelAnimationFrame(frame);
    clearTimeout(finishTimer);
    clearTimeout(fadeTimer);
    landing?.resolve();
    landing = null;
  }

  function animate(now) {
    if (landing) {
      const progress = Math.min(1, Math.max(0, (now - landing.start) / landing.duration));
      position = landing.from + (landing.to - landing.from) * (1 - (1 - progress) ** 2);
    } else {
      position += Math.min(48, Math.max(0, now - previousFrame)) * speed;
    }
    previousFrame = now;
    fill(Math.ceil(position) + 7);
    track.style.setProperty("--case-reel-position", position);
    if (landing && now >= landing.start + landing.duration + hold) stop();
    else frame = requestAnimationFrame(animate);
  }

  track.replaceChildren();
  fill(startPosition + 7);
  track.style.setProperty("--case-reel-position", startPosition);
  if (!reducedMotion) frame = requestAnimationFrame(animate);

  return {
    land(pet, fadeOpening) {
      if (stopped) return Promise.resolve();
      const start = performance.now();
      const remaining = Math.max(900, started + duration - start - hold);
      const untilReveal = reducedMotion ? Math.max(0, started + duration - start) : remaining + hold;
      if (fadeOpening) fadeTimer = setTimeout(() => fadeOpening(hold), Math.max(0, untilReveal - hold));
      if (reducedMotion) return new Promise((resolve) => {
        landing = { resolve };
        finishTimer = setTimeout(stop, untilReveal);
      });
      const to = Math.ceil(position + remaining * speed / 2);
      fill(to + 7);
      track.children[to].replaceWith(renderCard(pet));
      return new Promise((resolve) => {
        landing = { from: position, to, start, duration: remaining, resolve };
      });
    },
    stop,
  };
}

function createCasesView({ root, name, storageKey, catalogue, inventoryRequest, openCase, history, avatar }) {
  const nameEl = root.querySelector("#cases-name");
  const initial = root.querySelector("#cases-initial");
  const avatarEl = root.querySelector("#cases-avatar");
  const balance = root.querySelector("#cases-balance");
  const grid = root.querySelector("#cases-grid");
  const template = root.querySelector("#cases-card-template");
  const status = root.querySelector("#cases-status");
  const retry = root.querySelector("#cases-retry");
  const cataloguePanel = root.querySelector("#cases-catalogue");
  const catalogueTitle = root.querySelector("#cases-title");
  const casesHome = root.querySelector("#cases-home");
  const warehouseTab = root.querySelector("#cases-warehouse-tab");
  const warehousePanel = root.querySelector("#cases-warehouse");
  const warehouseTitle = root.querySelector("#cases-warehouse-title");
  const upgradeTab = root.querySelector("#cases-upgrade");
  const upgradePanel = root.querySelector("#cases-upgrade-panel");
  const upgradeTitle = root.querySelector("#cases-upgrade-title");
  const contractTab = root.querySelector("#cases-contract");
  const developmentPanel = root.querySelector("#cases-development");
  const developmentTitle = root.querySelector("#cases-development-title");
  const casePanel = root.querySelector("#cases-case");
  const caseTitle = root.querySelector("#cases-case-title");
  const caseArt = root.querySelector(".case-stage-art");
  const petsGrid = root.querySelector("#cases-pets");
  const petTemplate = root.querySelector("#cases-pet-template");
  const openButton = root.querySelector("#cases-open");
  const openActions = root.querySelector("#cases-open-actions");
  const fastButton = openActions.querySelector("[data-fast-open]");
  const openLabel = root.querySelector("#cases-open-label");
  const openPrice = root.querySelector("#cases-open-price");
  const openUnit = root.querySelector(".case-open-unit");
  const reelPanel = root.querySelector("#cases-reel");
  const reelTrack = root.querySelector("#cases-reel-track");
  const reelTemplate = root.querySelector("#cases-reel-card-template");
  const openStatus = root.querySelector("#cases-open-status");
  const resultPanel = root.querySelector("#cases-result");
  const secretVideo = root.querySelector("#cases-secret-video");
  const reducedMotion = matchMedia("(prefers-reduced-motion: reduce)");
  const resultName = root.querySelector("#cases-result-name");
  const resultRarity = root.querySelector("#cases-result-rarity");
  const resultIncome = root.querySelector("#cases-result-income");
  const resultSalePrice = root.querySelector("#cases-result-sale-price");
  const resultActions = root.querySelector("#cases-result-actions");
  const againButton = root.querySelector("#cases-open-again");
  const historyList = root.querySelector("#cases-history-list");
  const historyStatus = root.querySelector("#cases-history-status");
  const openingTemplate = root.querySelector("#cases-opening-template");
  const plural = new Intl.PluralRules("ru");
  const coinForms = { one: "монета", few: "монеты", many: "монет", other: "монеты" };
  const illustratedPets = new Set([
    "valera", "gena", "tolik", "frog", "arkady", "tamara", "boris",
    "igor", "edik", "zhorik", "hermes", "vasily", "lyudmila", "lion", "danya_dog",
    "sergey", "foidka_romana", "fat_oleg",
  ]);
  let pending = null;
  let historyTimer = null;
  let historyRows = new Map();
  let casesById = new Map();
  let selectedCase = null;
  let pendingPurchase = null;
  let purchaseBusy = false;
  let purchaseResult = null;
  let purchaseMessage = null;
  let activeOpening = null;
  let stopCaseSound;
  let secretVideoLoad;
  let revealVersion = 0;
  let secretUnlocked = false;
  let secretSprite = null;
  let balanceVersion = 0;
  let inventoryFull = false;
  const fullMessage = "у вас полный склад, продайте лишних питомцев";

  const warehouse = createCasesInventory({
    root: warehousePanel,
    request: inventoryRequest,
    showPetArt,
    balanceVersion: () => balanceVersion,
    canMutate: () => !purchaseBusy && !upgrade.isMutating(),
    onSettled: () => { showPurchase(); upgrade.reload(); },
    updateAccount,
  });
  const upgrade = createCasesUpgrade({
    root: upgradePanel,
    request: inventoryRequest,
    showPetArt,
    balanceVersion: () => balanceVersion,
    canMutate: () => !purchaseBusy && !warehouse.isMutating(),
    onSettled: () => { showPurchase(); warehouse.reload(); },
    updateAccount,
  });

  function updateAccount(state, version) {
    if (!pending) return;
    if (version === undefined) balanceVersion += 1;
    else if (version !== balanceVersion) return;
    balance.textContent = state.balance;
    if (typeof state.inventory_full === "boolean") {
      inventoryFull = state.inventory_full;
      if (!inventoryFull && purchaseMessage?.text === fullMessage) purchaseMessage = null;
    }
  }

  try {
    const saved = storageKey && JSON.parse(sessionStorage.getItem(storageKey));
    if (typeof saved?.case === "string" && typeof saved.request_id === "string" && /^\d+$/.test(saved.expected_price)) {
      pendingPurchase = saved;
    }
  } catch {
    // Повтор запроса остаётся доступен в текущей вкладке.
  }

  function rememberPurchase(order) {
    pendingPurchase = order;
    if (!storageKey) return;
    try {
      if (order) sessionStorage.setItem(storageKey, JSON.stringify(order));
      else sessionStorage.removeItem(storageKey);
    } catch {
      // В некоторых клиентах Discord хранилище Activity недоступно.
    }
  }

  function firstLetter(nickname) {
    return [...nickname][0]?.toLocaleUpperCase("ru") || "?";
  }

  function showName(nickname) {
    nameEl.textContent = nickname;
    nameEl.title = nickname;
    initial.textContent = firstLetter(nickname);
  }

  function showCatalogue(cases) {
    casesById = new Map(cases.map((item) => [item.id, item]));
    const cards = cases.map((item) => {
      const card = template.content.firstElementChild.cloneNode(true);
      card.dataset.case = item.id;
      const button = card.querySelector("button");
      button.disabled = !Array.isArray(item.pets) || !item.pets.length || !item.cost;
      button.addEventListener("click", () => showCase(item, true));
      card.querySelector(".case-name").textContent = item.name;
      card.querySelector(".case-amount").textContent = item.price;
      return card;
    });
    grid.replaceChildren(...cards);
  }

  function showSection(panel, title, tab = casesHome) {
    if (panel !== warehousePanel) warehouse.hide();
    if (panel !== upgradePanel) upgrade.hide();
    if (panel !== casePanel) {
      selectedCase = null;
      stopOpening();
      resetResult();
    }
    for (const section of [cataloguePanel, casePanel, warehousePanel, upgradePanel, developmentPanel]) section.hidden = section !== panel;
    for (const button of [casesHome, warehouseTab, upgradeTab, contractTab]) {
      if (button === tab) button.setAttribute("aria-current", "page");
      else button.removeAttribute("aria-current");
    }
    root.setAttribute("aria-labelledby", title.id);
  }

  function stopSound() {
    stopCaseSound?.();
    stopCaseSound = undefined;
  }

  function playSound(name) {
    stopSound();
    stopCaseSound = window.GameAudio?.playCaseSound(name);
  }

  function prepareSecretVideo() {
    if (secretVideoLoad || reducedMotion.matches) return secretVideoLoad;
    secretVideoLoad = (async () => {
      const response = await fetch("static/case-secret-explosion.mp4");
      if (!response.ok) throw new Error("Secret reveal video unavailable");
      secretVideo.src = URL.createObjectURL(await response.blob());
    })().catch(() => { secretVideoLoad = undefined; });
    return secretVideoLoad;
  }

  function stopSecretVideo() {
    if (secretVideo.hidden) return;
    secretVideo.hidden = true;
    secretVideo.pause();
    secretVideo.currentTime = 0;
  }

  function stopCaseEffects() {
    revealVersion += 1;
    stopSound();
    stopSecretVideo();
  }

  function resetResult() {
    if (purchaseResult) showPetArt(resultPanel, { emoji: "" }, null);
    purchaseResult = null;
    stopCaseEffects();
  }

  async function showSecretReveal(withSound) {
    stopSound();
    const version = revealVersion;
    await prepareSecretVideo();
    if (version !== revealVersion) return;
    if (document.hidden) return;
    if (reducedMotion.matches || !secretVideo.src) {
      if (withSound) playSound("secret");
      return;
    }
    secretVideo.hidden = false;
    void secretVideo.play().then(
      () => { if (version === revealVersion && withSound && !secretVideo.hidden) playSound("secret"); },
      () => {
        if (version !== revealVersion || secretVideo.hidden) return;
        stopSecretVideo();
        if (withSound) playSound("secret");
      },
    );
  }

  function stopOpening() {
    const animation = activeOpening?.animation;
    activeOpening = null;
    if (animation) {
      animation.stop();
      reelTrack.replaceChildren();
    }
  }

  function showCatalogueScreen(focus = false) {
    const previous = selectedCase?.id;
    showSection(cataloguePanel, catalogueTitle);
    if (focus) {
      const card = previous && grid.querySelector(`[data-case="${CSS.escape(previous)}"] button`);
      (card || catalogueTitle).focus();
    }
  }

  function showDevelopment(name, tab) {
    developmentTitle.textContent = name;
    showSection(developmentPanel, developmentTitle, tab);
    developmentTitle.focus();
  }

  function showWarehouse() {
    showSection(warehousePanel, warehouseTitle, warehouseTab);
    warehouse.show();
    warehouseTitle.focus();
  }

  function showUpgrade() {
    showSection(upgradePanel, upgradeTitle, upgradeTab);
    upgrade.show();
    upgradeTitle.focus();
  }

  function showSecretSilhouette(container) {
    showPetArt(container, { emoji: "?" }, "static/secret-pet-silhouette.webp");
  }

  function showReelPet(container, pet) {
    if (pet.rarity !== "secret") showPetArt(container, pet);
    else if (secretUnlocked) showPetArt(container, pet, secretSprite ? `data:image/webp;base64,${secretSprite}` : null);
    else showSecretSilhouette(container);
  }

  function renderReelCard(pet) {
    const card = reelTemplate.content.firstElementChild.cloneNode(true);
    card.dataset.rarity = pet.rarity;
    showReelPet(card, pet);
    return card;
  }

  function renderContents(item) {
    const cards = item.pets.map((pet) => {
      const card = petTemplate.content.firstElementChild.cloneNode(true);
      card.dataset.rarity = pet.rarity;
      const concealed = pet.rarity === "secret" && !secretUnlocked;
      card.querySelector(".case-pet-name").textContent = concealed ? "?" : pet.name;
      card.querySelector(".case-pet-rarity").textContent = pet.rarity_name;
      showReelPet(card, pet);
      return card;
    });
    petsGrid.replaceChildren(...cards);
  }

  function showCase(item, focus = false) {
    if (selectedCase?.id !== item.id) {
      stopOpening();
      resetResult();
    }
    if (item.pets.some((pet) => pet.rarity === "secret")) prepareSecretVideo();
    selectedCase = item;
    casePanel.dataset.case = item.id;
    caseTitle.textContent = `${item.name} кейс`;
    openPrice.textContent = item.price;
    openUnit.textContent = coinForms[plural.select(Number(item.cost.slice(-2)))];
    renderContents(item);
    showSection(casePanel, caseTitle);
    showPurchase();
    if (focus) {
      caseTitle.focus({ preventScroll: true });
      caseTitle.scrollIntoView({ block: "nearest" });
    }
  }

  function showPurchase() {
    if (!selectedCase) return;
    const current = pendingPurchase?.case === selectedCase.id;
    const anotherCase = pendingPurchase && !current;
    const opening = activeOpening?.caseId === selectedCase.id;
    const rolling = opening && Boolean(activeOpening.animation);
    const result = !opening && purchaseResult?.caseId === selectedCase.id ? purchaseResult : null;
    caseArt.hidden = rolling || Boolean(result);
    openActions.hidden = opening || Boolean(result);
    reelPanel.hidden = !rolling;
    resultPanel.hidden = !result;
    resultActions.hidden = !result;
    const full = inventoryFull && !current;
    openButton.disabled = purchaseBusy || warehouse.isMutating() || upgrade.isMutating() || Boolean(anotherCase) || full;
    fastButton.disabled = openButton.disabled;
    openLabel.textContent = current ? (purchaseBusy ? "Открываем…" : "Повторить запрос") : "Открыть";
    let message = purchaseMessage?.caseId === selectedCase.id ? purchaseMessage.text : "";
    if (opening) {
      message = "Открываем кейс…";
    } else if (full) {
      message = fullMessage;
    } else if (anotherCase) {
      const earlier = casesById.get(pendingPurchase.case);
      message = earlier ? `Сначала завершите открытие кейса «${earlier.name}».` : "Сначала завершите предыдущее открытие.";
    } else if (current && !purchaseBusy && !message) {
      message = "Уточните результат предыдущего открытия.";
    }
    openStatus.textContent = message;
    openStatus.hidden = !message;

    if (result) {
      const { pet, sprite, income, sale_price: salePrice } = result;
      resultPanel.dataset.rarity = pet.rarity;
      resultPanel.toggleAttribute("data-fast", result.fast);
      resultName.textContent = pet.name;
      resultRarity.textContent = pet.rarity_name;
      resultIncome.textContent = income;
      resultSalePrice.textContent = salePrice;
      showPetArt(resultPanel, pet, sprite ? `data:image/webp;base64,${sprite}` : undefined);
    }
  }

  async function purchase(fast = false) {
    if (!pending || !selectedCase || purchaseBusy || warehouse.isMutating() || upgrade.isMutating() ||
        (pendingPurchase && pendingPurchase.case !== selectedCase.id)) return;
    const item = selectedCase;
    const isRetry = Boolean(pendingPurchase);
    if (inventoryFull && !isRetry) return;
    const order = pendingPurchase || { case: item.id, request_id: crypto.randomUUID(), expected_price: item.cost };
    rememberPurchase(order);
    purchaseBusy = true;
    resetResult();
    purchaseMessage = null;
    const animation = fast ? null : createCaseReel(reelTrack, item.pets, renderReelCard);
    const opening = { caseId: item.id, animation };
    activeOpening = opening;
    if (!fast) playSound("opening");
    showPurchase();
    try {
      const response = await openCase(order);
      if (!response.pet || typeof response.balance !== "string") throw new Error("Неполный ответ сервера");
      rememberPurchase(null);
      updateAccount(response);
      const wonSecret = response.pet.rarity === "secret";
      if (animation) await animation.land(response.pet, wonSecret ? (duration) => stopCaseSound?.(duration) : undefined);
      if (activeOpening === opening) purchaseResult = { ...response, caseId: item.id, fast };
      if (wonSecret) {
        secretUnlocked = true;
        secretSprite = response.sprite || secretSprite;
        if (selectedCase?.id === item.id) renderContents(selectedCase);
      }
    } catch (error) {
      if (activeOpening === opening) stopSound();
      if (error.status === 400 || error.status === 409 || (!isRetry && [401, 403].includes(error.status))) {
        rememberPurchase(null);
      }
      if (error.message === fullMessage) {
        inventoryFull = true;
        balanceVersion += 1;
      }
      purchaseMessage = {
        caseId: item.id,
        text: error.status < 500 ? error.message : "Ответ не получен. Повторите тот же запрос.",
      };
      if (error.status === 409 && pending) loadCatalogue(pending.signal);
    } finally {
      const visible = activeOpening === opening;
      if (visible) stopOpening();
      purchaseBusy = false;
      showPurchase();
      if (visible && purchaseResult?.pet.rarity === "secret") void showSecretReveal(!fast);
      warehouse.reload();
      upgrade.reload();
    }
  }

  async function showAvatar(image, signal, openingId) {
    try {
      const blob = await avatar(signal, openingId);
      if (!blob || signal.aborted) return;
      const url = URL.createObjectURL(blob);
      try {
        image.src = url;
        await image.decode();
        if (!signal.aborted) image.hidden = false;
      } finally {
        URL.revokeObjectURL(url);
      }
    } catch {
      // При ошибке загрузки остаётся первая буква ника.
    }
  }

  function showPetArt(container, pet, source = illustratedPets.has(pet.key) ? `static/pets/${pet.key}.webp` : null) {
    const emoji = container.querySelector("[data-pet-emoji]");
    const image = container.querySelector("[data-pet-image]");
    emoji.textContent = pet.emoji;
    emoji.hidden = false;
    image.hidden = true;
    image.onload = null;
    if (!source) {
      image.removeAttribute("src");
      return;
    }

    image.onload = () => {
      emoji.hidden = true;
      image.hidden = false;
    };
    image.src = source;
  }

  function renderHistory(openings, signal) {
    const displayedIds = [...historyRows.keys()];
    if (openings.length === displayedIds.length && openings.every((item, index) => item.id === displayedIds[index])) return;
    const nextRows = new Map();
    for (const opening of openings) {
      let row = historyRows.get(opening.id);
      if (!row) {
        row = openingTemplate.content.firstElementChild.cloneNode(true);
        row.dataset.rarity = opening.pet.rarity;
        const nameEl = row.querySelector(".case-opening-name");
        nameEl.textContent = opening.name;
        nameEl.title = opening.name;
        row.querySelector(".case-opening-initial").textContent = firstLetter(opening.name);
        const chest = row.querySelector(".case-opening-chest");
        const caseName = opening.case_name ? `Кейс «${opening.case_name}»` : "Кейс";
        chest.title = caseName;
        if (opening.case_key) chest.dataset.case = opening.case_key;
        const secret = opening.pet.rarity === "secret";
        const petName = secret ? "Секретный питомец" : opening.pet.name;
        row.querySelector(".case-opening-pet").setAttribute("aria-label", `${caseName} → ${petName}`);
        if (secret) showSecretSilhouette(row);
        else showPetArt(row, opening.pet);
        showAvatar(row.querySelector(".case-opening-avatar img"), signal, opening.id);
      }
      nextRows.set(opening.id, row);
    }
    historyList.replaceChildren(...nextRows.values());
    historyRows = nextRows;
  }

  async function refreshHistory(signal) {
    try {
      const { openings } = await history(signal);
      if (signal.aborted) return;
      renderHistory(openings, signal);
      historyStatus.textContent = "Пока нет открытий других игроков.";
      historyStatus.hidden = openings.length > 0;
    } catch {
      if (signal.aborted) return;
      historyStatus.textContent = "Не удалось загрузить историю.";
      historyStatus.hidden = false;
    }
    historyTimer = setTimeout(() => refreshHistory(signal), 10000);
  }

  function hide() {
    pending?.abort();
    pending = null;
    clearTimeout(historyTimer);
    historyRows.clear();
    historyList.replaceChildren();
    showCatalogueScreen();
  }

  async function loadCatalogue(signal) {
    const version = balanceVersion;
    retry.hidden = true;
    status.hidden = grid.hasChildNodes();
    status.textContent = "Загружаем каталог…";
    try {
      const data = await catalogue(signal);
      if (signal.aborted) return;
      if (data.secret_unlocked && (secretUnlocked || activeOpening?.caseId !== "secret")) {
        secretUnlocked = true;
        secretSprite = data.secret_sprite || secretSprite;
      }
      showCatalogue(data.cases);
      showName(data.name);
      updateAccount(data, version);
      const updated = selectedCase && casesById.get(selectedCase.id);
      if (updated?.pets && updated.cost) showCase(updated);
      status.hidden = true;
      if (avatarEl.hidden) await showAvatar(avatarEl, signal);
    } catch (error) {
      if (signal.aborted) return;
      status.textContent = error.message;
      status.hidden = false;
      retry.hidden = false;
    }
  }

  function show() {
    hide();
    window.GameAudio?.prepareCaseSounds();
    pending = new AbortController();
    const { signal } = pending;
    historyStatus.textContent = "Загружаем историю…";
    historyStatus.hidden = false;
    balance.textContent = "—";
    refreshHistory(signal);
    loadCatalogue(signal);
  }

  function disconnect(reason) {
    hide();
    balance.textContent = "—";
    avatarEl.hidden = true;
    showName("Игрок Discord");
    status.textContent = reason;
    status.hidden = false;
    historyStatus.hidden = true;
    retry.hidden = true;
  }

  showName(name);
  retry.addEventListener("click", () => { if (pending) loadCatalogue(pending.signal); });
  casesHome.addEventListener("click", (event) => {
    event.preventDefault();
    showCatalogueScreen(true);
  });
  warehouseTab.addEventListener("click", showWarehouse);
  upgradeTab.addEventListener("click", showUpgrade);
  contractTab.addEventListener("click", () => showDevelopment("Контракт", contractTab));
  root.querySelector("#cases-back").addEventListener("click", () => showCatalogueScreen(true));
  root.querySelector("#cases-result-back").addEventListener("click", () => showCatalogueScreen(true));
  openButton.addEventListener("click", () => purchase());
  againButton.addEventListener("click", () => {
    if (!selectedCase) return;
    resetResult();
    showCase(selectedCase, true);
  });
  fastButton.addEventListener("click", () => purchase(true));
  secretVideo.addEventListener("ended", stopSecretVideo);
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) stopCaseEffects();
  });
  return { show, hide, disconnect };
}
