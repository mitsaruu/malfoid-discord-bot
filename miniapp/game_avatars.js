"use strict";

window.createGameAvatars = function (load) {
  const cache = new Map();

  function remove(id) {
    const entry = cache.get(id);
    cache.delete(id);
    if (entry?.url) URL.revokeObjectURL(entry.url);
  }

  function get(id) {
    if (!cache.has(id)) {
      if (cache.size >= 128) remove(cache.keys().next().value);
      const entry = {};
      cache.set(id, entry);
      entry.pending = Promise.resolve().then(() => load(id)).then(blob => {
        if (blob && cache.get(id) === entry) entry.url = URL.createObjectURL(blob);
      }).catch(() => {});
    }
    return cache.get(id);
  }

  return {
    image(id, name, active = false) {
      const face = document.createElement("span");
      face.className = "game-player-avatar";
      face.dataset.active = String(active);
      face.setAttribute("aria-hidden", "true");
      face.textContent = String(name ?? "").trim().split(/\s+/).slice(0, 2)
        .map(part => Array.from(part)[0] ?? "").join("").toLocaleUpperCase() || "?";
      if (typeof load === "function" && /^\d+$/.test(id ?? "")) {
        const key = String(id);
        const entry = get(key);
        const attach = () => {
          if (!entry.url || cache.get(key) !== entry) return;
          const picture = document.createElement("img");
          picture.alt = "";
          picture.hidden = true;
          picture.onload = () => { picture.hidden = false; };
          picture.src = entry.url;
          face.append(picture);
        };
        if (entry.url) attach();
        else void entry.pending.then(attach);
      }
      return face;
    },
    clear() { for (const id of cache.keys()) remove(id); },
  };
};
