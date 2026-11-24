"use strict";

function createAiChat({ request, image, profile }) {
  const byId = id => document.getElementById(id);
  const shell = byId("activity-shell");
  const content = byId("activity-content");
  const sidebar = byId("activity-sidebar");
  const root = byId("ai-chat");
  const title = byId("ai-title");
  const greeting = byId("ai-greeting");
  const menuToggle = byId("ai-menu-toggle");
  const menuClose = byId("ai-menu-close");
  const backdrop = byId("ai-backdrop");
  const historyList = byId("ai-history-list");
  const historyStatus = byId("ai-history-status");
  const historyRetry = byId("ai-history-retry");
  const thread = byId("ai-thread");
  const status = byId("ai-status");
  const chatRetry = byId("ai-chat-retry");
  const form = byId("ai-composer");
  const prompt = byId("ai-prompt");
  const fileInput = byId("ai-file");
  const attach = byId("ai-attach");
  const preview = byId("ai-preview");
  const previewImage = byId("ai-preview-image");
  const previewName = byId("ai-preview-name");
  const removeImage = byId("ai-remove-image");
  const send = byId("ai-send");
  const quotaText = byId("ai-quota");
  const mobile = window.matchMedia("(max-width: 780px)");
  const touchKeyboard = window.matchMedia("(pointer: coarse)");
  const viewport = window.visualViewport;
  const connection = new AbortController();
  const drafts = new Map();
  const conversations = new Map();
  const noTurns = [];
  const supportedImages = new Set(["image/png", "image/jpeg", "image/webp"]);
  const maxImageBytes = 4 * 1024 * 1024;
  const ignoreSwipe = ".ai-overflow, table, pre, math, input, textarea, select, [contenteditable], [data-ai-no-swipe]";

  let visible = false;
  let chatId = null;
  let chats = null;
  let quota = null;
  let quotaTimer;
  let listRequest = null;
  let detailRequest = null;
  let writeRevision = 0;
  let pending = null;
  let menuOpen = false;
  let gesture = null;
  let threadError = "";
  let connectionError = "";
  let renderedTurns = noTurns;
  let pendingTurn = null;
  let turnImages = new AbortController();
  let turnImageUrls = [];

  function currentDraft() {
    if (!drafts.has(chatId)) {
      drafts.set(chatId, { text: "", file: null, url: "", attempt: null, error: "" });
    }
    return drafts.get(chatId);
  }

  function element(tag, className, value) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (value !== undefined) node.textContent = value;
    return node;
  }

  function errorText(error) {
    if (error?.name === "TimeoutError") return "Сервер не успел ответить. Попробуйте ещё раз.";
    if (error?.name === "TypeError") return "Не удалось связаться с сервером. Проверьте соединение и попробуйте ещё раз.";
    return error?.message || "Попробуйте ещё раз.";
  }

  function scrollToLatest() {
    thread.scrollTop = thread.scrollHeight;
  }

  function clearTurnImages() {
    turnImages.abort();
    turnImages = new AbortController();
    for (const url of turnImageUrls) URL.revokeObjectURL(url);
    turnImageUrls = [];
  }

  async function loadTurnImage(id, target, signal) {
    try {
      const blob = await image(id, signal);
      if (signal.aborted) return;
      if (!blob) throw new Error();
      const url = URL.createObjectURL(blob);
      turnImageUrls.push(url);
      const img = element("img", "ai-message-image");
      img.alt = "Прикреплённое изображение";
      img.addEventListener("error", () => { target.textContent = "Предпросмотр недоступен."; });
      img.addEventListener("load", () => {
        if (signal.aborted) return;
        const nearEnd = thread.scrollHeight - thread.scrollTop - thread.clientHeight < 72;
        target.replaceChildren(img);
        if (nearEnd) scrollToLatest();
      });
      img.src = url;
    } catch {
      if (!signal.aborted) target.textContent = "Не удалось загрузить изображение.";
    }
  }

  function createTurn(turn, previewUrl = "") {
    const article = element("article", "ai-turn");
    const question = element("div", "ai-question");
    if (turn.prompt) question.append(element("p", "ai-question-text", turn.prompt));
    if (previewUrl) {
      const img = element("img", "ai-message-image");
      img.alt = "Прикреплённое изображение";
      img.src = previewUrl;
      question.append(img);
    } else if (turn.image) {
      const attachment = element("div", "ai-message-attachment", "Загружаем изображение…");
      question.append(attachment);
      void loadTurnImage(turn.id, attachment, turnImages.signal);
    }
    const reply = element("div", "ai-reply");
    reply.append(element("h2", "ai-reply-label", "Malfoid"));
    const answer = element("div", "ai-answer");
    if (turn.answer_html) {
      // The Activity endpoint supplies sanitized HTML. Preserve its native MathML.
      answer.innerHTML = turn.answer_html;
      for (const link of answer.querySelectorAll("a[href]")) {
        link.target = "_blank";
        link.rel = "noopener noreferrer";
      }
      for (const block of answer.querySelectorAll('table, pre, math[display="block"]')) {
        const region = element("div", "ai-overflow");
        region.tabIndex = 0;
        region.setAttribute("role", "region");
        region.setAttribute("aria-label", block.localName === "table" ? "Таблица"
          : block.localName === "math" ? "Формула" : "Блок кода");
        block.before(region);
        region.append(block);
      }
    } else {
      answer.className += " ai-answer-plain";
      answer.textContent = turn.answer;
    }
    reply.append(answer);
    article.append(question, reply);
    return article;
  }

  function renderThread() {
    const conversation = conversations.get(chatId);
    const turns = conversation?.turns ?? noTurns;
    const currentPending = pending?.chatId === chatId ? pending : null;
    const welcome = chatId === null && !currentPending;
    root.dataset.mode = welcome ? "welcome" : "conversation";
    greeting.hidden = !welcome;
    thread.hidden = welcome;
    thread.setAttribute("aria-busy", String(Boolean(detailRequest)));
    title.textContent = conversation?.chat.title || "Чат с ИИ";
    prompt.placeholder = welcome ? "Спросить Malfoid" : "Напишите сообщение";
    if (renderedTurns !== turns || !visible) {
      clearTurnImages();
      thread.replaceChildren(...(visible ? turns.map(turn => createTurn(turn)) : []));
      renderedTurns = visible ? turns : noTurns;
      pendingTurn = null;
    }
    if (pendingTurn) {
      pendingTurn.remove();
      pendingTurn = null;
    }
    if (currentPending && visible) {
      pendingTurn = createTurn({ prompt: currentPending.text, answer: "Готовлю ответ…" }, currentPending.url);
      thread.append(pendingTurn);
    }
    scrollToLatest();
  }

  function renderComposer() {
    const draft = currentDraft();
    const sending = pending?.chatId === chatId;
    const limited = quota?.remaining === 0;
    prompt.readOnly = sending;
    attach.disabled = sending || Boolean(connectionError);
    removeImage.disabled = sending;
    const retrying = Boolean(draft.attempt);
    send.disabled = Boolean(connectionError || pending || detailRequest || threadError
      || !(draft.text.trim() || draft.file) || (limited && !retrying));
    const sendLabel = retrying && !sending ? "Повторить отправку" : "Отправить";
    send.setAttribute("aria-label", sendLabel);
    send.title = sendLabel;
    const notice = connectionError || threadError || draft.error
      || (detailRequest ? "Загружаем чат…" : sending ? "Malfoid готовит ответ…" : "");
    if (status.textContent !== notice) status.textContent = notice;
    status.dataset.error = String(Boolean(connectionError || threadError || draft.error));
    chatRetry.hidden = !threadError || Boolean(connectionError);
    const quotaLabel = limited ? "0 из " + quota.limit + " · Сброс в 00:00 МСК"
      : quota ? "Осталось " + quota.remaining + " из " + quota.limit
      : "Лимит пока неизвестен";
    if (quotaText.textContent !== quotaLabel) quotaText.textContent = quotaLabel;
    quotaText.dataset.limited = String(limited);
  }

  function restoreDraft() {
    const draft = currentDraft();
    prompt.value = draft.text;
    preview.hidden = !draft.file;
    previewName.textContent = draft.file?.name ?? "";
    if (draft.url) {
      previewImage.hidden = false;
      previewImage.src = draft.url;
    } else {
      previewImage.hidden = true;
      previewImage.removeAttribute("src");
    }
    renderComposer();
  }

  function renderHistory() {
    const focused = document.activeElement?.dataset?.chatId;
    let restoredFocus;
    historyList.replaceChildren(...(chats ?? []).map(chat => {
      const row = element("li");
      const button = element("button", "ai-history-link", chat.title);
      button.type = "button";
      button.title = chat.title;
      button.dataset.chatId = chat.id;
      if (chat.id === chatId) button.setAttribute("aria-current", "true");
      if (chat.id === focused) restoredFocus = button;
      button.addEventListener("click", () => selectChat(chat.id));
      row.append(button);
      return row;
    }));
    restoredFocus?.focus({ preventScroll: true });
  }

  function scheduleQuota(delay) {
    clearTimeout(quotaTimer);
    if (!visible || !quota || connectionError) return;
    const timeToReset = quota.reset_at * 1000 - Date.now();
    quotaTimer = setTimeout(refreshHistory, delay ?? (timeToReset > 0 ? timeToReset + 1000 : 30000));
  }

  function updateQuota(next) {
    if (quota && next.reset_at < quota.reset_at) return;
    quota = next;
    scheduleQuota();
    renderComposer();
  }

  async function refreshHistory() {
    if (listRequest || connectionError) return;
    const revision = writeRevision;
    historyRetry.hidden = true;
    if (!chats) historyStatus.textContent = "Загружаем историю…";
    listRequest = request("api/ai", undefined, connection.signal);
    try {
      const result = await listRequest;
      if (connectionError || revision !== writeRevision) return;
      chats = result.chats;
      historyStatus.textContent = chats.length ? "" : "Чатов пока нет";
      renderHistory();
      updateQuota(result.quota);
    } catch (error) {
      if (connectionError || revision !== writeRevision) return;
      historyStatus.textContent = "Не удалось обновить историю. " + errorText(error);
      historyRetry.hidden = false;
      if (quota && quota.reset_at * 1000 <= Date.now()) scheduleQuota(30000);
    } finally {
      listRequest = null;
      if (revision !== writeRevision) void refreshHistory();
    }
  }

  async function selectChat(id, { fromMenu = true } = {}) {
    detailRequest?.abort();
    chatId = id;
    threadError = "";
    const controller = id === null || connectionError ? null : new AbortController();
    detailRequest = controller;
    restoreDraft();
    renderHistory();
    renderThread();
    if (fromMenu) {
      setMenu(false, false);
      if (id === null && !mobile.matches) prompt.focus({ preventScroll: true });
      else title.focus({ preventScroll: true });
    }
    if (!controller) return;
    const revision = writeRevision;
    try {
      const result = await request(`api/ai/chats/${encodeURIComponent(id)}`, undefined, controller.signal);
      if (controller.signal.aborted) return;
      conversations.set(id, result);
      if (revision === writeRevision) updateQuota(result.quota);
    } catch (error) {
      if (!controller.signal.aborted) threadError = "Не удалось открыть чат. " + errorText(error);
    } finally {
      if (detailRequest === controller) {
        detailRequest = null;
        renderThread();
        renderComposer();
      }
    }
  }

  function setAttachment(file, message = "") {
    const draft = currentDraft();
    if (draft.url) URL.revokeObjectURL(draft.url);
    draft.file = file;
    draft.url = file ? URL.createObjectURL(file) : "";
    draft.attempt = null;
    draft.error = message;
    restoreDraft();
  }

  function encodeImage(file) {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve({
        mime: file.type,
        data: reader.result.split(",", 2)[1],
      });
      reader.onerror = reader.onabort = () => reject(new Error("Не удалось прочитать изображение."));
      reader.readAsDataURL(file);
    });
  }

  async function submit(event) {
    event.preventDefault();
    renderComposer();
    if (send.disabled) return;
    const id = chatId;
    const draft = currentDraft();
    const attempt = {
      chatId: id, text: draft.text.trim(), url: draft.url,
      requestId: draft.attempt ?? crypto.randomUUID(),
    };
    let updateCurrentDraft = false;
    draft.error = "";
    pending = attempt;
    if (touchKeyboard.matches) prompt.blur();
    else if (document.activeElement === send) prompt.focus({ preventScroll: true });
    renderComposer();
    renderThread();
    try {
      const body = { request_id: attempt.requestId, prompt: attempt.text };
      if (id !== null) body.chat_id = id;
      if (draft.file) body.image = await encodeImage(draft.file);
      if (connection.signal.aborted) return;
      draft.attempt = attempt.requestId;
      const result = await request("api/ai/send", body, connection.signal);
      if (connection.signal.aborted) return;
      const savedId = result.chat.id;
      writeRevision++;
      conversations.set(savedId, result);
      chats = [result.chat, ...(chats ?? []).filter(chat => chat.id !== savedId)];
      historyStatus.textContent = "";
      if (draft.url) URL.revokeObjectURL(draft.url);
      drafts.delete(id);
      if (id === chatId) {
        updateCurrentDraft = true;
        detailRequest?.abort();
        detailRequest = null;
        threadError = "";
        chatId = savedId;
      }
      updateQuota(result.quota);
      renderHistory();
    } catch (error) {
      if (!connectionError) {
        draft.error = errorText(error);
        if (draft.attempt) {
          writeRevision++;
          void refreshHistory();
        }
      }
    } finally {
      pending = null;
      if (chatId === id || updateCurrentDraft) {
        renderThread();
        restoreDraft();
      } else {
        renderComposer();
      }
    }
  }

  function setMenu(open, restoreFocus = true) {
    const wasOpen = menuOpen;
    menuOpen = open && visible && mobile.matches;
    sidebar.inert = visible && mobile.matches && !menuOpen;
    content.inert = menuOpen;
    menuToggle.setAttribute("aria-expanded", String(menuOpen));
    backdrop.hidden = !menuOpen;
    if (menuOpen) {
      shell.dataset.aiMenu = "open";
      sidebar.setAttribute("role", "dialog");
      sidebar.setAttribute("aria-modal", "true");
      menuClose.focus({ preventScroll: true });
    } else {
      delete shell.dataset.aiMenu;
      sidebar.removeAttribute("role");
      sidebar.removeAttribute("aria-modal");
      if (wasOpen && visible && restoreFocus) {
        (mobile.matches ? menuToggle : title).focus({ preventScroll: true });
      }
    }
  }

  function updateViewport() {
    const atLatest = visible && !thread.hidden && thread.scrollHeight - thread.scrollTop - thread.clientHeight < 72;
    if (visible && mobile.matches && viewport && viewport.scale === 1) {
      shell.style.setProperty("--ai-viewport-height", `${viewport.height}px`);
      shell.style.setProperty("--ai-viewport-top", `${viewport.offsetTop}px`);
      if (window.innerHeight - viewport.height > 100) shell.style.setProperty("--ai-bottom-inset", "0px");
      else shell.style.removeProperty("--ai-bottom-inset");
    } else {
      shell.style.removeProperty("--ai-viewport-height");
      shell.style.removeProperty("--ai-viewport-top");
      shell.style.removeProperty("--ai-bottom-inset");
    }
    if (atLatest) scrollToLatest();
  }

  function finishGesture(event, finished) {
    if (!gesture) return;
    const touch = [...event.changedTouches].find(item => item.identifier === gesture.id);
    if (!touch) return;
    const dx = touch.clientX - gesture.x;
    const dy = touch.clientY - gesture.y;
    const forward = gesture.open ? -dx : dx;
    if (finished) {
      if (forward >= 56 && forward >= Math.abs(dy) * 1.5) {
        if (event.cancelable) event.preventDefault();
        setMenu(!gesture.open);
      }
      gesture = null;
    } else if ((Math.abs(dy) > 10 && Math.abs(dy) >= Math.abs(dx)) || forward < -10) {
      gesture = null;
    } else if (forward > 10 && forward >= Math.abs(dy) * 1.5 && event.cancelable) {
      event.preventDefault();
    }
  }

  prompt.addEventListener("input", () => {
    const draft = currentDraft();
    if (draft.text.trim() !== prompt.value.trim()) draft.attempt = null;
    draft.text = prompt.value;
    draft.error = "";
    renderComposer();
  });
  prompt.addEventListener("keydown", event => {
    if (event.key !== "Enter" || event.shiftKey || event.isComposing || event.keyCode === 229) return;
    if (touchKeyboard.matches && !event.ctrlKey && !event.metaKey) return;
    if (send.disabled) return;
    event.preventDefault();
    form.requestSubmit();
  });
  fileInput.addEventListener("change", () => {
    const file = fileInput.files[0];
    fileInput.value = "";
    if (!file || attach.disabled) return;
    if (file.size && file.size <= maxImageBytes && supportedImages.has(file.type)) {
      setAttachment(file);
    } else {
      currentDraft().error = "Поддерживаются только PNG, JPEG и WebP до 4 МБ.";
      renderComposer();
    }
  });
  previewImage.addEventListener("error", () => { previewImage.hidden = true; });
  attach.addEventListener("click", () => fileInput.click());
  removeImage.addEventListener("click", () => {
    setAttachment(null);
    attach.focus({ preventScroll: true });
  });
  form.addEventListener("submit", submit);
  byId("ai-new-chat").addEventListener("click", () => selectChat(null));
  chatRetry.addEventListener("click", () => selectChat(chatId, { fromMenu: false }));
  historyRetry.addEventListener("click", refreshHistory);
  menuToggle.addEventListener("click", () => setMenu(!menuOpen));
  menuClose.addEventListener("click", () => setMenu(false));
  backdrop.addEventListener("click", () => setMenu(false));
  mobile.addEventListener("change", () => {
    setMenu(false);
    updateViewport();
  });
  document.addEventListener("keydown", event => {
    if (!menuOpen) return;
    if (event.key === "Escape") {
      event.preventDefault();
      setMenu(false);
    } else if (event.key === "Tab") {
      const controls = [...sidebar.querySelectorAll("button:not(:disabled)")].filter(button => button.getClientRects().length);
      const first = controls[0];
      const last = controls.at(-1);
      if ((event.shiftKey && document.activeElement === first) || (!event.shiftKey && document.activeElement === last)) {
        event.preventDefault();
        (event.shiftKey ? last : first).focus();
      }
    }
  });
  shell.addEventListener("touchstart", event => {
    gesture = null;
    if (!visible || !mobile.matches || event.touches.length !== 1 || event.target.closest?.(ignoreSwipe)) return;
    const touch = event.touches[0];
    if (!menuOpen && touch.clientX > 24) return;
    gesture = { id: touch.identifier, x: touch.clientX, y: touch.clientY, open: menuOpen };
  }, { passive: true });
  shell.addEventListener("touchmove", event => finishGesture(event, false), { passive: false });
  shell.addEventListener("touchend", event => finishGesture(event, true), { passive: false });
  shell.addEventListener("touchcancel", () => { gesture = null; });
  viewport?.addEventListener("resize", updateViewport);
  viewport?.addEventListener("scroll", updateViewport);
  document.addEventListener("visibilitychange", () => {
    if (visible && document.visibilityState === "visible") void refreshHistory();
  });

  let loadedProfile = false;
  async function loadProfile() {
    if (loadedProfile || !profile || connectionError) return;
    loadedProfile = true;
    byId("ai-profile-name").textContent = profile.name;
    byId("ai-profile-initial").textContent = Array.from(profile.name)[0]?.toUpperCase() ?? "?";
    byId("ai-greeting-title").textContent = `Привет, ${profile.name}!`;
    try {
      const blob = await profile.avatar();
      if (!blob || connectionError) return;
      const avatar = byId("ai-profile-avatar");
      const url = URL.createObjectURL(blob);
      avatar.addEventListener("load", () => {
        avatar.hidden = false;
        URL.revokeObjectURL(url);
      }, { once: true });
      avatar.addEventListener("error", () => URL.revokeObjectURL(url), { once: true });
      avatar.src = url;
    } catch {
      // The profile initial remains visible when the avatar cannot be loaded.
    }
  }

  return {
    show() {
      visible = true;
      setMenu(false);
      updateViewport();
      scheduleQuota();
      restoreDraft();
      renderThread();
      void refreshHistory();
      void loadProfile();
    },
    hide() {
      visible = false;
      setMenu(false);
      updateViewport();
      clearTimeout(quotaTimer);
      renderThread();
    },
    disconnect(reason) {
      connectionError = reason;
      connection.abort();
      detailRequest?.abort();
      detailRequest = null;
      clearTimeout(quotaTimer);
      chatRetry.hidden = historyRetry.hidden = true;
      byId("ai-new-chat").disabled = true;
      historyStatus.textContent = reason;
      renderComposer();
    },
  };
}
