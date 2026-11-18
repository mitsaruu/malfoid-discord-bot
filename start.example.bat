@echo off
chcp 65001 >nul
rem Шаблон запуска Malfoid Discord Bot. Скопируйте его в start.bat и впишите свои ключи после "=".
rem Без пробелов вокруг "=" и без кавычек. Строки с ключами, которых нет, можно удалить.
set DISCORD_TOKEN=сюда_токен_бота
set SPOTIFY_CLIENT_ID=сюда_client_id
set SPOTIFY_CLIENT_SECRET=сюда_client_secret
set GEMINI_API_KEY=сюда_ключ_gemini
rem Для мини-приложения /words и /rocket: Developer Portal -> OAuth2 -> Client Secret
set DISCORD_CLIENT_SECRET=сюда_client_secret_приложения
cd /d "%~dp0"
if not exist "rust\target\release\musicbot.exe" (
    echo Сначала соберите бота: cargo build --manifest-path rust/Cargo.toml --release --locked
    pause
    exit /b 1
)
rem Для первой регистрации на тестовом сервере раскомментируйте и заполните:
rem set TEST_GUILD_ID=
rust\target\release\musicbot.exe
pause
