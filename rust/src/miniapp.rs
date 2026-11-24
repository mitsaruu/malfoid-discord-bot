//! Мини-приложение для игр (Discord Activity). Discord показывает страницу miniapp/index.html внутри
//! себя; страницу и ответы на ходы отдаёт этот веб-сервер. Снаружи до него Discord достучится через туннель
//! (например, cloudflared) — см. README.

use crate::pets::{self, Case};
use crate::wordboard::{self, Player};
use crate::words::{self, Lang};
use crate::{ai_activity, ai_math, rocket, Shared};
use axum::extract::{DefaultBodyLimit, FromRequest, Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use base64::Engine;
use poise::serenity_prelude as serenity;
use rand::distr::{Alphanumeric, SampleString};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
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

impl Session {
    fn wallet(&self, data: &crate::Data) -> crate::db::Wallet<'_> {
        crate::db::Wallet {
            guild: self.guild, user: self.user, name: &self.name, start: data.economy.start_balance,
        }
    }
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
        "style.css" | "ai-chat.css" | "cases.css" | "rocket.css" | "casino.css" | "roulette.css" | "coinflip.css" | "durak.css" | "durak_online.css" =>
            file(&app, &name, "text/css; charset=utf-8").await,
        "app.js" | "ai-chat.js" | "audio.js" | "game_avatars.js" | "cases.js" | "cases-inventory.js" | "cases-upgrade.js" | "cases-contract.js" | "cases-leaderboard.js" | "rocket.js" | "mines.js" | "casino_slots.js" | "poker.js" | "table_blackjack.js" | "roulette.js" | "coinflip.js" | "durak.js" | "durak_online.js" =>
            file(&app, &name, "text/javascript; charset=utf-8").await,
        "lobby-games.webp" | "pet-cases.webp" | "secret-pet-silhouette.webp" => file(&app, &name, "image/webp").await,
        "menu-ambient.mp3" | "case-opening.mp3" | "case-secret.mp3" =>
            file(&app, &name, "audio/mpeg").await,
        "case-secret-explosion.mp4" => file(&app, &name, "video/mp4").await,
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn pet_art(State(app): State<App>, Path(name): Path<String>) -> Response {
    let Some(pet) = name.strip_suffix(".webp").and_then(pets::find_species).filter(|pet| pet.rarity != "secret") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    file(&app, &format!("pets/{}.webp", pet.key), "image/webp").await
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
    let mut session = Session {
        guild, user, name: name.clone(), face: member.face(), channel, parent, created: now, checked: now, lang,
    };
    check_game_channel(&app.data, &session, "games")?;
    let game = app.data.boards.take_word_link(guild, user, channel).await.map(|lang| {
        session.lang = lang;
        "words"
    });
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
    Ok(Json(json!({
        "access_token": access_token, "session": pass, "name": name, "user_id": user.to_string(), "game": game,
    })).into_response())
}

fn case_pet(pet: &pets::Species) -> Value {
    json!({
        "key": pet.key, "name": pet.name, "emoji": pet.emoji,
        "rarity": pet.rarity, "rarity_name": pets::rarity_of(pet).name,
    })
}

fn public_case_pet(pet: &pets::Species) -> Value {
    if pet.rarity == "secret" {
        json!({ "name": "Секретный питомец", "rarity": "secret", "emoji": "?" })
    } else {
        case_pet(pet)
    }
}

fn inventory_full(data: &crate::Data, session: &Session) -> Result<bool, Response> {
    let count = crate::db::Database::pet_count_on(&data.db.lock(), session.guild, session.user).map_err(failed)?;
    Ok(count >= data.pets.max_pets)
}

async fn cases_catalogue(State(app): State<App>, headers: HeaderMap) -> Reply {
    let (_, session) = app.authorized(&headers, None, false).await?;
    let balance = words::balance(&app.data, session.guild, session.user).map_err(failed)?;
    let secret_unlocked = pets::has_opened_stellochka(&app.data.db, session.guild, session.user).map_err(failed)?;
    let secret_sprite = if secret_unlocked {
        pets::secret_pet_art(&app.data).await.map(|art| base64::engine::general_purpose::STANDARD.encode(art))
    } else {
        None
    };
    let cases = Case::ALL.map(|case| {
        let price = app.data.pets.case_prices[case as usize];
        let pets: Vec<_> = case.species().map(case_pet).collect();
        json!({
            "id": case.key(),
            "name": case.title(),
            "price": crate::text::number(price),
            "cost": price.to_string(),
            "species_count": pets.len(),
            "pets": pets,
        })
    });
    Ok(([(header::CACHE_CONTROL, "private, no-store")], Json(json!({
        "name": session.name, "balance": crate::text::number(balance), "cases": cases,
        "secret_unlocked": secret_unlocked, "secret_sprite": secret_sprite,
        "inventory_full": inventory_full(&app.data, &session)?,
    }))).into_response())
}

async fn cases_open(State(app): State<App>, headers: HeaderMap, Json(body): Json<Value>) -> Reply {
    let bad = || json_error("Некорректный запрос открытия кейса.", StatusCode::BAD_REQUEST);
    let kind = body.get("case").and_then(Value::as_str)
        .and_then(Case::from_key).ok_or_else(bad)?;
    let id = body.get("request_id").and_then(Value::as_str)
        .filter(|id| id.len() == 36 && id.bytes().all(|byte| byte.is_ascii_hexdigit() || byte == b'-'))
        .ok_or_else(bad)?;
    let price = body.get("expected_price").and_then(Value::as_str).and_then(|price| price.parse::<i64>().ok())
        .filter(|price| *price > 0).ok_or_else(bad)?;

    let (_, session) = app.authorized(&headers, Some("case"), true).await?;
    let wallet = session.wallet(&app.data);
    let opening = pets::buy_case(&app.data.db, wallet, &session.face, &app.data.pets, kind,
        Some(pets::CaseRequest { id, price }))
        .map_err(failed)?;
    let full = inventory_full(&app.data, &session)?;
    let (pet, balance) = opening.map_err(|message| json_error(
        if full { "у вас полный склад, продайте лишних питомцев" } else { &message },
        StatusCode::CONFLICT,
    ))?;
    let mut response = json!({
        "pet": case_pet(pet),
        "balance": crate::text::number(balance),
        "income": format!("{}/ч", crate::text::money_text(pets::income(pet, 1))),
        "sale_price": crate::text::money_text(pets::value(pet, 1)),
        "inventory_full": full,
    });
    if pet.rarity == "secret" {
        if let Some(art) = pets::secret_pet_art(&app.data).await {
            response["sprite"] = json!(base64::engine::general_purpose::STANDARD.encode(art));
        }
    }
    Ok(([(header::CACHE_CONTROL, "private, no-store")], Json(response)).into_response())
}

async fn cases_inventory(State(app): State<App>, headers: HeaderMap) -> Reply {
    let (_, session) = app.authorized(&headers, None, false).await?;
    let balance = words::balance(&app.data, session.guild, session.user).map_err(failed)?;
    let owned = app.data.db.user_pets(session.guild, session.user).map_err(failed)?;
    let full = owned.len() as i64 >= app.data.pets.max_pets;
    let owned = pets::sorted_pets(owned);
    let hourly_income: i128 = owned.iter().filter(|(pet, _)| pet.inactive_since.is_none())
        .map(|(pet, species)| i128::from(pets::income(species, pet.level))).sum();
    let secret_sprite = if owned.iter().any(|(_, species)| species.rarity == "secret") {
        pets::secret_pet_art(&app.data).await.map(|art| base64::engine::general_purpose::STANDARD.encode(art))
    } else {
        None
    };
    let animals: Vec<_> = owned.iter().map(|(pet, species)| {
        let level = pet.level.max(1);
        let active = pet.inactive_since.is_none();
        let income_rate = pets::income(species, level);
        let upgrade_price = (level < app.data.pets.max_level)
            .then(|| crate::text::money_text(pets::feed_cost(species, level)));
        json!({
            "id": pet.id.to_string(),
            "pet": case_pet(species),
            "level": level.to_string(),
            "active": active,
            "income": format!("{}/ч", crate::text::money_text(if active { income_rate } else { 0 })),
            "income_rate": format!("{}/ч", crate::text::money_text(income_rate)),
            "upgrade_price": upgrade_price,
            "sale_price": pets::value(species, level).to_string(),
        })
    }).collect();
    let targets: Vec<_> = pets::SPECIES.iter().filter(|species| species.rarity != "secret").map(|species| json!({
        "pet": case_pet(species), "sale_price": pets::value(species, 1).to_string(),
        "income": format!("{}/ч", crate::text::money_text(pets::income(species, 1))),
    })).collect();
    Ok(([(header::CACHE_CONTROL, "private, no-store")], Json(json!({
        "pets": animals, "balance": crate::text::number(balance), "secret_sprite": secret_sprite,
        "hourly_income": hourly_income.to_string(),
        "inventory_full": full, "capacity": app.data.pets.max_pets, "active_limit": pets::ACTIVE_PET_LIMIT,
        "upgrade_targets": targets,
    }))).into_response())
}

async fn cases_leaderboard(State(app): State<App>, headers: HeaderMap) -> Reply {
    let (_, session) = app.authorized(&headers, None, false).await?;
    let players = pets::player_leaderboard(&app.data.db, session.wallet(&app.data)).map_err(failed)?;
    let players: Vec<_> = players.into_iter().map(|player| {
        let best_pets: Vec<_> = player.best_pets.into_iter().map(|pet| {
            let mut entry = public_case_pet(pet.species);
            entry["level"] = json!(pet.level);
            entry
        }).collect();
        let (chat_level, _, _) = crate::levels::level_info(player.xp);
        json!({
            "id": player.user.to_string(), "name": player.name, "me": player.user == session.user,
            "xp": player.xp.to_string(), "chat_level": chat_level,
            "income": player.hourly_income.to_string(), "balance": player.balance.to_string(), "pets": best_pets,
        })
    }).collect();
    Ok(([(header::CACHE_CONTROL, "private, no-store")], Json(json!({ "players": players }))).into_response())
}

async fn cases_inventory_move(State(app): State<App>, headers: HeaderMap, Json(body): Json<Value>) -> Reply {
    let bad = || json_error("Выбери питомца и группу.", StatusCode::BAD_REQUEST);
    let id = body.get("id").and_then(positive_integer).ok_or_else(bad)?;
    let active = body.get("active").and_then(Value::as_bool).ok_or_else(bad)?;
    let replace = body.get("replace").map(|value| positive_integer(value).ok_or_else(bad)).transpose()?;
    let (_, session) = app.authorized(&headers, Some("pets"), true).await?;
    pets::move_pet(&app.data.db, session.wallet(&app.data), id, active, replace, crate::db::now())
        .map_err(failed)?.map_err(|message| json_error(message, StatusCode::CONFLICT))?;
    Ok(Json(json!({ "moved": true })).into_response())
}

async fn cases_pet_upgrade(State(app): State<App>, headers: HeaderMap, Json(body): Json<Value>) -> Reply {
    let bad = || json_error("Выбери питомцев для ставки и цель.", StatusCode::BAD_REQUEST);
    let entries = match body.get("stakes") {
        Some(stakes) => stakes.as_array().ok_or_else(bad)?.as_slice(),
        None => std::slice::from_ref(&body),
    };
    let stakes = pet_stake_selection(entries).ok_or_else(bad)?;
    let target = body.get("target").and_then(Value::as_str).and_then(pets::find_species).ok_or_else(bad)?;
    let (_, session) = app.authorized(&headers, Some("case"), true).await?;
    let result = pets::upgrade_pet(&app.data.db, session.wallet(&app.data), &session.face, &stakes, target,
        app.data.pets.collect_cap_hours, || rand::random_range(0..pets::UPGRADE_SCALE))
        .map_err(failed)?.map_err(|message| json_error(message, StatusCode::CONFLICT))?;
    Ok(Json(json!({
        "won": result.won, "chance": result.chance, "roll": result.roll, "pet": case_pet(result.target),
        "balance": crate::text::number(result.balance), "inventory_full": inventory_full(&app.data, &session)?,
    })).into_response())
}

fn pet_stake_selection(entries: &[Value]) -> Option<BTreeMap<i64, i64>> {
    if entries.is_empty() || entries.len() > pets::MAX_PET_LIMIT {
        return None;
    }
    let stakes = entries.iter().map(|entry| Some((
        positive_integer(entry.get("id")?)?, positive_integer(entry.get("level")?)?,
    ))).collect::<Option<BTreeMap<_, _>>>()?;
    (stakes.len() == entries.len()).then_some(stakes)
}

fn contract_selection(body: &Value) -> Result<BTreeMap<i64, i64>, Response> {
    body.get("stakes").and_then(Value::as_array).and_then(|entries| pet_stake_selection(entries))
        .ok_or_else(|| json_error("Выбери питомцев для контракта.", StatusCode::BAD_REQUEST))
}

fn contract_terms(quote: pets::ContractQuote) -> Value {
    let (min, max) = quote.range;
    json!({
        "stake": quote.stake.to_string(), "range": [min.to_string(), max.to_string()],
    })
}

async fn cases_contract_quote(State(app): State<App>, headers: HeaderMap, Json(body): Json<Value>) -> Reply {
    let stakes = contract_selection(&body)?;
    let (_, session) = app.authorized(&headers, Some("case"), false).await?;
    let quote = pets::contract_quote(&app.data.db, session.wallet(&app.data), &stakes, app.data.pets.max_level)
        .map_err(failed)?.map_err(|message| json_error(&message, StatusCode::CONFLICT))?;
    Ok(([(header::CACHE_CONTROL, "private, no-store")], Json(contract_terms(quote))).into_response())
}

async fn cases_contract_sign(State(app): State<App>, headers: HeaderMap, Json(body): Json<Value>) -> Reply {
    let stakes = contract_selection(&body)?;
    let bad = || json_error("Обнови расчёт контракта.", StatusCode::BAD_REQUEST);
    let expected = body.get("expected").ok_or_else(bad)?;
    let stake = expected.get("stake").and_then(positive_integer).ok_or_else(bad)?;
    let range = expected.get("range").and_then(Value::as_array)
        .filter(|range| range.len() == 2).ok_or_else(bad)?;
    let min = positive_integer(&range[0]).ok_or_else(bad)?;
    let max = positive_integer(&range[1]).ok_or_else(bad)?;
    let expected = pets::ContractQuote { stake, range: (min, max) };
    let (_, session) = app.authorized(&headers, Some("case"), true).await?;
    let result = pets::sign_contract(&app.data.db, session.wallet(&app.data), &stakes, &expected, &app.data.pets,
        |limit| rand::random_range(0..limit))
        .map_err(failed)?.map_err(|message| json_error(&message, StatusCode::CONFLICT))?;
    let prize = result.prize;
    Ok(Json(json!({
        "pet": case_pet(prize.species), "level": prize.level.to_string(),
        "income": format!("{}/ч", crate::text::money_text(pets::income(prize.species, prize.level))),
        "price": prize.price().to_string(), "stake": result.stake.to_string(), "dearer": result.dearer,
        "balance": crate::text::number(result.balance), "inventory_full": inventory_full(&app.data, &session)?,
    })).into_response())
}

fn positive_integer(value: &Value) -> Option<i64> {
    value.as_str()?.parse::<i64>().ok().filter(|number| *number > 0)
}

async fn cases_inventory_collect(State(app): State<App>, headers: HeaderMap) -> Reply {
    let (_, session) = app.authorized(&headers, Some("pets_collect"), true).await?;
    let (total, balance) = pets::collect_pets(
        &app.data.db, session.wallet(&app.data), app.data.pets.collect_cap_hours, crate::db::now(),
    ).map_err(failed)?.map_err(|message| json_error(message, StatusCode::CONFLICT))?;
    Ok(Json(json!({
        "payout": crate::text::money_text(total),
        "balance": crate::text::number(balance),
    })).into_response())
}

async fn cases_inventory_upgrade(State(app): State<App>, headers: HeaderMap, Json(body): Json<Value>) -> Reply {
    let bad = || json_error("Выбери питомца и актуальный уровень.", StatusCode::BAD_REQUEST);
    let id = body.get("id").and_then(positive_integer).ok_or_else(bad)?;
    let level = body.get("level").and_then(positive_integer).ok_or_else(bad)?;
    let (_, session) = app.authorized(&headers, Some("pet_feed"), true).await?;
    let result = pets::feed_owned_pet(&app.data.db, session.wallet(&app.data), id, &app.data.pets, Some(level))
        .map_err(failed)?.map_err(|message| json_error(&message, StatusCode::CONFLICT))?;
    Ok(Json(json!({
        "name": result.species.name,
        "level": result.pet.level.to_string(),
        "balance": crate::text::number(result.balance),
    })).into_response())
}

async fn cases_inventory_sell(State(app): State<App>, headers: HeaderMap, Json(body): Json<Value>) -> Reply {
    let bad = || json_error("Выбери питомцев для продажи.", StatusCode::BAD_REQUEST);
    let list = body.get("ids").and_then(Value::as_array).filter(|list| !list.is_empty()).ok_or_else(bad)?;
    let ids = list.iter().map(positive_integer).collect::<Option<BTreeSet<_>>>().ok_or_else(bad)?;
    if ids.len() != list.len() {
        return Err(bad());
    }
    let (_, session) = app.authorized(&headers, Some("pet_sell"), true).await?;
    let sale = pets::sell_inventory_pets(&app.data.db, session.wallet(&app.data), &ids,
        app.data.pets.collect_cap_hours, crate::db::now)
        .map_err(failed)?.map_err(|message| json_error(message, StatusCode::CONFLICT))?;
    Ok(Json(json!({
        "payout": crate::text::money_text(sale.payout), "balance": crate::text::number(sale.balance),
        "inventory_full": inventory_full(&app.data, &session)?,
    })).into_response())
}

async fn cases_avatar(State(app): State<App>, headers: HeaderMap) -> Reply {
    let (_, session) = app.authorized(&headers, None, false).await?;
    case_avatar_image(&app, &session.face).await
}

async fn cases_history(State(app): State<App>, headers: HeaderMap) -> Reply {
    let (_, session) = app.authorized(&headers, None, false).await?;
    let openings = pets::case_history(&app.data.db, session.guild).map_err(failed)?;
    let openings: Vec<_> = openings.into_iter().filter_map(|opening| {
        let pet = pets::find_species(&opening.species)?;
        let mut event = json!({
            "id": opening.id.to_string(),
            "name": opening.name,
            "me": opening.user == session.user,
            "case_key": opening.kind.map(Case::key),
            "case_name": opening.kind.map(Case::title),
            "pet": public_case_pet(pet),
        });
        if let Some(upgrade) = opening.upgrade {
            let source = upgrade.most_expensive_stake().map(|(species, level)| {
                let mut pet = public_case_pet(species);
                pet["level"] = json!(level);
                pet
            });
            event["upgrade"] = json!({ "source": source, "won": upgrade.won });
        }
        Some(event)
    }).collect();
    Ok(([(header::CACHE_CONTROL, "private, no-store")], Json(json!({ "openings": openings }))).into_response())
}

async fn cases_history_avatar(State(app): State<App>, Path(file): Path<String>, headers: HeaderMap) -> Reply {
    let missing = || StatusCode::NOT_FOUND.into_response();
    let id = file.strip_suffix(".png").and_then(|id| id.parse::<i64>().ok())
        .filter(|id| *id > 0).ok_or_else(missing)?;
    let (_, session) = app.authorized(&headers, None, false).await?;
    let face = pets::case_opening_avatar(&app.data.db, session.guild, id)
        .map_err(failed)?.ok_or_else(missing)?;
    case_avatar_image(&app, &face).await
}

async fn case_avatar_image(app: &Miniapp, face: &str) -> Reply {
    let bytes = wordboard::cached_avatar(&app.data, face).await
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    Ok(([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "private, no-store")], bytes).into_response())
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
    let state = rocket::start(&app.data, (session.guild, session.user), &session.name, &session.face, bet, auto, expected)
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

async fn rocket_avatar(State(app): State<App>, Path(file): Path<String>, headers: HeaderMap) -> Reply {
    let missing = || StatusCode::NOT_FOUND.into_response();
    let user = file.strip_suffix(".png").and_then(discord_id).ok_or_else(missing)?;
    let (_, session) = app.authorized(&headers, None, false).await?;
    let face = rocket::history_avatar(&app.data.db, session.guild, user)
        .map_err(failed)?.ok_or_else(missing)?;
    let bytes = wordboard::cached_avatar(&app.data, &face).await.ok_or_else(missing)?;
    Ok(([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "private, no-store")], bytes).into_response())
}

