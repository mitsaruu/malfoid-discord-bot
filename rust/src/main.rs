//! Discord-бот. Настройки и levels.db находятся в каталоге бота.
//!
//! Переменные окружения:
//!   DISCORD_TOKEN   — токен бота (обязательно);
//!   TEST_GUILD_ID   — id тестового сервера: команды регистрируются только на нём;
//!   REGISTER_GLOBAL — 1: заменить список глобальных команд приложения;
//!   BOT_DIR         — папка с config/ и levels.db (по умолчанию — текущая или папка выше);
//!   YTDLP           — как запускать yt-dlp (по умолчанию «yt-dlp»; на сервере — «venv/bin/python -m yt_dlp»).

mod admin;
mod backups;
mod ai;
mod ai_activity;
mod ai_commands;
mod ai_math;
mod blackjack;
mod cards;
mod chat;
mod config;
mod daily;
mod db;
mod draw;
mod durak;
mod durak_bank;
mod durak_online;
mod economy;
mod fun;
mod interactions;
mod levels;
mod miniapp;
mod moderation;
mod patchnotes;
mod pets;
mod polls;
mod qol;
#[cfg(feature="voice")]
mod music_qol;
mod reminders;
mod reply;
mod rocket;
mod casino_solo;
mod poker;
mod table_blackjack;
mod roulette;
mod shop;
#[cfg(feature = "voice")]
mod music;
#[cfg(feature = "voice")]
mod spotify;
mod text;
mod wagers;
mod weather;
mod diagnostics;
mod music_channels;
#[cfg(feature = "voice")]
mod music_queue;
#[cfg(feature = "voice")]
mod music_runtime;
mod wordboard;
mod wordstats;
mod wordsummary;
mod words;
#[cfg(feature = "voice")]
mod music_library;

use poise::serenity_prelude as serenity;
#[cfg(feature = "voice")]
use songbird::SerenityInit;
use std::path::PathBuf;

/// Общие данные бота — доступны из каждой команды через ctx.data().
pub struct Data {
    pub started_at: std::time::Instant,
    pub diagnostics: diagnostics::DiagnosticsState,
    #[cfg(feature = "voice")]
    pub music_runtime: music_runtime::RuntimeState,
    pub db: std::sync::Arc<db::Database>,
    pub economy: economy::EconomySettings,
    pub rocket: rocket::State,
    pub casino_solo: casino_solo::State,
    pub poker: poker::State,
    pub table_blackjack: table_blackjack::State,
    pub roulette: roulette::State,
    pub durak: durak::State,
    pub durak_online: durak_online::State,
    pub pets: pets::PetSettings,
    pub shop: shop::State,
    pub levels: levels::LevelSettings,
    pub growth: fun::GrowthSettings,
    /// ИИ: настройки, ключ Gemini и счётчики лимитов
    pub ai: ai::AiState,
    pub quiz: ai_commands::QuizSettings,
    /// Бот в чате: автоответы, анти-спам, ответы ботам, случайные реплики
    pub chat: chat::ChatState,
    /// Варн номер N → тайм-аут (settings.ini → [moderation] warn_punishments)
    pub warn_punishments: Vec<(i64, i64)>,
    /// Каналы, где викторина идёт прямо сейчас
    pub quiz_running: std::sync::Mutex<std::collections::HashSet<u64>>,
    /// Сервер → последние вопросы викторины (чтобы ИИ не повторялся)
    pub quiz_recent: std::sync::Mutex<std::collections::HashMap<u64, Vec<String>>>,
    /// Команды, которые работают только в канале для игр (если его выбрали)
    pub games_commands: std::collections::BTreeSet<String>,
    /// Игры в слова: настройки и словари
    pub words: words::WordsState,
    /// Картинки игр в слова в чате
    pub boards: wordboard::BoardState,
    /// Доступен ли локальный HTTP-сервер мини-приложения.
    miniapp_ready: std::sync::atomic::AtomicBool,
    /// Папка бота: config/, levels.db, фоны картинок и шрифты
    pub dir: PathBuf,
    /// Для скачивания аватарок
    pub web: reqwest::Client,
    /// Кто сейчас играет в блэкджек: нельзя начать вторую партию, не закончив первую
    pub blackjack_players: std::sync::Mutex<std::collections::HashSet<u64>>,
    /// Башмак блэкджека на каждом сервере: id сервера → оставшиеся карты
    pub blackjack_shoes: std::sync::Mutex<std::collections::HashMap<u64, Vec<blackjack::Card>>>,
    #[cfg_attr(not(feature = "voice"), allow(dead_code))]
    pub ytdlp: String,
    /// Музыка: что играет на каждом сервере (очередь — в базе)
    #[cfg(feature = "voice")]
    pub music: music::MusicState,
}

