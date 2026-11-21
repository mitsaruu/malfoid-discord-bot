//! Мини-приложение для игр (Discord Activity). Discord показывает страницу miniapp/index.html внутри
//! себя; страницу и ответы на ходы отдаёт этот веб-сервер. Снаружи до него Discord достучится через туннель
//! (например, cloudflared) — см. README.

use crate::wordboard::{self, Player};
use crate::words::{self, Lang};
use crate::{rocket, Shared};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use poise::serenity_prelude as serenity;
use rand::distr::{Alphanumeric, SampleString};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SESSION_LIFETIME: Duration = Duration::from_secs(12 * 3600);
const SESSION_LIMIT: usize = 2048;
const AUTH_TTL: Duration = Duration::from_secs(15);
const DISCORD_TIMEOUT: Duration = Duration::from_secs(5);

/// Кто играет: выдаётся после входа через Discord, страница присылает «пропуск» с каждым запросом.
#[derive(Clone)]
struct Session {
    guild: u64,
    user: u64,
    name: String,
    face: String, // ссылка на аватарку
    channel: u64,
    parent: Option<u64>,
    created: Instant,
    checked: Instant,
    lang: Lang,
}

pub struct Miniapp {
    data: Shared,
    ctx: serenity::Context,
    client_secret: String,
    sessions: Mutex<HashMap<String, Session>>,
}

type App = Arc<Miniapp>;
type Reply = Result<Response, Response>;

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn json_error(text: &str, status: StatusCode) -> Response {
    (status, Json(json!({ "error": text }))).into_response()
}

/// Ошибка в мини-приложении не роняет бота: пишем её в консоль и отвечаем странице.
fn failed(error: impl std::fmt::Display) -> Response {
    println!("Мини-приложение: {error}");
    json_error("Что-то пошло не так. Ошибка записана в логи.", StatusCode::INTERNAL_SERVER_ERROR)
}

fn authorization_error() -> Response {
    json_error("Discord не подтвердил доступ к этому каналу. Открой игру заново в доступном канале.", StatusCode::FORBIDDEN)
}

fn discord_id(value: &str) -> Option<u64> {
    value.parse().ok().filter(|id| *id != 0)
}

impl Miniapp {
    /// Кто делает запрос: ищем пропуск из заголовка Authorization: Bearer <пропуск>.
    fn session(&self, headers: &HeaderMap) -> Result<(String, Session), Response> {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::to_string)
            .unwrap_or_default();
        match locked(&self.sessions).get(&token) {
            Some(session) if session.created.elapsed() < SESSION_LIFETIME => Ok((token, session.clone())),
            _ => Err(json_error("Сессия устарела — открой игру заново.", StatusCode::UNAUTHORIZED)),
        }
    }

    async fn authorized(&self, headers: &HeaderMap, command: Option<&str>, fresh: bool) -> Result<(String, Session), Response> {
        let (pass, mut session) = self.session(headers)?;
        if fresh || session.checked.elapsed() >= AUTH_TTL {
            let (member, parent) = authorize_channel(&self.ctx, session.guild, session.user, session.channel).await?;
            session.name = member.display_name().to_string();
            session.face = member.face();
            session.parent = parent;
            session.checked = Instant::now();
            let mut sessions = locked(&self.sessions);
            let stored = sessions.get_mut(&pass).ok_or_else(authorization_error)?;
            stored.checked = session.checked;
            stored.parent = parent;
            stored.name.clone_from(&session.name);
            stored.face.clone_from(&session.face);
            session.lang = stored.lang;
        }
        if let Some(command) = command {
            check_game_channel(&self.data, &session, command)?;
        }
        Ok((pass, session))
    }

    fn state_of(&self, session: &Session, game: &words::Game) -> Result<Value, Response> {
        let balance = words::balance(&self.data, session.guild, session.user).map_err(failed)?;
        Ok(words::game_state(&self.data, game, balance))
    }

    /// Обновляет картинку в чате, не задерживая ответ странице.
    fn refresh_board(self: &Arc<Self>, session: &Session, lang: Lang) {
        let (app, player) = (self.clone(), Player { id: session.user, name: session.name.clone(), face: session.face.clone() });
        let guild = session.guild;
        let channel = session.channel;
        tokio::spawn(async move {
            wordboard::update(&app.ctx, &app.data, guild, serenity::ChannelId::new(channel), player, lang).await;
        });
    }

}