async fn casino_action(
    State(app): State<App>,
    headers: HeaderMap,
    Path((game, action)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Reply {
    let rule = match game.as_str() {
        "mines" | "poker" | "slots" | "blackjack" | "roulette" | "coinflip" | "durak" => game.as_str(),
        _ => return Err(StatusCode::NOT_FOUND.into_response()),
    };
    if action.len() > 24 || !body.is_object() {
        return Err(json_error("Некорректное действие.", StatusCode::BAD_REQUEST));
    }
    let (_, session) = app.authorized(&headers, None, false).await?;
    // Новые игры появились после настройки части серверов, поэтому применяем
    // игровой канал к ним и при отсутствии их имён в старом конфиге.
    if matches!(rule, "poker" | "mines" | "durak") {
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
            "durak" if action.starts_with("online_") => Ok(data.durak_online.handle_with_bank(data.as_ref(), guild, user, name, &action, &body)),
            "durak" => Ok(data.durak.handle(guild, user, &action, &body)),
            "poker" => crate::poker::handle(&data, guild, user, name, &action, &body),
            "blackjack" => crate::table_blackjack::handle(&data, guild, user, name, &action, &body),
            "roulette" => Ok(crate::roulette::handle(&data, guild, user, name, &action, &body)?),
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
                crate::roulette::sweep(&data);
                if let Err(error) = data.durak_online.sweep(data.as_ref()) {
                    eprintln!("Дурак: {error}");
                }
            }).await {
                eprintln!("Mini App: {error}");
            }
        }
    });
}