impl Data {
    pub fn miniapp_ready(&self) -> bool {
        self.miniapp_ready.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Собирает данные бота из базы и настроек. dir — папка бота.
    pub fn new(database: db::Database, settings: &config::Settings, dir: PathBuf, miniapp_ready: bool) -> Self {
        let ytdlp = std::env::var("YTDLP").ok().filter(|value| !value.trim().is_empty()).map_or_else(|| default_ytdlp(&dir), |value| fix_ytdlp(&value));
        Self {
            started_at: std::time::Instant::now(),
            diagnostics: Default::default(),
            #[cfg(feature = "voice")]
            music_runtime: Default::default(),
            db: std::sync::Arc::new(database),
            economy: economy::EconomySettings::load(settings),
            rocket: rocket::State::with_settings(settings),
            casino_solo: Default::default(),
            poker: Default::default(),
            table_blackjack: Default::default(),
            roulette: Default::default(),
            durak: Default::default(),
            durak_online: Default::default(),
            pets: pets::PetSettings::load(settings),
            shop: Default::default(),
            levels: levels::LevelSettings::load(settings),
            growth: fun::GrowthSettings::load(settings),
            ai: ai::AiState::new(settings, std::env::var("GEMINI_API_KEY").ok()),
            quiz: ai_commands::QuizSettings::load(settings),
            chat: chat::ChatState::new(settings),
            warn_punishments: moderation::parse_warn_punishments(&settings.text("moderation", "warn_punishments", "3: 60, 5: 1440, 7: 10080")),
            quiz_running: Default::default(),
            quiz_recent: Default::default(),
            games_commands: admin::parse_commands(&settings.text("channels", "games_commands", admin::DEFAULT_GAMES_COMMANDS)),
            words: words::WordsState::new(words::WordSettings::load(settings)),
            boards: Default::default(),
            miniapp_ready: std::sync::atomic::AtomicBool::new(miniapp_ready),
            dir,
            web: reqwest::Client::new(),
            blackjack_players: Default::default(),
            blackjack_shoes: Default::default(),
            ytdlp,
            #[cfg(feature = "voice")]
            music: Default::default(),
        }
    }
}

/// Как запускать yt-dlp, если переменная YTDLP не задана: из venv рядом с ботом,
/// а если его нет — программа yt-dlp из PATH.
fn default_ytdlp(dir: &std::path::Path) -> String {
    for python in [dir.join("venv").join("bin").join("python"), dir.join("venv").join("Scripts").join("python.exe")] {
        let path = python.display().to_string();
        if python.is_file() {
            return if path.contains(char::is_whitespace) {
                format!("\"{path}\" -m yt_dlp")
            } else {
                format!("{path} -m yt_dlp")
            };
        }
    }
    "yt-dlp".to_string()
}

fn ytdlp_parts(value: &str) -> (&str, &str) {
    let value = value.trim();
    if let Some((program, args)) = value.strip_prefix('"').and_then(|rest| rest.split_once('"')) {
        return (program, args.trim_start());
    }
    value.split_once(char::is_whitespace).unwrap_or((value, ""))
}

/// YTDLP указывает просто на python (например, systemd отрезал «-m yt_dlp» после пробела) — дописываем модуль.
fn fix_ytdlp(value: &str) -> String {
    let value = value.trim();
    let (program, args) = ytdlp_parts(value);
    let name = std::path::Path::new(program).file_name().and_then(|name| name.to_str()).unwrap_or("");
    if name.starts_with("python") && !args.split_whitespace().any(|part| part == "-m") {
        format!("{value} -m yt_dlp")
    } else {
        value.to_string()
    }
}

pub type Error = Box<dyn std::error::Error + Send + Sync>;
/// Данные бота общие для команд и фоновых задач, поэтому лежат в Arc
pub type Shared = std::sync::Arc<Data>;
pub type Context<'a> = poise::Context<'a, Shared, Error>;

/// Папка бота: BOT_DIR, иначе текущая (если в ней есть config/), иначе папка выше (запуск из rust/).
fn bot_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("BOT_DIR") {
        return PathBuf::from(dir);
    }
    let here = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if here.join("config").is_dir() {
        here
    } else {
        here.parent().map(PathBuf::from).unwrap_or(here)
    }
}

