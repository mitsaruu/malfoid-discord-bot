# AI options Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Выбор модели и рассуждения, четыре Hydrogen-ответа, адресные минутные тайм-ауты и умный шар.
**Architecture:** Параметры запроса передаются в общий генератор; расход модели хранится в SQLite, резервы освобождает RAII. Отдельная классификация не зависит от истории ответа.
**Tech Stack:** Rust, poise, reqwest, rusqlite, tokio.
**Spec:** ../specs/2026-11-17-ai-options-design.md

## Global Constraints
- Не выводить секреты, не запускать второго бота; не пушить и не выкатывать без запроса.
- Hydrogen: 4 успешных ответа / пользователь / сутки Europe/Moscow, включая auto.
- Прямое оскорбление бота: 60 секунд с первого раза; сомнение не наказывает.

## Review Focus
- Параллельные обращения и отмена не обходят квоту.
- Полночь, перезапуск и разные серверы не сбрасывают расход преждевременно.
- Явный выбор модели не включает скрытый fallback.
- Цитаты и чужие оскорбления не приводят к тайм-ауту.
- Невалидный вывод шара никогда не попадает в чат.

## Task 1: Durable quota (db.rs)
- [x] Тесты лимита, возврата резерва, дней, пользователей и повторного открытия БД.
- [x] Таблица ai_model_usage + атомарные reserve/finish/usage.

## Task 2: Request selection (ai.rs, ai_commands.rs)
- [x] Тесты моделей, уровней, auto, отказа по квоте и отмены.
- [x] Необязательные параметры /ask, запрос без глобальных мутаций, RAII-квота.
- [x] Остаток Hydrogen в /ai_limits, Москва.

## Task 3: Directed moderation (ai.rs, chat.rs, ai_rules.txt)
- [x] Тесты строгого результата классификатора, API-пробы исключений и обзор фиксированной длительности.
- [x] Удалить наказание за любые плохие слова; классифицировать текущее обращение и выдавать 60 секунд.

## Task 4: AI ball (fun.rs)
- [x] Тесты индексов, недопустимых ответов, запасного выбора.
- [x] Flash-Lite-only запрос, короткий timeout, только готовый текст.

## Task 5: Integration
- [x] Проверить diff; Rust tests в изолированной сборке без ключей; обзор изменений.
- [x] Обновить документацию и патчноут; сообщить ограничения реального Discord-тестирования.


## Verification ledger
- Windows Cargo blocked by pre-existing missing dependency metadata (E0463); used isolated Linux workspace without bot tokens.
- Initial regression run: 205 passed, 6 ignored. Delivery guard run: 206 passed. Choice-registration run: 207 passed, 6 ignored.
- Live Flash-Lite structured classifier: direct insult -> bot; third-party insult -> other; quote -> other; ordinary profanity -> none.
- Review fixed shorter-timeout overwrite, Discord-delivery quota settlement, moderation before personal generation limits, and background model pinning.
- Existing longer timeouts are checked using fresh Discord HTTP data; lookup failure skips punishment.
- Final production-mode `cargo check --release --locked -j 1`: passed; three existing dead-code warnings unrelated to AI.
- No bot instance launched, production changes, Git commits or pushes for this feature. Actual slash-command interaction and timeout in Discord still require deployment verification.