async fn state(State(app): State<App>, headers: HeaderMap) -> Reply {
    let (_, session) = app.authorized(&headers, Some("words"), false).await?;
    let game = words::daily_game(&app.data, session.guild, session.user, session.lang).map_err(failed)?;
    if !game.finished() {
        app.refresh_board(&session, session.lang);
    }
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

/// Аватар участника того же стола. Подлинная сессия нужна даже для загрузки своего аватара.
async fn durak_avatar(State(app): State<App>, Path((room, file)): Path<(String, String)>, headers: HeaderMap) -> Reply {
    let missing = || StatusCode::NOT_FOUND.into_response();
    let user = file.strip_suffix(".png").and_then(discord_id).ok_or_else(missing)?;
    let (_, session) = app.authorized(&headers, None, false).await?;
    if !app.data.durak_online.shared_room(session.guild, session.user, user, &room) {
        return Err(missing());
    }
    session_avatar(&app, session.guild, user).await
}

async fn game_avatar(State(app): State<App>, Path(file): Path<String>, headers: HeaderMap) -> Reply {
    let user = file.strip_suffix(".png").and_then(discord_id).ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    let (_, session) = app.authorized(&headers, None, false).await?;
    session_avatar(&app, session.guild, user).await
}

async fn session_avatar(app: &Miniapp, guild: u64, user: u64) -> Reply {
    let missing = || StatusCode::NOT_FOUND.into_response();
    let face = locked(&app.sessions).values()
        .filter(|player| player.guild == guild && player.user == user && player.created.elapsed() < SESSION_LIFETIME)
        .max_by_key(|player| player.checked)
        .map(|player| player.face.clone()).ok_or_else(missing)?;
    let bytes = wordboard::cached_avatar(&app.data, &face).await.ok_or_else(missing)?;
    Ok(([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "private, no-store")], bytes).into_response())
}