/// Ошибка внутри команды: пишем в консоль и коротко отвечаем человеку.
async fn on_error(error: poise::FrameworkError<'_, Shared, Error>) {
    match error {
        poise::FrameworkError::Command { error, ctx, .. } => {
            let details = interactions::sanitize_error(&format!("{}: {error:?}", interactions::error_context(ctx)));
            println!("Ошибка в команде: {details}");
            let place = format!("команда /{}", ctx.command().name);
            if !interactions::is_expired(&error) {
                let reply = poise::CreateReply::default().content("😵 Что-то пошло не так. Ошибка записана в консоль.").ephemeral(true);
                let _ = ctx.send(reply).await;
            }
            admin::report_error(ctx.http(), ctx.data(), ctx.guild_id(), &place, &details).await;
        }
        poise::FrameworkError::ArgumentParse { ctx, .. } => {
            // Например, Discord прислал параметры в другом виде (команды зарегистрированы другой версией бота)
            let details = format!("{}: параметры не соответствуют схеме команды", interactions::error_context(ctx));
            println!("Не разобрала параметры /{}: {details}", ctx.command().name);
            let place = format!("параметры команды /{}", ctx.command().name);
            let text = "😵 Не поняла параметры команды. Если команды только что обновились — перезапусти Discord (Ctrl+R).";
            let _ = ctx.send(poise::CreateReply::default().content(text).ephemeral(true)).await;
            admin::report_error(ctx.http(), ctx.data(), ctx.guild_id(), &place, &details).await;
        }
        poise::FrameworkError::CommandCheckFailed { error: Some(error), ctx, .. } => {
            let details = interactions::sanitize_error(&format!("{}: {error:?}", interactions::error_context(ctx)));
            println!("Ошибка перед командой: {details}");
            if !interactions::is_expired(&error) {
                let _ = ctx.send(poise::CreateReply::default().content("😵 Не получилось выполнить проверку команды.").ephemeral(true)).await;
            }
            admin::report_error(ctx.http(), ctx.data(), ctx.guild_id(), "проверка команды", &details).await;
        }
        other => {
            println!("Ошибка: {}", interactions::sanitize_error(&other.to_string()));
            if let Err(error) = poise::builtins::on_error(other).await {
                println!("Ошибка при обработке ошибки: {error}");
            }
        }
    }
}

/// Регистрирует команды глобально. Если в Developer Portal включены Activities, у приложения есть
/// команда-«точка входа» (Entry Point, тип 4) — Discord не даёт удалить её при массовом обновлении,
/// поэтому отправляем её вместе с нашими командами. Возвращает, сколько наших команд.
async fn register_globally(token: &str, app_id: u64, commands: &[poise::Command<Shared, Error>]) -> Result<usize, Error> {
    let url = format!("https://discord.com/api/v10/applications/{app_id}/commands");
    let web = reqwest::Client::builder().timeout(std::time::Duration::from_secs(20)).build()?;
    let auth = format!("Bot {token}");
    let response = web.get(&url).header("Authorization", &auth).send().await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        return Err(format!("Discord не отдал список команд: {status} {text}").into());
    }
    let existing = serde_json::from_str(&text)?;
    let (payload, ours) = global_command_payload(commands, existing)?;
    let response = web.put(&url).header("Authorization", &auth).header("Content-Type", "application/json").body(serde_json::to_string(&payload)?).send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("Discord не принял команды: {status} {}", response.text().await.unwrap_or_default()).into());
    }
    Ok(ours)
}

fn global_command_payload(
    commands: &[poise::Command<Shared, Error>],
    existing: Vec<serde_json::Value>,
) -> Result<(Vec<serde_json::Value>, usize), serde_json::Error> {
    let mut payload = poise::builtins::create_application_commands(commands)
        .into_iter().map(serde_json::to_value).collect::<Result<Vec<_>, _>>()?;
    let count = payload.len();
    payload.extend(existing.into_iter().filter(|command| command["type"] == 4));
    Ok((payload, count))
}

