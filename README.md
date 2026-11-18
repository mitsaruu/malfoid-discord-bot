# Malfoid Discord Bot

Бот на Rust с музыкой, ИИ, экономикой, питомцами, мини-играми и модерацией.
[Команды, настройки и обслуживание](rust/README.md).

## Структура проекта

- [`rust/`](rust/) — исходный код бота и тесты.
- [`miniapp/`](miniapp/) — интерфейс игр «Слова» и «Ракетка» в Discord.
- [`config/`](config/) — настройки, словари и автоответы.
- [`docs/superpowers/`](docs/superpowers/) — спецификации и планы разработки.

## Запуск

Установите Rust. Для сборки с музыкой нужны CMake и системный компилятор.
Соберите бота из корня репозитория:

```sh
cargo build --manifest-path rust/Cargo.toml --release --locked
```

- **Windows.** Скопируйте `start.example.bat` в `start.bat`. Укажите токен и запустите файл.
- **Linux.** Передайте `DISCORD_TOKEN` в окружение службы, задайте рабочим каталогом корень проекта и запустите `rust/target/release/musicbot`.
- Для регистрации команд на тестовом сервере задайте `TEST_GUILD_ID`. Для глобальной регистрации задайте `REGISTER_GLOBAL=1`. Entry Point мини-приложения сохраняется.

Включите **Message Content Intent** в Discord Developer Portal для ответов в чате и антиспама.
Сохраняйте `config/`, `levels.db` и ключи при обновлении. Для тестового запуска используйте отдельный `BOT_DIR` и тестовый токен.

Для музыки нужны **yt-dlp** и поддерживаемый им JavaScript-рантайм, например **Deno**. Установите yt-dlp отдельно или в `venv` командой `python -m pip install -r requirements.txt`. Если исполняемый файл недоступен через PATH, задайте `YTDLP`.

## Мини-приложение

`/words` открывает слова, `/rocket` запускает ракетку.

1. В Discord Developer Portal скопируйте OAuth2 Client Secret в переменную `DISCORD_CLIENT_SECRET`. При настройке добавьте адрес возврата `https://127.0.0.1`.
2. Опубликуйте локальный HTTP-сервер бота через HTTPS-прокси или туннель. По умолчанию он слушает `127.0.0.1:8080` из секции `[miniapp]`.
3. В **Activities → URL Mappings** сопоставьте `/` с хостом прокси или туннеля. Включите Activities и нужные платформы в **Supported Platforms**.
4. Перезапустите бота и проверьте запуск внутри Discord.

`patchnotes.txt` содержит историю обновлений. Канал публикации задаётся через `/patchnotes_channel`.
