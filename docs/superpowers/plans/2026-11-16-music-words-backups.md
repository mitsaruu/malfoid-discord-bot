# Избранное, история, итоги и резервные копии — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox syntax.

**Goal:** Добавить избранное, историю, ежедневные результаты в московскую полночь и ежедневные копии, объединить вход в игру.

**Architecture:** Существующий Rust-бот, SQLite и фоновые задачи Tokio. Три независимых модуля: музыкальная библиотека, результаты слов и backup. Использовать текущие очередь, рендер изображений и запуск Discord Activity.

**Tech Stack:** Rust, poise/serenity, tokio, rusqlite с online backup API, image, chrono-tz.

**Spec:** `docs/superpowers/specs/2026-11-16-music-words-backups-design.md`.

## Global Constraints

- Итоги обоих языков и смена слова: 00:00 Europe/Moscow.
- Сохранять старые партии, статистику и работоспособность старых кнопок.
- Хранить последние 7 успешных локальных резервных копий; без `.env` и ключей.
- История: максимум 500 записей на сервер, только фактически начавшиеся треки.
- Не менять три ранее внесённых исправления компиляции; не пушить и не перезапускать рабочую службу.
- Использовать текущий checkout (изменения относятся к этому же боту); не создавать новый worktree без запроса пользователя.

## Review Focus

- Повторная заявка получает автора нажавшего, а не автора исторического трека.
- Чужие кнопки избранного не открывают и не изменяют личную библиотеку.
- Смена суток, перезапуск и ошибка Discord не теряют запланированные итоги и не публикуют открытое слово.
- WAL-база копируется согласованно; неисправная новая копия не удаляет исправные старые.
- Ограничения Discord: 5 кнопок в строке, 25 пунктов select, ограниченные текст и изображения.

## Task 1: Музыкальная библиотека

**Files:** create `rust/src/music_library.rs`; modify `rust/src/music.rs` only for library hooks, early playback event and shared replay helper. Main registration is owned by coordinator.

**Interfaces:** `prepare(&Database) -> rusqlite::Result<()>`; slash `favorites`, `history`; `pressed(&serenity::Context, &Shared, &ComponentInteraction) -> bool`; `favorite_button() -> CreateActionRow`; record a confirmed playback with original URL/title/details. `music::enqueue_saved` joins the caller's voice channel, enqueues fresh metadata and starts the existing `play_next` flow.

- [x] Add tests for favorite dedupe/isolation/removal, history cap/order/isolation, failed playback filtering and fresh requester metadata.
- [x] Run targeted tests; if Windows Application Control blocks Rust, record limitation and use isolated Linux tests.
- [x] Implement schema and interactions, private paged favorites/history, durable favorite button payload, voice replay.
- [x] Verify tests and review integration for delayed interactions and stale buttons.

## Task 2: Резервные копии

**Files:** create `rust/src/backups.rs`; modify `rust/Cargo.toml` only to enable rusqlite backup. Main startup is owned by coordinator.

**Interfaces:** `BackupSettings::load(&Settings)`; `start_loop(Shared, BackupSettings)`; synchronous testable snapshot entry point takes `&Database`, root and timestamp, returns completed path. Source database is `data.db`; source files from `data.dir`.

- [x] Add tests for snapshot integrity including WAL, safe retention, failure preserving copies, exclusion of secrets and symlinks, schedule calculation.
- [x] Implement online snapshot + integrity check + manifest; atomic finalization; permission restriction and retention of owned successful directories.
- [x] Implement default 04:00 Moscow schedule, immediate catch-up without recent backup, and retry on failure. No automatic restore and no external upload.
- [x] Verify targeted tests.

## Task 3: Единая игра и ежедневные итоги

**Files:** create `rust/src/wordsummary.rs`; modify `wordboard.rs`, `words.rs`, `miniapp.rs`, `interactions.rs`, `admin.rs`, `main.rs`, `qol.rs`, `config/settings.ini`, `miniapp/index.html`.

**Interfaces:** `wordsummary::prepare(&Database)`; `remember_channel(...)`; `start_loop(Context, Shared)`; `/words(language: Option<WordLanguage>)` opens Activity without an early defer. Both `Lang::command()` return `/words`.

- [x] Add tests for Moscow midnight, before/at/after rollover, persistent channel and page delivery checkpoints, last-language default, grouping losses and unfinished games.
- [x] Replace registered commands `/wort` and `/slovo` with `/words`; update command gate/help/miniapp copy. Persist language choice. Existing custom IDs remain recognized.
- [x] Replace join-triggered summary with a scheduler and persistent per-page delivery state. At midnight summarize closed day, group best attempts, render avatar/grid pages, add play button; retry failures and catch up yesterday after restart.
- [x] Keep image layout bounded and labels legible; render and visually inspect the sample; verify multi-page bounds in tests.

## Task 4: Integration and verification

**Files:** main wiring, help/category lists, Rust README, patchnotes v12, SQL integration checks if useful.

- [x] Review each independent module against spec; fix integration findings.
- [x] Run full Rust tests with `--locked` and voice enabled; build release. Use isolated Linux build workspace without tokens if Windows blocks toolchain.
- [x] Verify command list has favorites/history and one words command; Activity initial callback and Entry Point registration remain intact.
- [x] Review full diff, preserve secrets and existing fixes, report implementation/tests and precise remaining runtime checks.
- [x] Leave changes reviewable locally; push/deployment requires user instruction.

## Проверено 16.11.2026

- Полная версия с музыкой: 162 Rust-теста прошли, 6 сетевых тестов оставлены ignored.
- Фронтенд: 5 Node-тестов перехода суток, включая устаревший ввод; синтаксис app.js проверен.
- Дополнительно прошли 4 SQL-проверки предыдущего QoL.
- Метаданные реального списка регистрации: 76 команд, единственная команда запуска слов — /words.
- Исправления прошли независимое ревью; PNG 1000×608 проверен визуально.
- Linux cargo build --release --locked -j 1 успешно; две диагностические отметки dead_code. Бинарник запускается без токена и корректно завершает работу до открытия базы/Discord.
- Сборка и тесты выполнены в /home/mitsaru/musicbot-builds/qol2-20261116 без токенов и рабочей базы. Контрольные суммы 51 исходного файла совпадают с локальным checkout.
- Рабочая служба не перезапускалась; изменения не коммитились и не публиковались. Проверка новых кнопок, звука и доставки итогов в настоящем Discord — после разрешённой выкатки.