/// Раз в 20 секунд: напоминания, у которых пришло время, и опросы, которые пора закрыть.
fn start_fast_loop(http: std::sync::Arc<serenity::Http>, data: Shared) {
    let (reminder_http, reminder_data) = (http.clone(), data.clone());
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(20));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            if let Err(error) = reminders::send_due(&reminder_http, &reminder_data).await {
                println!("Напоминания: {error}");
            }
        }
    });
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(20));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            if let Err(error) = polls::close_finished(&http, &data).await {
                println!("Опросы: {error}");
            }
        }
    });
}

fn bot_commands() -> Vec<poise::Command<Shared, Error>> {
    vec![
        economy::balance(),
        economy::work(),
        economy::daily(),
        economy::give_money(),
        blackjack::blackjack(),
        pets::pets_collect(),
        pets::pet_trade(),
        shop::shop(),
        shop::shop_buy(),
        shop::shop_add(),
        shop::shop_remove(),
        levels::rank(),
        levels::leaderboard(),
        levels::money_leaderboard(),
        wordboard::games(),
        wordstats::wordstats(),
        wordstats::wordtop(),
        wordstats::wordduel(),
        fun::dice(),
        fun::roll(),
        fun::predskazanie(),
        fun::shar(),
        fun::dick_growth(),
        fun::dick_size(),
        fun::dick_leaderboard(),
        reminders::remind(),
        reminders::reminders(),
        polls::poll(),
        patchnotes::show(),
        patchnotes::set_channel(),
        admin::set_games_channel(),
        admin::set_log_channel(),
        admin::show_warns(),
        admin::warns_clear(),
        weather::weather(),
        weather::weather_cities(),
        ai_commands::forget(),
        ai_commands::ai_limits(),
        ai_commands::summary(),
        ai_commands::quiz(),
        qol::help(),
        qol::server_settings(),
        #[cfg(feature="voice")]
        music_qol::nowplaying(),
        #[cfg(feature="voice")]
        music_qol::queue_remove(),
        #[cfg(feature="voice")]
        music_qol::queue_move(),
        #[cfg(feature="voice")]
        music_qol::queue_clear_mine(),
        #[cfg(feature="voice")]
        music_library::favorites(),
        #[cfg(feature="voice")]
        music_library::history(),
        music_channels::set_music_channel(),
        diagnostics::status(),
        daily::daily_setup(),
        daily::daily_off(),
        daily::daily_test(),
        #[cfg(feature = "voice")]
        music::play(),
        #[cfg(feature = "voice")]
        music::skip(),
        #[cfg(feature = "voice")]
        music::pause(),
        #[cfg(feature = "voice")]
        music::resume(),
        #[cfg(feature = "voice")]
        music::show_queue(),
        #[cfg(feature = "voice")]
        music::remove_all(),
        #[cfg(feature = "voice")]
        music::setvolume(),
        #[cfg(feature = "voice")]
        music::leave(),
        #[cfg(feature = "voice")]
        music::autoplay(),
        #[cfg(feature = "voice")]
        music::playlist_save(),
        #[cfg(feature = "voice")]
        music::playlist_play(),
        #[cfg(feature = "voice")]
        music::show_playlists(),
        #[cfg(feature = "voice")]
        music::playlist_delete(),
        #[cfg(feature = "voice")]
        music::lyrics(),
        #[cfg(feature = "voice")]
        music::top_chart_month(),
    ]
}

#[derive(Debug, PartialEq, Eq)]
enum Registration {
    Disabled,
    Guild(u64),
    Global,
}

fn registration(guild: Option<&str>, global: Option<&str>) -> Result<Registration, &'static str> {
    match (guild, global) {
        (Some(_), Some("1")) => Err("Выбери один режим: TEST_GUILD_ID или REGISTER_GLOBAL=1."),
        (Some(guild), _) => guild.parse::<std::num::NonZeroU64>()
            .map(|id| Registration::Guild(id.get()))
            .map_err(|_| "TEST_GUILD_ID должен быть ненулевым числовым ID сервера."),
        (None, Some("1")) => Ok(Registration::Global),
        (None, _) => Ok(Registration::Disabled),
    }
}