fn ai_activity_error(error: ai_activity::Error) -> Response {
    if let ai_activity::Error::Storage(source) = &error {
        eprintln!("Не удалось обработать чат Mini App: {source}");
    }
    ([(header::CACHE_CONTROL, "private, no-store")], json_error(error.message(), error.status())).into_response()
}

fn ai_json(body: Value) -> Response {
    ([(header::CACHE_CONTROL, "private, no-store")], Json(body)).into_response()
}

async fn ai_overview(State(app): State<App>, headers: HeaderMap) -> Reply {
    let (_, session) = app.authorized(&headers, None, false).await?;
    let overview = ai_activity::overview(&app.data, session.user).map_err(ai_activity_error)?;
    Ok(ai_json(overview))
}

async fn ai_chat(State(app): State<App>, Path(chat): Path<String>, headers: HeaderMap) -> Reply {
    let (_, session) = app.authorized(&headers, None, false).await?;
    let history = ai_activity::chat(&app.data, session.user, &chat).map_err(ai_activity_error)?;
    Ok(ai_json(ai_math::render_activity_turns(history).await.map_err(failed)?))
}

async fn ai_send(State(app): State<App>, incoming: Request) -> Reply {
    let (_, session) = app.authorized(incoming.headers(), None, false).await?;
    let Json(body) = Json::<Value>::from_request(incoming, &app).await.map_err(|error| {
        let status = error.status();
        let message = if status == StatusCode::PAYLOAD_TOO_LARGE {
            "Запрос слишком большой. Прикрепи изображение до 4 МБ."
        } else {
            "Не удалось прочитать сообщение. Попробуй отправить его ещё раз."
        };
        json_error(message, status)
    })?;
    let request = ai_activity::Request::from_json(&body).map_err(ai_activity_error)?;
    drop(body);
    let history = ai_activity::send(&app.data, session.user, request).await.map_err(ai_activity_error)?;
    Ok(ai_json(ai_math::render_activity_turns(history).await.map_err(failed)?))
}