async fn index(State(app): State<App>) -> Response {
    file(&app, "index.html", "text/html; charset=utf-8").await
}

/// Разрешённые файлы интерфейса.
async fn static_file(State(app): State<App>, Path(name): Path<String>) -> Response {
    match name.as_str() {
        "style.css" | "rocket.css" | "casino.css" =>
            file(&app, &name, "text/css; charset=utf-8").await,
        "app.js" | "rocket.js" | "mines.js" | "casino_slots.js" | "poker.js" | "table_blackjack.js" =>
            file(&app, &name, "text/javascript; charset=utf-8").await,
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn file(app: &Miniapp, name: &str, content_type: &'static str) -> Response {
    match tokio::fs::read(app.data.dir.join("miniapp").join(name)).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache")], bytes).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

fn application_id(app: &Miniapp) -> String {
    app.ctx.http.application_id().map(|id| id.to_string()).unwrap_or_default()
}

async fn config(State(app): State<App>) -> Response {
    Json(json!({ "client_id": application_id(&app) })).into_response()
}

fn text_field(body: &Value, key: &str) -> String {
    match body.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

// The URL's guild/channel fields are client input. Check the authenticated
// player's current roles and channel access before binding them to a session.
async fn authorize_channel(ctx: &serenity::Context, guild: u64, user: u64, channel: u64) -> Result<(serenity::Member, Option<u64>), Response> {
    async fn access(ctx: &serenity::Context, guild: u64, user: u64, channel: u64) -> Option<(serenity::Member, Option<u64>)> {
        let guild = serenity::GuildId::new(guild);
        let user = serenity::UserId::new(user);
        let id = serenity::ChannelId::new(channel);
        let (member, found) = tokio::try_join!(ctx.http.get_member(guild, user), ctx.http.get_channel(id)).ok()?;
        let found = found.guild().filter(|channel| channel.guild_id == guild)?;
        let thread = matches!(found.kind, serenity::ChannelType::PublicThread | serenity::ChannelType::PrivateThread | serenity::ChannelType::NewsThread);
        let permissions_channel = if thread {
            ctx.http.get_channel(found.parent_id?).await.ok()?.guild().filter(|channel| channel.guild_id == guild)?
        } else {
            found.clone()
        };
        let permissions = ctx.cache.guild(guild)?.user_permissions_in(&permissions_channel, &member);
        if !permissions.view_channel() {
            return None;
        }
        if found.kind == serenity::ChannelType::PrivateThread && !permissions.manage_threads() {
            id.get_thread_member(&ctx.http, user, false).await.ok()?;
        }
        Some((member, found.parent_id.filter(|_| thread).map(|id| id.get())))
    }
    match tokio::time::timeout(DISCORD_TIMEOUT, access(ctx, guild, user, channel)).await {
        Ok(Some(authorized)) => Ok(authorized),
        _ => Err(authorization_error()),
    }
}

fn check_game_channel(data: &crate::Data, session: &Session, command: &str) -> Result<(), Response> {
    let required = data.db.games_channel(session.guild).map_err(failed)?;
    if crate::admin::allowed_here(&data.games_commands, command, required, session.channel, session.parent) {
        return Ok(());
    }
    Err(json_error("Эта игра доступна только в игровом канале сервера.", StatusCode::FORBIDDEN))
}

/// Вход: страница получила от Discord одноразовый код — меняем его на токен и узнаём, кто это.
async fn token(State(app): State<App>, Json(body): Json<Value>) -> Reply {
    let (code, guild_id) = (text_field(&body, "code"), text_field(&body, "guild_id"));
    let client_id = application_id(&app);
    let exchange = app
        .data
        .web
        .post("https://discord.com/api/v10/oauth2/token")
        .form(&[("client_id", client_id.as_str()), ("client_secret", app.client_secret.as_str()),
                ("grant_type", "authorization_code"), ("code", code.as_str())])
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(failed)?;
    if !exchange.status().is_success() {
        println!("Мини-приложение: Discord не принял код входа: {}", exchange.status());
        return Err(json_error("Discord не подтвердил вход. Проверь DISCORD_CLIENT_SECRET.", StatusCode::UNAUTHORIZED));
    }
    let tokens: Value = serde_json::from_str(&exchange.text().await.map_err(failed)?).map_err(failed)?;
    let access_token = text_field(&tokens, "access_token");
    let who = app
        .data
        .web
        .get("https://discord.com/api/v10/users/@me")
        .bearer_auth(&access_token)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(failed)?;
    if !who.status().is_success() {
        return Err(json_error("Не получилось узнать, кто ты.", StatusCode::UNAUTHORIZED));
    }
    let who: Value = serde_json::from_str(&who.text().await.map_err(failed)?).map_err(failed)?;
    let user = discord_id(&text_field(&who, "id")).ok_or_else(authorization_error)?;

    let guild = discord_id(&guild_id).filter(|id| app.ctx.cache.guild(serenity::GuildId::new(*id)).is_some());
    let Some(guild) = guild else {
        return Err(json_error("Открой игру на сервере, где есть бот (не в личке).", StatusCode::FORBIDDEN));
    };
    let channel = discord_id(&text_field(&body, "channel_id")).ok_or_else(authorization_error)?;
    let (member, parent) = authorize_channel(&app.ctx, guild, user, channel).await?;
    let name = member.display_name().to_string();
    let now = Instant::now();
    let lang = crate::wordsummary::preferred_lang(&app.data, guild, user);
    let session = Session {
        guild, user, name: name.clone(), face: member.face(), channel, parent, created: now, checked: now, lang,
    };
    let wants_rocket = app.data.rocket.take_open(guild, user);
    let (game, state) = if wants_rocket {
        check_game_channel(&app.data, &session, "rocket")?;
        ("rocket", Value::Null)
    } else {
        check_game_channel(&app.data, &session, "words")?;
        let game = words::daily_game(&app.data, guild, user, lang).map_err(failed)?;
        let state = app.state_of(&session, &game)?;
        if !game.finished() {
            app.refresh_board(&session, lang);
        }
        ("words", state)
    };
    let pass = Alphanumeric.sample_string(&mut rand::rng(), 32);
    {
        let mut sessions = locked(&app.sessions);
        sessions.retain(|_, old| old.created.elapsed() < SESSION_LIFETIME);
        if sessions.len() >= SESSION_LIMIT {
            if let Some(oldest) = sessions.iter().min_by_key(|(_, old)| old.created).map(|(pass, _)| pass.clone()) {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(pass.clone(), session);
    }
    Ok(Json(json!({ "access_token": access_token, "session": pass, "name": name, "game": game, "state": state })).into_response())
}

async fn rocket_state(State(app): State<App>, headers: HeaderMap) -> Reply {
    let (_, session) = app.authorized(&headers, None, false).await?;
    Ok(Json(rocket::read(&app.data, (session.guild, session.user)).map_err(failed)?).into_response())
}

async fn rocket_start(State(app): State<App>, headers: HeaderMap, Json(body): Json<Value>) -> Reply {
    let bad = || json_error("Проверь ставку и коэффициент автовывода.", StatusCode::BAD_REQUEST);
    let bet = body.get("bet").and_then(Value::as_str).filter(|text| text.len() <= 32).ok_or_else(bad)?;
    let auto = match body.get("auto") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if value.len() <= 12 => Some(value.as_str()),
        _ => return Err(bad()),
    };
    let expected = match body.get("expected_id") {
        Some(Value::Null) => None,
        Some(Value::String(id)) => Some(id.parse::<u64>().map_err(|_| bad())?),
        _ => return Err(bad()),
    };
    let (_, session) = app.authorized(&headers, Some("rocket"), true).await?;
    let state = rocket::start(&app.data, (session.guild, session.user), &session.name, bet, auto, expected)
        .map_err(failed)?.map_err(|error| json_error(&error, StatusCode::CONFLICT))?;
    Ok(Json(state).into_response())
}

async fn rocket_cashout(State(app): State<App>, headers: HeaderMap, Json(body): Json<Value>) -> Reply {
    let (_, session) = app.authorized(&headers, None, false).await?;
    let id = body.get("id").and_then(Value::as_str).and_then(|id| id.parse::<u64>().ok())
        .ok_or_else(|| json_error("Нужен номер раунда.", StatusCode::BAD_REQUEST))?;
    let state = rocket::cash_out(&app.data, (session.guild, session.user), id)
        .map_err(failed)?.map_err(|error| json_error(&error, StatusCode::CONFLICT))?;
    Ok(Json(state).into_response())
}

async fn casino_action(
    State(app): State<App>,
    headers: HeaderMap,
    Path((game, action)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Reply {
    let rule = match game.as_str() {
        "mines" | "poker" | "slots" | "blackjack" => game.as_str(),
        _ => return Err(StatusCode::NOT_FOUND.into_response()),
    };
    if action.len() > 24 || !body.is_object() {
        return Err(json_error("Некорректное действие.", StatusCode::BAD_REQUEST));
    }
    let (_, session) = app.authorized(&headers, None, false).await?;
    // Новые игры появились после настройки части серверов, поэтому применяем
    // игровой канал к ним и при отсутствии их имён в старом конфиге.
    if matches!(rule, "poker" | "mines") {
        let channel = app.data.db.games_channel(session.guild).map_err(failed)?;
        if channel.is_some_and(|id| id != session.channel && Some(id) != session.parent) {
            return Err(json_error("Эта игра доступна только в игровом канале сервера.", StatusCode::FORBIDDEN));
        }
    } else {
        check_game_channel(&app.data, &session, rule)?;
    }
    let data = app.data.clone();
    let result = tokio::task::spawn_blocking(move || {
        let (guild, user, name) = (session.guild, session.user, session.name.as_str());
        match game.as_str() {
            "poker" => crate::poker::handle(&data, guild, user, name, &action, &body),
            "blackjack" => crate::table_blackjack::handle(&data, guild, user, name, &action, &body),
            _ => crate::casino_solo::handle(&data, guild, user, name, &game, &action, &body),
        }
    }).await.map_err(failed)?.map_err(failed)?;
    let state = result.map_err(|error| json_error(&error, StatusCode::CONFLICT))?;
    Ok(Json(state).into_response())
}

fn start_game_cleanup(data: Shared) {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(1));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            let data = data.clone();
            if let Err(error) = tokio::task::spawn_blocking(move || {
                crate::poker::sweep(&data);
                crate::table_blackjack::sweep(&data);
                crate::casino_solo::sweep(&data);
            }).await {
                eprintln!("Mini App: {error}");
            }
        }
    });
}

async fn state(State(app): State<App>, headers: HeaderMap) -> Reply {
    let (_, session) = app.authorized(&headers, Some("words"), false).await?;
    let game = words::daily_game(&app.data, session.guild, session.user, session.lang).map_err(failed)?;
    Ok(Json(json!({ "state": app.state_of(&session, &game)? })).into_response())
}

async fn guess(State(app): State<App>, headers: HeaderMap, Json(body): Json<Value>) -> Reply {
    let (_, session) = app.authorized(&headers, Some("words"), true).await?;
    let word = text_field(&body, "word");
    let expected_day = text_field(&body, "day");
    let expected_day = (!expected_day.is_empty()).then_some(expected_day.as_str());
    let (game, problem) = words::make_guess_for_day(&app.data, session.guild, session.user, &session.name, &word, session.lang, expected_day)
        .await
        .map_err(failed)?;
    if problem.is_none() {
        app.refresh_board(&session, game.lang); // обновляем картинку в чате
        if game.finished() {
            // слово дня доиграно — вдруг у него дуэль на это слово
            let app = app.clone();
            tokio::spawn(async move {
                if let Err(error) = crate::wordstats::resolve_duels(&app.ctx.http, &app.data).await {
                    println!("Дуэли в словах: {error}");
                }
            });
        }
    }
    Ok(Json(json!({ "state": app.state_of(&session, &game)?, "error": problem })).into_response())
}

/// Открыть слово дня; с полем lang — переключиться на другую игру прямо в мини-приложении.
async fn new_game(State(app): State<App>, headers: HeaderMap, Json(body): Json<Value>) -> Reply {
    let (pass, mut session) = app.authorized(&headers, Some("words"), true).await?;
    let switched = Lang::from_key(&text_field(&body, "lang"));
    if let Some(lang) = switched {
        session.lang = lang;
        if let Some(stored) = locked(&app.sessions).get_mut(&pass) {
            stored.lang = lang;
        }
        crate::wordsummary::set_lang(&app.data, session.guild, session.user, lang).map_err(failed)?;
    }
    let game = words::daily_game(&app.data, session.guild, session.user, session.lang).map_err(failed)?;
    if switched.is_some() && !game.finished() {
        app.refresh_board(&session, game.lang);
    }
    Ok(Json(json!({ "state": app.state_of(&session, &game)? })).into_response())
}

/// Кто сегодня играет в слово дня этой игры на сервере: имя и цвета догадок (без букв) — для панели слева
/// сверху. Страница спрашивает раз в пару секунд, так что видно, как другие подбирают слова.
async fn players(State(app): State<App>, headers: HeaderMap) -> Reply {
    let (_, session) = app.authorized(&headers, Some("words"), false).await?;
    let lang = session.lang;
    let day = words::today(&app.data, lang);
    let games = words::player_games(&app.data, session.guild, session.user, lang, &day).map_err(failed)?;
    let mut players = Vec::new();
    for (user, game) in games {
        let me = user == session.user;
        let name = if me {
            session.name.clone()
        } else {
            crate::wordsummary::profile(&app.data.db, session.guild, user).map_err(failed)?
                .map(|(name, _)| name).unwrap_or_else(|| "Игрок".to_string())
        };
        let marks: Vec<Vec<&str>> = game.rows().iter().map(|row| row.iter().map(|mark| mark.as_str()).collect()).collect();
        players.push(json!({
            "id": user.to_string(), "name": name, "me": me, "marks": marks,
            "won": game.won(), "finished": game.finished(),
        }));
    }
    Ok(Json(json!({ "players": players, "guild": session.guild.to_string(), "day": day, "lang": lang.key() })).into_response())
}

/// Аватарка участника из Discord (картинки с чужих сайтов внутри Discord-приложения не грузятся,
/// поэтому отдаём их со своего сервера). Адрес: /api/avatar/<сервер>/<участник>.png
async fn avatar(State(app): State<App>, Path((guild, file)): Path<(u64, String)>, headers: HeaderMap) -> Reply {
    let missing = || StatusCode::NOT_FOUND.into_response();
    let user = file.strip_suffix(".png").and_then(discord_id).ok_or_else(missing)?;
    let (_, session) = app.authorized(&headers, Some("words"), false).await?;
    if guild != session.guild {
        return Err(missing());
    }
    let day = words::today(&app.data, session.lang);
    if app.data.db.word_row(session.lang.table(), guild, user, &day).map_err(failed)?.is_none() {
        return Err(missing());
    }
    let face = if user == session.user {
        session.face
    } else {
        crate::wordsummary::profile(&app.data.db, guild, user).map_err(failed)?.ok_or_else(missing)?.1
    };
    let bytes = wordboard::cached_avatar(&app.data, &face).await.ok_or_else(missing)?;
    Ok(([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "private, no-store")], bytes).into_response())
}

pub fn router(app: App) -> axum::Router {
    axum::Router::new()
        .route("/", get(index))
        .route("/static/{name}", get(static_file))
        .route("/api/config", get(config))
        .route("/api/token", post(token))
        .route("/api/state", get(state))
        .route("/api/guess", post(guess))
        .route("/api/new", post(new_game))
        .route("/api/players", get(players))
        .route("/api/rocket/state", get(rocket_state))
        .route("/api/rocket/start", post(rocket_start))
        .route("/api/rocket/cashout", post(rocket_cashout))
        .route("/api/games/{game}/{action}", post(casino_action).layer(DefaultBodyLimit::max(2048)))
        .route("/api/avatar/{guild}/{file}", get(avatar))
        .with_state(app)
}

/// Запускает веб-сервер мини-приложения в фоне.
pub fn start(ctx: serenity::Context, data: Shared, client_secret: String, listener: tokio::net::TcpListener) {
    start_game_cleanup(data.clone());
    let app = Arc::new(Miniapp { data: data.clone(), ctx, client_secret, sessions: Default::default() });
    tokio::spawn(async move {
        if let Ok(address) = listener.local_addr() {
            println!("Мини-приложение работает на http://{address} — пробрось его туннелем (см. README)");
        }
        if let Err(error) = axum::serve(listener, router(app)).await {
            println!("Мини-приложение остановилось: {error}");
        }
        data.miniapp_ready.store(false, std::sync::atomic::Ordering::Relaxed);
    });
}
