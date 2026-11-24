"use strict";

function createCasesLeaderboard({ root, request, showPetArt, showSecretSilhouette }) {
  const sort = root.querySelector("#player-top-sort");
  const table = root.querySelector("#player-top-table");
  const status = root.querySelector("#player-top-status");
  const retry = root.querySelector("#player-top-retry");
  const rows = root.querySelector("#player-top-rows");
  const rowTemplate = root.querySelector("#player-top-row-template");
  const petTemplate = root.querySelector("#player-top-pet-template");
  const number = new Intl.NumberFormat("ru-RU");
  const collator = new Intl.Collator("ru-RU");
  let players = [];
  let active = false;
  let read;

  function renderPet(pet) {
    const item = petTemplate.content.firstElementChild.cloneNode(true);
    item.title = `${pet.name} · ур. ${pet.level}`;
    const art = item.querySelector(".player-top-pet-art");
    art.setAttribute("aria-label", item.title);
    if (pet.rarity === "secret") {
      art.setAttribute("data-secret", "");
      showSecretSilhouette(art);
    } else {
      showPetArt(art, pet);
    }
    return item;
  }

  function render() {
    const key = sort.value;
    const ordered = players.slice().sort((first, second) => {
      if (first[key] !== second[key]) return first[key] > second[key] ? -1 : 1;
      return collator.compare(first.name, second.name) || first.id.localeCompare(second.id);
    });
    const list = document.createDocumentFragment();
    for (const [index, player] of ordered.entries()) {
      const entry = rowTemplate.content.firstElementChild.cloneNode(true);
      const row = entry.querySelector(".player-top-row");
      row.dataset.player = player.id;
      row.toggleAttribute("data-me", player.me);
      entry.querySelector(".player-top-place").textContent = String(index + 1);
      row.querySelector(".player-top-initial").textContent = Array.from(player.name.trim())[0]?.toLocaleUpperCase("ru-RU") || "?";
      const name = row.querySelector(".player-top-name");
      name.textContent = player.name;
      name.title = player.name;
      row.querySelector(".player-top-self").hidden = !player.me;
      row.querySelector("[data-player-income]").textContent = `${number.format(player.income)} м/ч`;
      row.querySelector("[data-player-balance]").textContent = number.format(player.balance);
      row.querySelector("[data-player-level]").textContent = `ур. ${number.format(player.chat_level)}`;
      const pets = row.querySelector("[data-player-pets]");
      if (player.pets.length) pets.replaceChildren(...player.pets.map(renderPet));
      else pets.closest(".player-top-pets").dataset.empty = "";
      list.append(entry);
    }
    rows.replaceChildren(list);
    table.hidden = ordered.length === 0;
  }

  async function load() {
    if (!active) return;
    read?.abort();
    const controller = new AbortController();
    read = controller;
    retry.hidden = true;
    if (!players.length) status.textContent = "Загружаем топ игроков…";
    try {
      const state = await request(controller.signal);
      if (controller.signal.aborted) return;
      players = state.players.map((player) => ({
        ...player, xp: BigInt(player.xp), income: BigInt(player.income), balance: BigInt(player.balance),
      }));
      status.textContent = players.length ? "" : "В топе пока нет игроков.";
      render();
    } catch (error) {
      if (controller.signal.aborted) return;
      status.textContent = error.message || "Не удалось загрузить топ игроков.";
      retry.hidden = false;
    }
  }

  sort.addEventListener("change", render);
  retry.addEventListener("click", load);
  return {
    show() {
      if (active) return;
      active = true;
      load();
    },
    hide() {
      if (!active) return;
      active = false;
      read?.abort();
      players = [];
      rows.replaceChildren();
      table.hidden = true;
    },
    reload: load,
  };
}
