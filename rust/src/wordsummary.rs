//! Scheduled, restart-safe summaries of the last closed Moscow day, for both languages.
//! The job and its pages are immutable snapshots; only delivery checkpoints change.

use crate::db::Database;
use crate::draw::{self, Anchor, Fonts};
use crate::wordboard;
use crate::words::{self, Game, Lang, MAX_GUESSES};
use crate::{Data, Error, Shared};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use image::{Rgba, RgbaImage};
use poise::serenity_prelude as serenity;
use rusqlite::{params, OptionalExtension};
use std::time::Duration;

const PLAYERS_PER_PAGE: usize = 6;
const NETWORK_TIMEOUT: Duration = Duration::from_secs(8);
const RETRY_INTERVAL: Duration = Duration::from_secs(60);

pub fn prepare(db: &Database) -> rusqlite::Result<()> {
    let mut connection = db.lock();
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS word_summary_targets (
            guild_id INTEGER NOT NULL, lang TEXT NOT NULL, channel_id INTEGER NOT NULL,
            PRIMARY KEY (guild_id, lang));
         CREATE TABLE IF NOT EXISTS word_profiles (
            guild_id INTEGER NOT NULL, user_id INTEGER NOT NULL, name TEXT NOT NULL, face TEXT NOT NULL,
            PRIMARY KEY (guild_id, user_id));
         CREATE TABLE IF NOT EXISTS word_language_preferences (
            guild_id INTEGER NOT NULL, user_id INTEGER NOT NULL, lang TEXT NOT NULL,
            PRIMARY KEY (guild_id, user_id));
         CREATE TABLE IF NOT EXISTS word_summary_jobs (
            id INTEGER PRIMARY KEY, guild_id INTEGER NOT NULL, lang TEXT NOT NULL, day TEXT NOT NULL,
            channel_id INTEGER NOT NULL, UNIQUE (guild_id, lang, day));
         CREATE TABLE IF NOT EXISTS word_summary_pages (
            job_id INTEGER NOT NULL, page INTEGER NOT NULL, total INTEGER NOT NULL,
            content TEXT NOT NULL, players TEXT NOT NULL, first_attempt INTEGER, message_id INTEGER, scan_before INTEGER,
            PRIMARY KEY (job_id, page));
         CREATE TABLE IF NOT EXISTS word_summary_legacy (
            guild_id INTEGER NOT NULL, lang TEXT NOT NULL, day TEXT NOT NULL,
            PRIMARY KEY (guild_id, lang, day));
         CREATE TABLE IF NOT EXISTS word_summary_migrations (version INTEGER PRIMARY KEY);",
    )?;
    let has_cursor: bool = transaction.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('word_summary_pages') WHERE name = 'scan_before')",
        [],
        |row| row.get(0),
    )?;
    if !has_cursor {
        transaction.execute("ALTER TABLE word_summary_pages ADD COLUMN scan_before INTEGER", [])?;
    }
    if transaction.execute("INSERT OR IGNORE INTO word_summary_migrations (version) VALUES (1)", [])? != 0 {
        // Old code marked the PUBLICATION day before POST. Those ambiguous legacy markers
        // must be trusted to avoid duplicating already emitted summaries. Never write new ones.
        for lang in Lang::ALL {
            transaction.execute(
                &format!(
                    "INSERT OR IGNORE INTO word_summary_legacy (guild_id, lang, day)
                    SELECT guild_id, ?1, date(day, '-1 day') FROM {} WHERE date(day) IS NOT NULL",
                    lang.summary_table()
                ),
                [lang.key()],
            )?;
        }
    }
    transaction.commit()
}

/// Call whenever a valid guild game is opened/played, before checking `announce`.
/// The caller validates the guild/channel association; all stored IDs retain their u64 bits.
pub fn remember_channel(data: &Data, guild: u64, channel: u64, player: &wordboard::Player, lang: Lang) -> rusqlite::Result<()> {
    let mut connection = data.db.lock();
    let transaction = connection.transaction()?;
    transaction.execute(
        "INSERT INTO word_summary_targets (guild_id, lang, channel_id) VALUES (?1, ?2, ?3)
         ON CONFLICT (guild_id, lang) DO UPDATE SET channel_id = excluded.channel_id",
        params![guild as i64, lang.key(), channel as i64],
    )?;
    transaction.execute(
        "INSERT INTO word_profiles (guild_id, user_id, name, face) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (guild_id, user_id) DO UPDATE SET name = excluded.name, face = excluded.face",
        params![guild as i64, player.id as i64, player.name, player.face],
    )?;
    transaction.commit()
}

pub fn preferred_lang(data: &Data, guild: u64, user: u64) -> Lang {
    let result: rusqlite::Result<Option<String>> = data
        .db
        .lock()
        .query_row(
            "SELECT lang FROM word_language_preferences WHERE guild_id = ?1 AND user_id = ?2",
            params![guild as i64, user as i64],
            |row| row.get(0),
        )
        .optional();
    match result {
        Ok(lang) => lang.as_deref().and_then(Lang::from_key).unwrap_or(Lang::Ru),
        Err(error) => {
            report("language preference", &error);
            Lang::Ru
        }
    }
}

pub fn set_lang(data: &Data, guild: u64, user: u64, lang: Lang) -> rusqlite::Result<()> {
    data.db.lock().execute(
        "INSERT INTO word_language_preferences (guild_id, user_id, lang) VALUES (?1, ?2, ?3)
         ON CONFLICT (guild_id, user_id) DO UPDATE SET lang = excluded.lang",
        params![guild as i64, user as i64, lang.key()],
    )?;
    Ok(())
}