fn lock_instance(dir: &std::path::Path) -> std::io::Result<std::fs::File> {
    let lock = std::fs::OpenOptions::new().read(true).write(true)
        .create(true).truncate(false).open(dir.join(".musicbot.lock"))?;
    lock.try_lock().map_err(std::io::Error::from)?;
    Ok(lock)
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Бот остановлен: {}", interactions::sanitize_error(&error.to_string()));
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Error> {
    let token = std::env::var("DISCORD_TOKEN").ok().filter(|token| !token.trim().is_empty())
        .ok_or("Нет токена: задай переменную окружения DISCORD_TOKEN.")?;
    let registration = registration(
        std::env::var("TEST_GUILD_ID").ok().as_deref(),
        std::env::var("REGISTER_GLOBAL").ok().as_deref(),
    )?;
    let dir = bot_dir();
    let _instance = lock_instance(&dir)
        .map_err(|error| format!("Каталог {} уже используется или недоступен: {error}", dir.display()))?;
    let settings = config::Settings::load_checked(&dir.join("config").join("settings.ini"))?;
    let database = match db::Database::open(&dir.join("levels.db")) {
        Ok(database) => database,
        Err(error) => {
            println!("Не получилось открыть базу {}: {error}", dir.join("levels.db").display());
            std::process::exit(1);
        }
    };
    match wagers::refund(&database, None) {
        Ok(0) => {},
        Ok(count) => println!("Возвращены ставки прерванных игр: {count}"),
        Err(error) => {
            eprintln!("Не получилось вернуть ставки прерванных игр: {error}");
            std::process::exit(1);
        }
    }
    #[cfg(feature = "voice")]
    if let Err(error) = music_runtime::recover(&database) {
        println!("Не получилось восстановить сохранённую очередь: {error}");
        std::process::exit(1);
    }
    // Мини-приложение: секрет приложения из Developer Portal → OAuth2 и адрес веб-сервера из settings.ini
    let client_secret = std::env::var("DISCORD_CLIENT_SECRET").ok().filter(|secret| !secret.is_empty());
    let miniapp_host = settings.text("miniapp", "host", "127.0.0.1");
    let miniapp_port = settings.int("miniapp", "port", 8080).clamp(0, 65535) as u16;
    let backup_settings = backups::BackupSettings::load(&settings);
    let miniapp = if let Some(secret) = client_secret.filter(|_| miniapp_port > 0) {
        match tokio::net::TcpListener::bind((miniapp_host.as_str(), miniapp_port)).await {
            Ok(listener) => Some((secret, listener)),
            Err(error) => {
                eprintln!("Мини-приложение не запустилось на {miniapp_host}:{miniapp_port}: {error}");
                None
            }
        }
    } else {
        None
    };
    let data: Shared = std::sync::Arc::new(Data::new(database, &settings, dir.clone(), miniapp.is_some()));

    let token_for_setup = token.clone();
    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: bot_commands(),
            on_error: |error| Box::pin(on_error(error)),
            // Перед каждой командой: игры и рейтинги — только в канале для игр, если его выбрали
            command_check: Some(|ctx| Box::pin(interactions::before_command(ctx))),
            event_handler: |framework, event| {
                Box::pin(async move {
                    let (ctx, data) = (framework.serenity_context, framework.user_data);
                    // Кнопка «Играть!» под картинкой игры в слова
                    if let serenity::FullEvent::InteractionCreate { interaction: serenity::Interaction::Component(press) } = event {
                        if rocket::pressed(ctx, press).await { return Ok(()); }
                        if roulette::pressed(ctx, press).await? { return Ok(()); }
                        if qol::help_pressed(ctx,press,&framework.options.commands).await{return Ok(())}
                        if ["music:", "library:", "queue:", "version:"].iter().any(|prefix| press.data.custom_id.starts_with(prefix))
                            && !music_channels::button_check(ctx, data, press).await? {
                            return Ok(());
                        }
                        // …и кнопки вариантов в опросах
                        #[cfg(feature = "voice")]
                        if music_queue::pressed(ctx,data,press).await{return Ok(())}
                        #[cfg(feature = "voice")]
                        if music_qol::version_pressed(ctx,data,press).await{return Ok(())}
                        #[cfg(feature = "voice")]
                        if music_library::pressed(ctx,data,press).await{return Ok(())}
                        #[cfg(feature = "voice")]
                        if music::button_pressed(ctx, data, press).await {
                            return Ok(()); // кнопки плеера: пауза, дальше, повтор, перемешать
                        }
                        if !wordboard::play_pressed(ctx, data, press).await {
                            polls::vote_pressed(ctx, data, press).await;
                        }
                    }
                    // Сообщения: анти-спам, ответы ИИ, автоответы и опыт
                    if let serenity::FullEvent::Message { new_message } = event {
                        chat::on_message(ctx, data, new_message).await;
                    }
                    Ok(())
                })
            },
            ..Default::default()
        })
        .setup(move |ctx, ready, framework| {
            Box::pin(async move {
                let commands = &framework.options().commands;
                // Не получилось зарегистрировать команды — бот всё равно работает (со списком команд, что уже в Discord)
                if let Registration::Guild(guild) = registration {
                    match poise::builtins::register_in_guild(ctx, commands, serenity::GuildId::new(guild)).await {
                        Ok(()) => println!("Зарегистрировано слэш-команд на сервере {guild}: {}", commands.len()),
                        Err(error) => println!("⚠️ Не получилось зарегистрировать команды на сервере {guild}: {error}"),
                    }
                } else if registration == Registration::Global {
                    match register_globally(&token_for_setup, ready.application.id.get(), commands).await {
                        Ok(count) => println!("Зарегистрировано слэш-команд глобально: {count}"),
                        Err(error) => println!("⚠️ Не получилось зарегистрировать команды глобально: {error}"),
                    }
                } else {
                    println!("Команды не регистрировал: задай TEST_GUILD_ID (сервер для проверки) или REGISTER_GLOBAL=1.");
                }
                shop::start_expiry_loop(ctx.clone(), data.clone()); // сроки ролей и сверка покупок
                levels::start_voice_xp_loop(ctx.clone(), data.clone()); // опыт за голосовые каналы, раз в минуту
                rocket::start_cleanup(data.clone());
                wordstats::start_duel_loop(ctx.http.clone(), data.clone()); // итоги дуэлей в словах
                wordsummary::start_loop(ctx.clone(), data.clone()); // итоги дня ровно в московскую полночь
                backups::start_loop(data.clone(), backup_settings);
                #[cfg(feature="voice")]
                music_qol::start_idle_loop(ctx.clone(),data.clone());
                start_fast_loop(ctx.http.clone(), data.clone()); // напоминания и опросы
                daily::start_loop(ctx.http.clone(), data.clone()); // утренние и вечерние сообщения
                chat::start_chatter_loop(ctx.clone(), data.clone()); // случайные реплики по теме чата
                patchnotes::start_loop(ctx.http.clone(), data.clone());
                match miniapp {
                    Some((secret, listener)) => miniapp::start(ctx.clone(), data.clone(), secret, listener),
                    None => println!("Мини-приложение недоступно. Проверь DISCORD_CLIENT_SECRET, [miniapp] и адрес сервера."),
                }
                println!("yt-dlp запускается так: {}", data.ytdlp);
                println!("Бот запущен как {}", ready.user.name);
                Ok(data)
            })
        })
        .build();

    // Какие события бот получает от Discord: серверы, голосовые каналы (для музыки) и сообщения.
    // MESSAGE_CONTENT — текст сообщений (для ИИ, автоответов и анти-спама): включается в Developer Portal → Bot.
    let intents = serenity::GatewayIntents::GUILDS
        | serenity::GatewayIntents::GUILD_VOICE_STATES
        | serenity::GatewayIntents::GUILD_MESSAGES
        | serenity::GatewayIntents::MESSAGE_CONTENT;
    let builder = serenity::ClientBuilder::new(token, intents).framework(framework);
    #[cfg(feature = "voice")]
    let builder = builder.register_songbird();
    builder.await?.start().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn invalid_test_guild_never_falls_back_to_global_registration() {
        use super::{registration, Registration};
        assert_eq!(registration(None, Some("1")), Ok(Registration::Global));
        assert_eq!(registration(Some("42"), None), Ok(Registration::Guild(42)));
        assert_eq!(registration(None, None), Ok(Registration::Disabled));
        assert!(registration(Some("0"), None).is_err());
        assert!(registration(Some("broken"), Some("1")).is_err());
        assert!(registration(Some("42"), Some("1")).is_err());
    }

    #[test]
    fn registered_commands_have_one_activity_launcher_and_music_library() {
        let commands = super::bot_commands();
        let payload = serde_json::to_value(poise::builtins::create_application_commands(&commands)).unwrap();
        let list = payload.as_array().unwrap();
        let names: std::collections::BTreeSet<_> = list.iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(names.len(), list.len());
        let games = list.iter().find(|c| c["name"] == "games").unwrap();
        assert!(games["options"].as_array().is_none_or(Vec::is_empty));
        for retired in [
            "slovo", "wort", "words", "rocket", "slots", "roulette", "admin_money",
            "ask", "case", "cases", "pet_battle", "coinflip", "pets", "pet_sell", "pet_feed",
        ] {
            assert!(!names.contains(retired), "/{retired} is still in the guild payload");
        }
        assert!(!names.contains("welcome_channel"));
        assert!(names.contains("music_channel") && names.contains("status"));
        for active in ["blackjack", "pets_collect", "pet_trade", "forget", "ai_limits", "summary", "quiz"] {
            assert!(names.contains(active), "/{active} is missing from the guild payload");
        }
        #[cfg(feature = "voice")]
        {
            assert_eq!(names.len(), 68);
            assert!(names.contains("favorites") && names.contains("history"));
        }
        #[cfg(not(feature = "voice"))]
        {
            assert_eq!(names.len(), 47);
            assert!(!names.contains("favorites") && !names.contains("history"));
        }
    }

    /// Список команд для Discord собирается в JSON, который принимает массовое обновление (PUT).
    #[test]
    fn command_payload() {
        let commands = vec![super::economy::give_money(), super::admin::set_games_channel()];
        let payload = serde_json::to_value(poise::builtins::create_application_commands(&commands)).unwrap();
        let list = payload.as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["name"], "give_money");
        assert_eq!(list[0]["options"].as_array().unwrap().len(), 2);
        assert_eq!(list[1]["name"], "games_channel");
        assert_eq!(list[1]["default_member_permissions"], "32"); // Manage Server
    }

    #[test]
    fn global_payload_replaces_stale_commands_and_keeps_the_entry_point() {
        use serde_json::json;

        let commands = super::bot_commands();
        let guild_payload = poise::builtins::create_application_commands(&commands);
        let expected = serde_json::to_value(guild_payload).unwrap();
        let entry_point = json!({"id": "42", "type": 4, "name": "launch", "description": "Open the Activity", "handler": 2});
        let mut existing = ["ask", "case", "cases", "pet_battle", "coinflip", "pets", "pet_sell", "pet_feed"]
            .map(|name| json!({"type": 1, "name": name})).to_vec();
        existing.push(entry_point.clone());
        let (payload, count) = super::global_command_payload(&commands, existing).unwrap();
        assert_eq!(count, expected.as_array().unwrap().len());
        assert_eq!(payload.len(), count + 1);
        assert_eq!(&payload[..count], expected.as_array().unwrap());
        assert_eq!(payload[count], entry_point);
    }

    #[test]
    fn ytdlp_default() {
        let dir = std::env::temp_dir().join("musicbot-ytdlp-test");
        assert_eq!(super::default_ytdlp(&dir), "yt-dlp");
        assert_eq!(super::fix_ytdlp("/home/x/venv/bin/python"), "/home/x/venv/bin/python -m yt_dlp");
        assert_eq!(super::fix_ytdlp(" python3 "), "python3 -m yt_dlp");
        assert_eq!(super::fix_ytdlp("venv/bin/python -m yt_dlp"), "venv/bin/python -m yt_dlp");
        assert_eq!(super::fix_ytdlp("/usr/local/bin/yt-dlp"), "/usr/local/bin/yt-dlp");
        let quoted = super::fix_ytdlp(r#""some path/venv/bin/python""#);
        assert_eq!(super::ytdlp_parts(&quoted), ("some path/venv/bin/python", "-m yt_dlp"));
    }
}