async fn ai_image(State(app): State<App>, Path(turn): Path<String>, headers: HeaderMap) -> Reply {
    let (_, session) = app.authorized(&headers, None, false).await?;
    let (mime, bytes) = ai_activity::image(&app.data, session.user, &turn).map_err(ai_activity_error)?;
    Ok(([
        (header::CONTENT_TYPE, mime.as_str()),
        (header::CACHE_CONTROL, "private, no-store"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    ], bytes).into_response())
}

pub fn router(app: App) -> axum::Router {
    axum::Router::new()
        .route("/", get(index))
        .route("/static/{name}", get(static_file))
        .route("/static/pets/{name}", get(pet_art))
        .route("/api/config", get(config))
        .route("/api/token", post(token))
        .route("/api/ai", get(ai_overview))
        .route("/api/ai/chats/{chat}", get(ai_chat))
        .route("/api/ai/send", post(ai_send).layer(DefaultBodyLimit::max(6 * 1024 * 1024)))
        .route("/api/ai/image/{turn}", get(ai_image))
        .route("/api/state", get(state))
        .route("/api/guess", post(guess))
        .route("/api/new", post(new_game))
        .route("/api/players", get(players))
        .route("/api/cases", get(cases_catalogue))
        .route("/api/cases/open", post(cases_open).layer(DefaultBodyLimit::max(1024)))
        .route("/api/cases/inventory", get(cases_inventory))
        .route("/api/cases/leaderboard", get(cases_leaderboard))
        .route("/api/cases/inventory/collect", post(cases_inventory_collect))
        .route("/api/cases/inventory/move", post(cases_inventory_move).layer(DefaultBodyLimit::max(1024)))
        .route("/api/cases/inventory/transform", post(cases_pet_upgrade).layer(DefaultBodyLimit::max(4096)))
        .route("/api/cases/inventory/contract/quote", post(cases_contract_quote).layer(DefaultBodyLimit::max(4096)))
        .route("/api/cases/inventory/contract/sign", post(cases_contract_sign).layer(DefaultBodyLimit::max(4096)))
        .route("/api/cases/inventory/upgrade", post(cases_inventory_upgrade).layer(DefaultBodyLimit::max(1024)))
        .route("/api/cases/inventory/sell", post(cases_inventory_sell))
        .route("/api/cases/avatar.png", get(cases_avatar))
        .route("/api/cases/history", get(cases_history))
        .route("/api/cases/history/{file}", get(cases_history_avatar))
        .route("/api/rocket/state", get(rocket_state))
        .route("/api/rocket/start", post(rocket_start))
        .route("/api/rocket/cashout", post(rocket_cashout))
        .route("/api/rocket/avatar/{file}", get(rocket_avatar))
        .route("/api/games/{game}/{action}", post(casino_action).layer(DefaultBodyLimit::max(2048)))
        .route("/api/avatar/{guild}/{file}", get(avatar))
        .route("/api/durak/avatar/{room}/{file}", get(durak_avatar))
        .route("/api/game-avatar/{file}", get(game_avatar))
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