fn profile(db: &Database, guild: u64, user: u64) -> rusqlite::Result<Option<(String, String)>> {
    db.lock()
        .query_row(
            "SELECT name, face FROM word_profiles WHERE guild_id = ?1 AND user_id = ?2",
            params![guild as i64, user as i64],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
}

fn target_channel(db: &Database, guild: u64, lang: Lang) -> rusqlite::Result<Option<u64>> {
    if let Some(channel) = db.games_channel(guild)? {
        return Ok(Some(channel));
    }
    db.lock()
        .query_row(
            "SELECT channel_id FROM word_summary_targets WHERE guild_id = ?1 AND lang = ?2",
            params![guild as i64, lang.key()],
            |row| Ok(row.get::<_, i64>(0)? as u64),
        )
        .optional()
}

pub fn next_moscow_midnight(now: DateTime<Utc>) -> DateTime<Utc> {
    let tomorrow = now
        .with_timezone(&chrono_tz::Europe::Moscow)
        .date_naive()
        .succ_opt()
        .expect("date within chrono range");
    chrono_tz::Europe::Moscow
        .from_local_datetime(&tomorrow.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .with_timezone(&Utc)
}

fn latest_closed_day(now: DateTime<Utc>) -> String {
    now.with_timezone(&chrono_tz::Europe::Moscow)
        .date_naive()
        .pred_opt()
        .unwrap()
        .to_string()
}

#[derive(Clone)]
struct Entry {
    user: u64,
    name: String,
    face: String,
    game: Game,
}

impl Entry {
    fn score(&self) -> usize {
        if self.game.won() {
            self.game.guesses.len()
        } else if self.game.finished() {
            MAX_GUESSES + 1
        } else {
            MAX_GUESSES + 2
        }
    }

    fn json(&self) -> serde_json::Value {
        serde_json::json!({"user": self.user, "name": self.name, "face": self.face,
            "answer": self.game.answer, "guesses": self.game.guesses.join(",")})
    }
}

fn ranked_entries(db: &Database, guild: u64, lang: Lang, day: &str, rows: &[(u64, String, String)]) -> rusqlite::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for (user, answer, guesses) in rows {
        let game = Game::from_row(lang, day, answer.clone(), guesses);
        if !game.guesses.iter().any(|guess| !guess.trim().is_empty()) {
            continue;
        }
        let (name, face) = profile(db, guild, *user)?.unwrap_or_default();
        entries.push(Entry {
            user: *user,
            name,
            face,
            game,
        });
    }
    entries.sort_by_key(|entry| (entry.score(), entry.user));
    Ok(entries)
}

fn grouped_results(entries: &[Entry], best: Option<usize>) -> String {
    let mut lines = Vec::new();
    let mut index = 0;
    while index < entries.len() {
        let score = entries[index].score();
        let people: Vec<_> = entries[index..]
            .iter()
            .take_while(|entry| entry.score() == score)
            .map(|entry| format!("<@{}>", entry.user))
            .collect();
        index += people.len();
        let label = match score {
            1..=MAX_GUESSES => format!("{}{score}/{MAX_GUESSES}", if best == Some(score) { "👑 " } else { "" }),
            n if n == MAX_GUESSES + 1 => format!("X/{MAX_GUESSES}"),
            _ => "… Незавершённые".to_string(),
        };
        lines.push(format!("{label}: {}", people.join(" ")));
    }
    lines.join("\n")
}

fn caption(lang: Lang, day: &str, streak: i64, entries: &[Entry], best: Option<usize>, page: usize, total: usize) -> String {
    // The only answer in the Discord payload is under a spoiler. Renderers receive marks only.
    let answer = entries.first().map(|entry| entry.game.answer.to_uppercase()).unwrap_or_default();
    format!(
        "🧩 **Итоги · {} №{} · {day}** · {page}/{total}\n🔥 Сервер играет {streak} {} подряд\n{}\nСлово дня: ||{answer}||",
        lang.name(),
        words::day_number(lang, day),
        crate::text::plural(streak, "день", "дня", "дней"),
        grouped_results(entries, best)
    )
}

/// Build only yesterday's jobs. Existing older pending pages are always retried separately.
fn enqueue_latest(data: &Data, now: DateTime<Utc>) -> rusqlite::Result<()> {
    let day = latest_closed_day(now);
    let guilds: Vec<u64> = {
        let connection = data.db.lock();
        // A configured channel is enough to recover old guilds that predate remember_channel.
        for lang in Lang::ALL {
            connection.execute(
                "INSERT OR IGNORE INTO word_summary_targets (guild_id, lang, channel_id)
                 SELECT guild_id, ?1, channel_id FROM games_channel WHERE channel_id IS NOT NULL",
                [lang.key()],
            )?;
        }
        let mut statement = connection.prepare("SELECT DISTINCT guild_id FROM word_summary_targets ORDER BY guild_id")?;
        let rows = statement
            .query_map([], |row| Ok(row.get::<_, i64>(0)? as u64))?
            .collect::<rusqlite::Result<_>>()?;
        rows
    };
    for guild in guilds {
        for lang in Lang::ALL {
            if !data.words.settings.get(lang).announce {
                continue;
            }
            enqueue_day(data, guild, lang, &day)?;
        }
    }
    Ok(())
}

fn enqueue_day(data: &Data, guild: u64, lang: Lang, day: &str) -> rusqlite::Result<()> {
    let exists: bool = data.db.lock().query_row(
        "SELECT EXISTS (SELECT 1 FROM word_summary_jobs WHERE guild_id = ?1 AND lang = ?2 AND day = ?3)
          OR EXISTS (SELECT 1 FROM word_summary_legacy WHERE guild_id = ?1 AND lang = ?2 AND day = ?3)",
        params![guild as i64, lang.key(), day],
        |row| row.get(0),
    )?;
    if exists {
        return Ok(());
    }
    let Some(channel) = target_channel(&data.db, guild, lang)? else {
        return Ok(());
    };
    // Explicit closed date: never call daily_game, which would create today's game.
    let rows = data.db.word_day_rows(lang.table(), guild, day)?;
    let entries = ranked_entries(&data.db, guild, lang, day, &rows)?;
    if entries.is_empty() {
        return Ok(());
    }
    let played_days: Vec<String> = {
        let connection = data.db.lock();
        let mut statement = connection.prepare(&format!(
            "SELECT DISTINCT day FROM {} WHERE guild_id = ?1
             AND trim(COALESCE(guesses, ''), ', ' || char(9) || char(10) || char(13)) != ''",
            lang.table()
        ))?;
        let days = statement
            .query_map([guild as i64], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        days
    };
    let streak = words::streak(&played_days, day);
    let best = entries.iter().map(Entry::score).filter(|score| *score <= MAX_GUESSES).min();
    let total = entries.len().div_ceil(PLAYERS_PER_PAGE);
    let mut connection = data.db.lock();
    let transaction = connection.transaction()?;
    if transaction.execute(
        "INSERT OR IGNORE INTO word_summary_jobs (guild_id, lang, day, channel_id) VALUES (?1, ?2, ?3, ?4)",
        params![guild as i64, lang.key(), day, channel as i64],
    )? == 0
    {
        return Ok(());
    }
    let job = transaction.last_insert_rowid();
    for (index, entries) in entries.chunks(PLAYERS_PER_PAGE).enumerate() {
        let content = caption(lang, day, streak, entries, best, index + 1, total);
        // With six mentions and a five-letter answer this stays far below Discord's 2000.
        if content.encode_utf16().count() >= 2000 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let players = serde_json::Value::Array(entries.iter().map(Entry::json).collect()).to_string();
        transaction.execute(
            "INSERT INTO word_summary_pages (job_id, page, total, content, players) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![job, index as u32 + 1, total as u32, content, players],
        )?;
    }
    transaction.commit()
}

struct Page {
    job: i64,
    guild: u64,
    lang: Lang,
    day: String,
    channel: u64,
    number: u32,
    total: u32,
    content: String,
    players: String,
    first_attempt: Option<i64>, // UTC milliseconds, saved BEFORE the current POST
    scan_before: Option<u64>,   // exclusive Discord history cursor; durable across bounded scans
}

impl Page {
    fn nonce(&self) -> String {
        // Stable even after restoring a backup that reuses job rowids. FNV-1a over
        // the semantic page identity; fixed width IDs and separators avoid ambiguity.
        let mut hash = 0xcbf29ce484222325_u64;
        for byte in self
            .guild
            .to_be_bytes()
            .into_iter()
            .chain(self.lang.key().bytes())
            .chain([0])
            .chain(self.day.bytes())
            .chain([0])
            .chain(self.number.to_be_bytes())
        {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
        format!("ws-{hash:016x}")
    }
    fn footer(&self) -> String {
        format!(
            "Итоги • {} • {} • {}/{}",
            self.lang.key().to_uppercase(),
            self.day,
            self.number,
            self.total
        )
    }
}

fn pending_pages(db: &Database) -> rusqlite::Result<Vec<Page>> {
    let connection = db.lock();
    let mut statement = connection.prepare(
        "SELECT j.id, j.guild_id, j.lang, j.day, j.channel_id, p.page, p.total, p.content, p.players, p.first_attempt, p.scan_before
         FROM word_summary_jobs j JOIN word_summary_pages p ON p.job_id = j.id
         WHERE p.message_id IS NULL ORDER BY j.day DESC, j.guild_id, j.lang, p.page",
    )?;
    let pages = statement
        .query_map([], |row| {
            let key: String = row.get(2)?;
            let lang = Lang::from_key(&key).ok_or(rusqlite::Error::InvalidQuery)?;
            Ok(Page {
                job: row.get(0)?,
                guild: row.get::<_, i64>(1)? as u64,
                lang,
                day: row.get(3)?,
                channel: row.get::<_, i64>(4)? as u64,
                number: row.get(5)?,
                total: row.get(6)?,
                content: row.get(7)?,
                players: row.get(8)?,
                first_attempt: row.get(9)?,
                scan_before: row.get::<_, Option<i64>>(10)?.map(|id| id as u64),
            })
        })?
        .collect();
    pages
}

fn begin_attempt(db: &Database, page: &Page, now_ms: i64) -> rusqlite::Result<bool> {
    let updated = db.lock().execute(
        "UPDATE word_summary_pages SET first_attempt = ?1, scan_before = NULL
         WHERE job_id = ?2 AND page = ?3 AND message_id IS NULL AND first_attempt IS NULL AND scan_before IS NULL",
        params![now_ms, page.job, page.number],
    )?;
    Ok(updated != 0)
}

fn record_receipt(db: &Database, page: &Page, message: u64) -> rusqlite::Result<()> {
    db.lock().execute(
        "UPDATE word_summary_pages SET message_id = ?1, scan_before = NULL WHERE job_id = ?2 AND page = ?3 AND message_id IS NULL",
        params![message as i64, page.job, page.number],
    )?;
    Ok(())
}

/// The top and bottom bands add readable context to the existing clean six-grid renderer.
fn summary_image(fonts: &Fonts, lang: Lang, day: &str, page: u32, total: u32, cards: &[wordboard::Card]) -> RgbaImage {
    let cards = &cards[..cards.len().min(PLAYERS_PER_PAGE)];
    let board = wordboard::board_image(fonts, cards);
    let width = board.width().max(1000);
    let offset = (width - board.width()) / 2;
    let mut image = RgbaImage::from_pixel(width, board.height() + 104, Rgba([18, 19, 22, 255]));
    image::imageops::overlay(&mut image, &board, i64::from(offset), 64);
    let title = format!("Итоги · {} №{}", lang.name(), words::day_number(lang, day));
    let title = draw::fit_text(&fonts.bold, 28.0, &title, width as f32 - 40.0);
    draw::text(
        &mut image,
        (width as f32 / 2.0, 28.0),
        Anchor::Center,
        &fonts.bold,
        28.0,
        draw::WHITE,
        &title,
    );
    let subtitle = format!("{day} · {page}/{total} · {}", lang.key().to_uppercase());
    let subtitle = draw::fit_text(&fonts.regular, 20.0, &subtitle, width as f32 - 40.0);
    draw::text(
        &mut image,
        (width as f32 / 2.0, 54.0),
        Anchor::Center,
        &fonts.regular,
        20.0,
        draw::TEXT_DIM,
        &subtitle,
    );
    // Geometry matches board_image's scale=2, 6 columns; names identify avatar fallbacks too.
    for (index, card) in cards.iter().enumerate() {
        let name = draw::fit_text(&fonts.regular, 18.0, &card.name, 206.0);
        draw::text(
            &mut image,
            (offset as f32 + 139.0 + index as f32 * 246.0, board.height() as f32 + 84.0),
            Anchor::Center,
            &fonts.regular,
            18.0,
            draw::TEXT_DIM,
            &name,
        );
    }
    image
}

async fn avatar_card(
    ctx: serenity::Context,
    data: Shared,
    guild: u64,
    user: u64,
    mut name: String,
    mut face: String,
    rows: Vec<Vec<words::Mark>>,
) -> wordboard::Card {
    // One bound for member/user lookup and image download combined; work proceeds without an avatar on timeout.
    let found = tokio::time::timeout(Duration::from_secs(5), async {
        if name.is_empty() || face.is_empty() {
            if let Ok(Ok(member)) = tokio::time::timeout(Duration::from_secs(2), serenity::GuildId::new(guild).member(&ctx, user)).await {
                name = member.display_name().to_string();
                face = member.face();
            } else if let Ok(Ok(found)) = tokio::time::timeout(Duration::from_secs(2), serenity::UserId::new(user).to_user(&ctx)).await {
                name = found.global_name.clone().unwrap_or_else(|| found.name.clone());
                face = found.face();
            }
        }
        // Use cached_avatar's cache, but release its mutex before the network request.
        // cached_avatar itself holds that mutex during download, serializing all players.
        let cached = { data.boards.avatars.lock().await.get(&user).cloned() };
        if let Some(known) = cached {
            return known;
        }
        if face.is_empty() {
            return None;
        }
        let avatar = crate::levels::fetch_avatar(&data, &face).await;
        data.boards.avatars.lock().await.insert(user, avatar.clone());
        avatar
    })
    .await
    .ok()
    .flatten();
    if name.is_empty() {
        name = format!("Игрок {user}");
    }
    wordboard::Card { name, avatar: found, rows }
}

async fn render_page(ctx: &serenity::Context, data: &Shared, page: &Page) -> Result<Vec<u8>, Error> {
    let players: serde_json::Value = serde_json::from_str(&page.players)?;
    let players = players.as_array().ok_or("invalid summary players snapshot")?;
    if players.is_empty() || players.len() > PLAYERS_PER_PAGE {
        return Err("invalid summary page size".into());
    }
    let mut tasks = tokio::task::JoinSet::new();
    for (index, player) in players.iter().enumerate() {
        let user = player["user"].as_u64().ok_or("invalid summary user")?;
        let name = player["name"].as_str().ok_or("invalid summary name")?.to_string();
        let face = player["face"].as_str().ok_or("invalid summary face")?.to_string();
        let answer = player["answer"].as_str().ok_or("invalid summary answer")?.to_string();
        let guesses = player["guesses"].as_str().ok_or("invalid summary guesses")?;
        let rows = Game::from_row(page.lang, &page.day, answer, guesses).rows();
        let (ctx, data, guild) = (ctx.clone(), data.clone(), page.guild);
        tasks.spawn(async move { (index, avatar_card(ctx, data, guild, user, name, face, rows).await) });
    }
    let mut cards = Vec::new();
    while let Some(result) = tasks.join_next().await {
        cards.push(result?);
    }
    cards.sort_by_key(|(index, _)| *index);
    let cards: Vec<_> = cards.into_iter().map(|(_, card)| card).collect();
    let (dir, lang, day, number, total) = (data.dir.clone(), page.lang, page.day.clone(), page.number, page.total);
    tokio::task::spawn_blocking(move || {
        let fonts = draw::fonts(&dir).ok_or("no fonts for summary image")?;
        Ok(draw::to_png(&summary_image(fonts, lang, &day, number, total, &cards)))
    })
    .await?
}

fn matches_receipt(message: &serenity::Message, bot: serenity::UserId, nonce: &str, footer: &str) -> bool {
    message.author.id == bot
        && message.author.bot
        && (message.nonce.as_ref().is_some_and(|value| match value {
            serenity::Nonce::String(value) => value == nonce,
            serenity::Nonce::Number(value) => value.to_string() == nonce,
        }) || message
            .embeds
            .iter()
            .any(|embed| embed.footer.as_ref().is_some_and(|value| value.text == footer)))
}

#[derive(Debug, PartialEq, Eq)]
enum HistoryState {
    Pending(u64),
    Absent,
    Delivered,
}

/// Commit every fully inspected batch before another HTTP request. A timeout/API error
/// can then resume from the last successful batch, even in very crowded channels.
fn history_step(db: &Database, page: &Page, bot: serenity::UserId, messages: &[serenity::Message]) -> Result<HistoryState, Error> {
    let first_attempt = page.first_attempt.ok_or("history scan without a delivery attempt")?;
    let (nonce, footer) = (page.nonce(), page.footer());
    if let Some(message) = messages.iter().find(|message| matches_receipt(message, bot, &nonce, &footer)) {
        record_receipt(db, page, message.id.get())?;
        return Ok(HistoryState::Delivered);
    }
    let oldest = messages.iter().min_by_key(|message| message.id);
    if messages.len() < 100 || oldest.is_some_and(|message| message.timestamp.unix_timestamp() * 1000 <= first_attempt - 5000) {
        // Absence has been established. Clear both checkpoints before a fresh attempt;
        // if that POST fails, its reconciliation starts at the newest messages again.
        db.lock().execute(
            "UPDATE word_summary_pages SET first_attempt = NULL, scan_before = NULL
             WHERE job_id = ?1 AND page = ?2 AND message_id IS NULL",
            params![page.job, page.number],
        )?;
        return Ok(HistoryState::Absent);
    }
    let before = oldest.ok_or("history batch without a cursor")?.id.get();
    db.lock().execute(
        "UPDATE word_summary_pages SET scan_before = ?1 WHERE job_id = ?2 AND page = ?3 AND message_id IS NULL",
        params![before as i64, page.job, page.number],
    )?;
    Ok(HistoryState::Pending(before))
}

/// Reconcile the crash window between POST and SQLite receipt. Every bounded scan
/// resumes its persisted cursor; incomplete/error scans never authorize another POST.
async fn reconcile(ctx: &serenity::Context, db: &Database, page: &Page) -> Result<HistoryState, Error> {
    let channel = serenity::ChannelId::new(page.channel);
    let bot = ctx.cache.current_user().id;
    let mut before = page.scan_before.map(serenity::MessageId::new);
    // At most 500 messages per retry, with one overall network deadline. Progress is
    // saved after each 100 so even the deadline cannot perpetually restart the scan.
    let scan = async {
        for _ in 0..5 {
            let mut query = serenity::GetMessages::new().limit(100);
            if let Some(id) = before {
                query = query.before(id);
            }
            let messages = channel.messages(&ctx.http, query).await?;
            match history_step(db, page, bot, &messages)? {
                HistoryState::Pending(cursor) => before = Some(serenity::MessageId::new(cursor)),
                terminal => return Ok(terminal),
            }
        }
        Ok::<HistoryState, Error>(HistoryState::Pending(before.ok_or("missing history continuation")?.get()))
    };
    tokio::time::timeout(NETWORK_TIMEOUT, scan).await?
}

async fn deliver(ctx: &serenity::Context, data: &Shared, page: &Page) -> Result<(), Error> {
    if page.first_attempt.is_some() {
        match reconcile(ctx, &data.db, page).await? {
            HistoryState::Pending(_) | HistoryState::Delivered => return Ok(()),
            HistoryState::Absent => {}
        }
    }
    let png = render_page(ctx, data, page).await?;
    let filename = format!("words-{}-{}-{}.png", page.lang.key(), page.day, page.number);
    let message = serenity::CreateMessage::new()
        .content(&page.content)
        .embed(
            serenity::CreateEmbed::new()
                .image(format!("attachment://{filename}"))
                .footer(serenity::CreateEmbedFooter::new(page.footer())),
        )
        .add_file(serenity::CreateAttachment::bytes(png, filename))
        .components(wordboard::play_button(page.lang))
        .allowed_mentions(serenity::CreateAllowedMentions::new())
        .nonce(serenity::Nonce::String(page.nonce()))
        .enforce_nonce(true);
    // No DB guard survives this line. Timeout/cancellation/failure leaves the page pending.
    if !begin_attempt(&data.db, page, Utc::now().timestamp_millis())? {
        return Err("summary page still requires reconciliation or already has a receipt".into());
    }
    let sent = tokio::time::timeout(
        NETWORK_TIMEOUT,
        serenity::ChannelId::new(page.channel).send_message(&ctx.http, message),
    )
    .await??;
    record_receipt(&data.db, page, sent.id.get())?;
    Ok(())
}

fn report(operation: &str, error: &dyn std::fmt::Display) {
    println!(
        "Word summaries ({operation}): {}",
        crate::interactions::sanitize_error(&error.to_string())
    );
}

/// Start exactly once from framework setup (not from Ready or a player interaction).
/// Wakes at the absolute Moscow boundary; retries pending work every 60 seconds.
pub fn start_loop(ctx: serenity::Context, data: Shared) {
    tokio::spawn(async move {
        if let Err(error) = prepare(&data.db) {
            report("prepare", &error);
            return;
        }
        let wake = std::sync::Arc::new(tokio::sync::Notify::new());
        let (delivery_data, delivery_wake) = (data.clone(), wake.clone());
        // The midnight producer is independent of slow Discord/avatar requests.
        tokio::spawn(async move {
            loop {
                delivery_wake.notified().await;
                let boundary = next_moscow_midnight(Utc::now());
                match pending_pages(&delivery_data.db) {
                    Ok(pages) => {
                        for page in pages {
                            if Utc::now() >= boundary {
                                break;
                            }
                            if !delivery_data.words.settings.get(page.lang).announce {
                                continue;
                            }
                            let closed = latest_closed_day(Utc::now());
                            if page.day.parse::<NaiveDate>().is_err() || page.day > closed {
                                continue;
                            }
                            if let Err(error) = deliver(&ctx, &delivery_data, &page).await {
                                report("delivery", &error);
                            }
                        }
                    }
                    Err(error) => report("pending pages", &error),
                }
            }
        });
        loop {
            let now = Utc::now();
            let midnight = next_moscow_midnight(now);
            if let Err(error) = enqueue_latest(&data, now) {
                report("snapshot", &error);
            }
            wake.notify_one();
            let now = Utc::now();
            if now >= midnight {
                continue;
            }
            let until_midnight = (midnight - now).to_std().unwrap_or_default();
            tokio::time::sleep(until_midnight.min(RETRY_INTERVAL)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn data() -> Data {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
        let data = Data::new(Database::in_memory().unwrap(), &crate::config::Settings::parse(""), dir, false);
        prepare(&data.db).unwrap();
        data
    }

    fn game(data: &Data, guild: u64, user: u64, lang: Lang, day: &str, guesses: &str) {
        data.db.word_start(lang.table(), guild, user, day, "кошка").unwrap();
        data.db.set_word_guesses(lang.table(), guild, user, day, guesses).unwrap();
    }

    fn target(data: &Data, guild: u64, channel: u64, user: u64, lang: Lang) {
        remember_channel(
            data,
            guild,
            channel,
            &wordboard::Player {
                id: user,
                name: format!("Игрок {user}"),
                face: format!("https://example.invalid/{user}.png"),
            },
            lang,
        )
        .unwrap();
    }

    #[test]
    fn midnight_is_moscow_for_both_languages_before_at_and_after_rollover() {
        let midnight = Utc.with_ymd_and_hms(2026, 11, 15, 21, 0, 0).unwrap();
        assert_eq!(latest_closed_day(midnight - chrono::Duration::milliseconds(1)), "2026-11-14");
        assert_eq!(latest_closed_day(midnight), "2026-11-15");
        assert_eq!(latest_closed_day(midnight + chrono::Duration::milliseconds(1)), "2026-11-15");
        assert_eq!(next_moscow_midnight(midnight - chrono::Duration::milliseconds(1)), midnight);
        assert_eq!(next_moscow_midnight(midnight), midnight + chrono::Duration::days(1));
        assert_eq!(
            next_moscow_midnight(midnight + chrono::Duration::seconds(1)),
            midnight + chrono::Duration::days(1)
        );
        let new_year = Utc.with_ymd_and_hms(2026, 12, 31, 21, 0, 0).unwrap();
        assert_eq!(latest_closed_day(new_year), "2026-12-31");
    }

    #[test]
    fn preferred_language_defaults_to_russian_and_is_persistent_and_isolated() {
        let path = std::env::temp_dir().join(format!(
            "musicbot-word-summary-{}-{}.sqlite",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        {
            let initial = data();
            let mut first = initial;
            first.db = std::sync::Arc::new(Database::open(&path).unwrap());
            prepare(&first.db).unwrap();
            assert_eq!(preferred_lang(&first, 7, 1), Lang::Ru);
            set_lang(&first, 7, 1, Lang::De).unwrap();
        }
        {
            let mut restarted = data();
            restarted.db = std::sync::Arc::new(Database::open(&path).unwrap());
            prepare(&restarted.db).unwrap();
            assert_eq!(preferred_lang(&restarted, 7, 1), Lang::De);
            assert_eq!(preferred_lang(&restarted, 8, 1), Lang::Ru);
            assert_eq!(preferred_lang(&restarted, 7, 2), Lang::Ru);
            set_lang(&restarted, 7, 1, Lang::Ru).unwrap();
            assert_eq!(preferred_lang(&restarted, 7, 1), Lang::Ru);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn remembered_channels_profiles_and_overrides_are_isolated() {
        let data = data();
        target(&data, 7, 11, 1, Lang::Ru);
        target(&data, 7, 12, 1, Lang::De);
        target(&data, 8, 13, 1, Lang::Ru);
        target(&data, 7, 14, 2, Lang::Ru);
        assert_eq!(target_channel(&data.db, 7, Lang::Ru).unwrap(), Some(14));
        assert_eq!(target_channel(&data.db, 7, Lang::De).unwrap(), Some(12));
        assert_eq!(target_channel(&data.db, 8, Lang::Ru).unwrap(), Some(13));
        assert_eq!(
            profile(&data.db, 7, 1).unwrap(),
            Some(("Игрок 1".into(), "https://example.invalid/1.png".into()))
        );
        data.db.set_games_channel(7, Some(99)).unwrap();
        assert_eq!(target_channel(&data.db, 7, Lang::De).unwrap(), Some(99));
        data.db.set_games_channel(7, None).unwrap();
        assert_eq!(target_channel(&data.db, 7, Lang::Ru).unwrap(), Some(14));
    }

    #[test]
    fn grouping_crowns_all_best_ties_then_losses_then_unfinished_and_filters_empty() {
        let rows = vec![
            (8, "кошка".into(), "лампа".into()),
            (5, "кошка".into(), "лампа,кошка".into()),
            (4, "кошка".into(), "лампа,сосна,арбуз,берег,ветер,песня".into()),
            (2, "кошка".into(), "сосна,кошка".into()),
            (9, "кошка".into(), "".into()),
            (3, "кошка".into(), ",,,".into()),
            (6, "кошка".into(), "лампа,сосна,кошка".into()),
        ];
        let data = data();
        let entries = ranked_entries(&data.db, 7, Lang::Ru, "2026-11-15", &rows).unwrap();
        assert_eq!(entries.iter().map(|p| p.user).collect::<Vec<_>>(), [2, 5, 6, 4, 8]);
        let text = grouped_results(&entries, Some(2));
        assert!(text.contains("👑 2/6: <@2> <@5>"));
        assert!(text.contains("3/6: <@6>"));
        assert!(text.contains("X/6: <@4>"));
        assert!(text.contains("… Незавершённые: <@8>"));
        assert!(!text.contains("<@9>") && !text.contains("<@3>"));
    }

    #[test]
    fn only_latest_closed_day_is_enqueued_and_empty_days_are_silent() {
        let data = data();
        target(&data, 7, 11, 1, Lang::Ru);
        game(&data, 7, 1, Lang::Ru, "2026-11-13", "кошка");
        game(&data, 7, 1, Lang::Ru, "2026-11-15", "");
        let now = Utc.with_ymd_and_hms(2026, 11, 16, 0, 0, 0).unwrap();
        enqueue_latest(&data, now).unwrap();
        assert!(pending_pages(&data.db).unwrap().is_empty());
        game(&data, 7, 2, Lang::Ru, "2026-11-15", "лампа");
        game(&data, 7, 3, Lang::Ru, "2026-11-16", "кошка");
        enqueue_latest(&data, now).unwrap();
        let pages = pending_pages(&data.db).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].day, "2026-11-15");
        assert!(pages[0].content.contains("… Незавершённые: <@2>"));
        assert!(!pages[0].content.contains("<@3>"));
    }

    #[test]
    fn legacy_markers_use_publication_day_minus_one_and_migration_is_one_time() {
        let data = data();
        // Recreate only the migration flag to model an upgrade from the old sender.
        data.db.lock().execute("DELETE FROM word_summary_migrations", []).unwrap();
        data.db.mark_word_summary("word_summary_ru", 7, "2026-11-16").unwrap();
        prepare(&data.db).unwrap();
        target(&data, 7, 11, 1, Lang::Ru);
        for day in ["2026-11-15", "2026-11-16"] {
            game(&data, 7, 1, Lang::Ru, day, "кошка");
        }
        enqueue_latest(&data, Utc.with_ymd_and_hms(2026, 11, 16, 1, 0, 0).unwrap()).unwrap();
        assert!(pending_pages(&data.db).unwrap().is_empty());
        data.db.mark_word_summary("word_summary_ru", 7, "2026-11-17").unwrap();
        prepare(&data.db).unwrap();
        enqueue_latest(&data, Utc.with_ymd_and_hms(2026, 11, 17, 1, 0, 0).unwrap()).unwrap();
        assert_eq!(pending_pages(&data.db).unwrap()[0].day, "2026-11-16");
    }

    #[test]
    fn durable_snapshots_and_receipts_survive_prepare_and_do_not_premark_delivery() {
        let path = std::env::temp_dir().join(format!(
            "musicbot-word-summary-receipts-{}-{}.sqlite",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let mut data = data();
        data.db = std::sync::Arc::new(Database::open(&path).unwrap());
        prepare(&data.db).unwrap();
        target(&data, 7, 11, 1, Lang::Ru);
        for user in (1..=14).rev() {
            game(&data, 7, user, Lang::Ru, "2026-11-15", "кошка");
        }
        let now = Utc.with_ymd_and_hms(2026, 11, 16, 0, 0, 0).unwrap();
        enqueue_latest(&data, now).unwrap();
        let pages = pending_pages(&data.db).unwrap();
        assert_eq!(pages.len(), 3);
        assert!(pages
            .iter()
            .all(|p| p.first_attempt.is_none() && p.content.encode_utf16().count() < 2000));
        assert!(pages[0].content.contains("<@1> <@2> <@3> <@4> <@5> <@6>"));
        begin_attempt(&data.db, &pages[0], 1234).unwrap();
        begin_attempt(&data.db, &pages[0], 5678).unwrap();
        prepare(&data.db).unwrap();
        let pending = pending_pages(&data.db).unwrap();
        assert_eq!(pending[0].first_attempt, Some(1234));
        assert_eq!(pending[0].nonce(), pages[0].nonce());
        record_receipt(&data.db, &pages[0], 999).unwrap();
        drop(data);
        let mut data = self::data();
        data.db = std::sync::Arc::new(Database::open(&path).unwrap());
        prepare(&data.db).unwrap();
        game(&data, 7, 100, Lang::Ru, "2026-11-15", "кошка");
        enqueue_latest(&data, now).unwrap();
        let pending = pending_pages(&data.db).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].content, pages[1].content);
        assert!(!pending.iter().any(|p| p.content.contains("<@100>")));
        enqueue_latest(&data, now + chrono::Duration::days(1)).unwrap();
        assert_eq!(pending_pages(&data.db).unwrap().len(), 2, "older pending pages survive a new day");
        assert!(pending.iter().all(|p| p.nonce().starts_with("ws-") && p.nonce().len() <= 25));
        let mut largest = pending.into_iter().next().unwrap();
        largest.job = i64::MAX;
        largest.number = u32::MAX;
        assert!(largest.nonce().starts_with("ws-") && largest.nonce().len() <= 25);
        drop(data);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn consecutive_days_count_real_attempts_even_when_no_one_wins() {
        let data = data();
        target(&data, 7, 11, 1, Lang::Ru);
        game(&data, 7, 1, Lang::Ru, "2026-11-11", "кошка");
        game(&data, 7, 1, Lang::Ru, "2026-11-12", ", ,,"); // malformed legacy empty guesses still do not count
        game(&data, 7, 1, Lang::Ru, "2026-11-13", "лампа");
        game(&data, 7, 1, Lang::Ru, "2026-11-14", "лампа,сосна,арбуз,берег,ветер,песня");
        game(&data, 7, 1, Lang::Ru, "2026-11-15", "сосна");
        enqueue_latest(&data, Utc.with_ymd_and_hms(2026, 11, 16, 0, 0, 0).unwrap()).unwrap();
        let pages = pending_pages(&data.db).unwrap();
        assert_eq!(pages.len(), 1);
        assert!(pages[0].content.contains("3 дня подряд"));
        assert!(pages[0].content.contains("||КОШКА||"));
        assert!(!pages[0].content.contains("👑"));
    }

    #[test]
    fn crash_gap_reconciliation_accepts_only_this_bots_nonce_or_exact_footer() {
        let data = data();
        target(&data, 7, 11, 1, Lang::Ru);
        game(&data, 7, 1, Lang::Ru, "2026-11-15", "кошка");
        enqueue_latest(&data, Utc.with_ymd_and_hms(2026, 11, 16, 0, 0, 0).unwrap()).unwrap();
        let page = pending_pages(&data.db).unwrap().remove(0);
        assert_eq!(page.footer(), "Итоги • RU • 2026-11-15 • 1/1");
        let bot = serenity::UserId::new(42);
        let mut message = serenity::Message::default();
        message.author.id = bot;
        message.author.bot = true;
        message.nonce = Some(serenity::Nonce::String(page.nonce()));
        assert!(matches_receipt(&message, bot, &page.nonce(), &page.footer()));
        message.author.id = serenity::UserId::new(43);
        assert!(!matches_receipt(&message, bot, &page.nonce(), &page.footer()));
        message.author.id = bot;
        message.nonce = None;
        let embed = serenity::CreateEmbed::new().footer(serenity::CreateEmbedFooter::new(page.footer()));
        message
            .embeds
            .push(serde_json::from_value(serde_json::to_value(embed).unwrap()).unwrap());
        assert!(matches_receipt(&message, bot, &page.nonce(), &page.footer()));
        assert!(!matches_receipt(&message, bot, &page.nonce(), "Итоги • RU • 2026-11-14 • 1/1"));
        message.author.bot = false;
        assert!(!matches_receipt(&message, bot, &page.nonce(), &page.footer()));
    }

    #[test]
    fn nonce_uses_semantic_page_identity_and_survives_restored_rowids() {
        let data = data();
        target(&data, 7, 11, 1, Lang::Ru);
        game(&data, 7, 1, Lang::Ru, "2026-11-15", "кошка");
        enqueue_latest(&data, Utc.with_ymd_and_hms(2026, 11, 16, 0, 0, 0).unwrap()).unwrap();
        let mut page = pending_pages(&data.db).unwrap().remove(0);
        let nonce = page.nonce();
        assert_eq!(nonce.len(), 19);
        assert!(nonce.starts_with("ws-"));
        page.job += 100;
        assert_eq!(page.nonce(), nonce, "rebuilding the same page keeps its nonce");
        page.day = "2026-11-16".into();
        assert_ne!(page.nonce(), nonce, "reusing a rowid for a new date cannot reuse the nonce");
        page.day = "2026-11-15".into();
        page.lang = Lang::De;
        assert_ne!(page.nonce(), nonce);
        page.lang = Lang::Ru;
        page.guild = 8;
        assert_ne!(page.nonce(), nonce);
        page.guild = 7;
        page.number = 2;
        assert_ne!(page.nonce(), nonce);
    }

    #[test]
    fn configured_channel_seeds_old_guilds_both_languages_and_announce_is_respected() {
        let data = data();
        data.db.set_games_channel(42, Some(99)).unwrap();
        for lang in Lang::ALL {
            game(&data, 42, 1, lang, "2026-11-15", "лампа");
        }
        let now = Utc.with_ymd_and_hms(2026, 11, 16, 0, 0, 0).unwrap();
        enqueue_latest(&data, now).unwrap();
        let pages = pending_pages(&data.db).unwrap();
        assert_eq!(pages.len(), 2);
        assert!(pages.iter().all(|p| p.channel == 99 && p.content.contains("1 день")));
        let mut disabled = self::data();
        disabled.words = words::WordsState::new(words::WordSettings::load(&crate::config::Settings::parse(
            "[slovo]\nannounce=off\n[wortspiel]\nannounce=off",
        )));
        disabled.db.set_games_channel(42, Some(99)).unwrap();
        for lang in Lang::ALL {
            game(&disabled, 42, 1, lang, "2026-11-15", "кошка");
        }
        enqueue_latest(&disabled, now).unwrap();
        assert!(pending_pages(&disabled.db).unwrap().is_empty());
    }

    #[test]
    fn reconciliation_cursor_survives_prepare_advances_and_blocks_post_until_complete() {
        let data = data();
        target(&data, 7, 11, 1, Lang::Ru);
        game(&data, 7, 1, Lang::Ru, "2026-11-15", "кошка");
        enqueue_latest(&data, Utc.with_ymd_and_hms(2026, 11, 16, 0, 0, 0).unwrap()).unwrap();
        let page = pending_pages(&data.db).unwrap().remove(0);
        let first = 1_800_000_000_000;
        assert!(begin_attempt(&data.db, &page, first).unwrap());
        // More than one 500-message scan is needed. Every checkpoint survives prepare,
        // and the real POST gate refuses to start another attempt while any scan is pending.
        for batch in 0..7_u64 {
            let page = pending_pages(&data.db).unwrap().remove(0);
            assert_eq!(page.scan_before, if batch == 0 { None } else { Some(10_001 - batch * 100) });
            let messages = history_messages(10_000 - batch * 100, 100, first + 60_000);
            assert_eq!(
                history_step(&data.db, &page, serenity::UserId::new(42), &messages).unwrap(),
                HistoryState::Pending(9_901 - batch * 100)
            );
            prepare(&data.db).unwrap();
            let resumed = pending_pages(&data.db).unwrap().remove(0);
            assert_eq!(resumed.scan_before, Some(9_901 - batch * 100));
            assert_eq!(resumed.first_attempt, Some(first));
            assert!(!begin_attempt(&data.db, &resumed, first + 120_000).unwrap());
        }
        let page = pending_pages(&data.db).unwrap().remove(0);
        let messages = history_messages(9_300, 100, first - 5_000);
        assert_eq!(
            history_step(&data.db, &page, serenity::UserId::new(42), &messages).unwrap(),
            HistoryState::Absent
        );
        let cleared = pending_pages(&data.db).unwrap().remove(0);
        assert!(cleared.first_attempt.is_none() && cleared.scan_before.is_none());
        assert!(begin_attempt(&data.db, &cleared, first + 180_000).unwrap());
        let next_attempt = pending_pages(&data.db).unwrap().remove(0);
        assert_eq!(next_attempt.first_attempt, Some(first + 180_000));
        assert_eq!(
            next_attempt.scan_before, None,
            "a failed new POST scans from the newest message again"
        );
    }

    #[test]
    fn reconciliation_receipt_after_resuming_prevents_another_post() {
        let data = data();
        target(&data, 7, 11, 1, Lang::Ru);
        game(&data, 7, 1, Lang::Ru, "2026-11-15", "кошка");
        enqueue_latest(&data, Utc.with_ymd_and_hms(2026, 11, 16, 0, 0, 0).unwrap()).unwrap();
        let page = pending_pages(&data.db).unwrap().remove(0);
        let first = 1_800_000_000_000;
        assert!(begin_attempt(&data.db, &page, first).unwrap());
        let page = pending_pages(&data.db).unwrap().remove(0);
        assert_eq!(
            history_step(
                &data.db,
                &page,
                serenity::UserId::new(42),
                &history_messages(1_000, 100, first + 60_000)
            )
            .unwrap(),
            HistoryState::Pending(901)
        );
        let page = pending_pages(&data.db).unwrap().remove(0);
        let mut messages = history_messages(900, 100, first + 30_000);
        messages[50].author.id = serenity::UserId::new(42);
        messages[50].author.bot = true;
        messages[50].nonce = Some(serenity::Nonce::String(page.nonce()));
        assert_eq!(
            history_step(&data.db, &page, serenity::UserId::new(42), &messages).unwrap(),
            HistoryState::Delivered
        );
        assert!(pending_pages(&data.db).unwrap().is_empty());
        assert!(!begin_attempt(&data.db, &page, first + 120_000).unwrap());
        let message: i64 = data
            .db
            .lock()
            .query_row(
                "SELECT message_id FROM word_summary_pages WHERE job_id = ?1 AND page = ?2",
                params![page.job, page.number],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(message, 850);
    }

    fn history_messages(newest: u64, count: usize, timestamp_ms: i64) -> Vec<serenity::Message> {
        (0..count)
            .map(|index| {
                let mut message = serenity::Message::default();
                message.id = serenity::MessageId::new(newest - index as u64);
                message.timestamp = serenity::Timestamp::from_unix_timestamp(timestamp_ms / 1000).unwrap();
                message
            })
            .collect()
    }

    #[test]
    fn prepare_upgrades_old_page_tables_with_a_nullable_scan_cursor() {
        let data = data();
        data.db
            .lock()
            .execute("ALTER TABLE word_summary_pages DROP COLUMN scan_before", [])
            .unwrap();
        prepare(&data.db).unwrap();
        prepare(&data.db).unwrap();
        target(&data, 7, 11, 1, Lang::Ru);
        game(&data, 7, 1, Lang::Ru, "2026-11-15", "кошка");
        enqueue_latest(&data, Utc.with_ymd_and_hms(2026, 11, 16, 0, 0, 0).unwrap()).unwrap();
        assert_eq!(pending_pages(&data.db).unwrap()[0].scan_before, None);
    }

    #[test]
    fn summary_images_are_bounded_for_many_players_and_show_only_six_per_page() {
        let data = data();
        let fonts = draw::fonts(&data.dir).expect("summary rendering requires the bot's normal fonts");
        let cards: Vec<_> = (0..100)
            .map(|i| wordboard::Card {
                name: format!("Игрок {i}"),
                avatar: None,
                rows: words::Game::from_row(Lang::Ru, "2026-11-15", "кошка".into(), "сосна,кошка").rows(),
            })
            .collect();
        let image = summary_image(fonts, Lang::Ru, "2026-11-15", 1, 17, &cards);
        assert!(image.width() <= 1600 && image.height() <= 700);
        assert_eq!(
            image.dimensions(),
            summary_image(fonts, Lang::Ru, "2026-11-15", 1, 17, &cards[..6]).dimensions()
        );
        assert!(draw::to_png(&image).len() < 1_000_000);
        let one = summary_image(fonts, Lang::Ru, "2026-11-15", 1, 1, &cards[..1]);
        let sample = summary_image(fonts, Lang::Ru, "2026-11-15", 1, 1, &cards[..2]);
        assert_eq!(one.dimensions(), (1000, 608));
        assert_eq!(sample.dimensions(), (1000, 608));
        let output = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/sample_word_summary.png");
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        sample.save(output).unwrap();
    }
}
