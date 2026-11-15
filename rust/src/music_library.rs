//! Durable selected-track snapshots, personal favorites and confirmed guild history.
use crate::{db, Context, Data, Error, Shared};
use poise::serenity_prelude as s;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::time::Duration;

const PAGE_SIZE: usize = 10;
const VIEW_SECONDS: f64 = 900.;
pub(crate) const MAX_BULK_FAVORITES: usize = 200;

#[derive(Clone, Debug)]
pub struct SavedTrack {
    pub id: i64,
    /// The selected playable page, never the user's search or album input.
    pub url: String,
    pub title: String,
    pub details: Value,
}

pub fn prepare(db: &db::Database) -> rusqlite::Result<()> {
    db.lock().execute_batch(
        "CREATE TABLE IF NOT EXISTS music_library_tracks (
            id INTEGER PRIMARY KEY AUTOINCREMENT, guild_id INTEGER NOT NULL,
            url TEXT NOT NULL, title TEXT NOT NULL, details TEXT NOT NULL, dedupe TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS music_favorites (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            guild_id INTEGER NOT NULL, user_id INTEGER NOT NULL,
            snapshot_id INTEGER NOT NULL, dedupe TEXT NOT NULL,
            UNIQUE(guild_id,user_id,dedupe)
         );
         CREATE TABLE IF NOT EXISTS music_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT, guild_id INTEGER NOT NULL,
            snapshot_id INTEGER NOT NULL, instance TEXT NOT NULL, played_at INTEGER NOT NULL,
            UNIQUE(guild_id,instance)
         );
         CREATE INDEX IF NOT EXISTS music_history_guild ON music_history(guild_id,id DESC);
         CREATE TABLE IF NOT EXISTS music_library_views (
            id TEXT PRIMARY KEY, guild_id INTEGER NOT NULL, user_id INTEGER NOT NULL,
            kind TEXT NOT NULL, page INTEGER NOT NULL, revision INTEGER NOT NULL,
            expires REAL NOT NULL, entries TEXT NOT NULL
         );",
    )
}

fn valid_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn dedupe_key(url: &str, details: &Value) -> String {
    // Spotify's parser also accepts free text and album URLs; validate the origin and
    // exact track path here before allowing it to identify a single selected song.
    if let Some(original) = details["original"].as_str() {
        if let Ok(parsed) = reqwest::Url::parse(original) {
            if valid_url(original) && parsed.host_str() == Some("open.spotify.com") {
                let mut parts: Vec<_> = parsed.path().trim_matches('/').split('/').collect();
                if parts.first().is_some_and(|p| p.starts_with("intl-")) {
                    parts.remove(0);
                }
                if let ["track", id] = parts.as_slice() {
                    if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()) {
                        return format!("https://open.spotify.com/track/{id}");
                    }
                }
            }
        }
    }
    url.to_owned()
}

/// Persist before displaying a heart. IDs never get reused, including after restart.
pub fn snapshot(
    db: &db::Database,
    guild: u64,
    url: &str,
    title: &str,
    details: &Value,
) -> Result<SavedTrack, Error> {
    if !valid_url(url) {
        return Err("У выбранного трека нет действительной ссылки.".into());
    }
    let conn = db.lock();
    conn.execute("INSERT INTO music_library_tracks(guild_id,url,title,details,dedupe) VALUES(?1,?2,?3,?4,?5)",
        params![guild as i64, url, title, details.to_string(), dedupe_key(url, details)])?;
    Ok(SavedTrack {
        id: conn.last_insert_rowid(),
        url: url.into(),
        title: title.into(),
        details: details.clone(),
    })
}

/// The guild and durable snapshot ID are bound to this displayed card, not current state.
pub fn favorite_button(guild: u64, snapshot_id: i64) -> s::CreateActionRow {
    s::CreateActionRow::Buttons(vec![s::CreateButton::new(format!(
        "library:favorite:{guild}:{snapshot_id}"
    ))
    .label("В избранное")
    .emoji('❤')
    .style(s::ButtonStyle::Secondary)])
}

/// Convenience hook for /nowplaying: persist exactly its displayed title/url/details.
pub fn favorite_button_for(
    db: &db::Database,
    guild: u64,
    url: &str,
    title: &str,
    details: &Value,
) -> Result<s::CreateActionRow, Error> {
    Ok(favorite_button(
        guild,
        snapshot(db, guild, url, title, details)?.id,
    ))
}

fn add_favorite(db: &db::Database, guild: u64, user: u64, id: i64) -> rusqlite::Result<bool> {
    Ok(db.lock().execute(
        "INSERT OR IGNORE INTO music_favorites(guild_id,user_id,snapshot_id,dedupe)
        SELECT guild_id,?2,id,dedupe FROM music_library_tracks WHERE guild_id=?1 AND id=?3",
        params![guild as i64, user as i64, id],
    )? > 0)
}

fn remove_favorite(db: &db::Database, guild: u64, user: u64, id: i64) -> rusqlite::Result<bool> {
    Ok(db.lock().execute(
        "DELETE FROM music_favorites WHERE guild_id=?1 AND user_id=?2 AND id=?3",
        params![guild as i64, user as i64, id],
    )? > 0)
}

fn favorite_collection(
    db: &db::Database,
    guild: u64,
    user: u64,
) -> rusqlite::Result<(Vec<SavedTrack>, usize)> {
    let conn = db.lock();
    let total = conn.query_row(
        "SELECT COUNT(*) FROM music_favorites WHERE guild_id=?1 AND user_id=?2",
        params![guild as i64, user as i64],
        |r| r.get::<_, i64>(0),
    )? as usize;
    let mut st = conn.prepare("SELECT f.id,t.id,t.url,t.title,t.details,NULL FROM music_favorites f
        JOIN music_library_tracks t ON t.id=f.snapshot_id WHERE f.guild_id=?1 AND f.user_id=?2 ORDER BY t.id DESC LIMIT ?3")?;
    let rows = st.query_map(
        params![guild as i64, user as i64, MAX_BULK_FAVORITES as i64],
        row_entry,
    )?;
    let tracks = rows
        .map(|r| r.map(|entry| entry.track))
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((tracks, total))
}

fn bulk_notice(count: usize, total: usize) -> String {
    if total > count {
        format!("➕ Добавлено {count} треков. Лимит за раз — {MAX_BULK_FAVORITES}; всего в избранном {total}.")
    } else {
        format!("➕ Добавлено твоих любимых треков: {count}.")
    }
}

/// Called with measured Songbird play_time, never a wall-clock delay or enqueue event.
/// A new instance token is required for every new TrackHandle, including repeats.
/// A later decoder error does not erase audio that already played past the threshold.
pub fn record_confirmed(
    db: &db::Database,
    guild: u64,
    snapshot_id: i64,
    instance: &str,
    progress: Duration,
    played_at: i64,
) -> rusqlite::Result<bool> {
    if progress <= crate::music::TRACK_TOO_SHORT {
        return Ok(false);
    }
    let mut conn = db.lock();
    let tx = conn.transaction()?;
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO music_history(guild_id,snapshot_id,instance,played_at)
        SELECT guild_id,id,?3,?4 FROM music_library_tracks WHERE guild_id=?1 AND id=?2",
        params![guild as i64, snapshot_id, instance, played_at],
    )? > 0;
    if inserted {
        tx.execute(
            "DELETE FROM music_history WHERE guild_id=?1 AND id NOT IN
            (SELECT id FROM music_history WHERE guild_id=?1 ORDER BY id DESC LIMIT 500)",
            [guild as i64],
        )?;
    }
    tx.commit()?;
    Ok(inserted)
}

/// The queue's enqueue function creates the new request_id; never inherit an old requester.
pub(crate) fn enqueue_track(
    data: &Data,
    guild: u64,
    user: u64,
    track: &SavedTrack,
) -> Result<(), Error> {
    if !valid_url(&track.url) {
        return Err("Сохранённая ссылка больше недействительна.".into());
    }
    let mut details = if track.details.is_object() {
        track.details.clone()
    } else {
        json!({})
    };
    details["requester"] = user.into();
    details["resolved"] = true.into();
    crate::music_qol::enqueue(data, guild, &track.url, &track.title, &details, false)
}

#[derive(Clone, Copy)]
enum Kind {
    Favorites,
    History,
}
impl Kind {
    fn key(self) -> &'static str {
        match self {
            Self::Favorites => "favorites",
            Self::History => "history",
        }
    }
    fn from_key(key: &str) -> Self {
        if key == "history" {
            Self::History
        } else {
            Self::Favorites
        }
    }
}

struct Entry {
    id: i64,
    track: SavedTrack,
    played_at: Option<i64>,
}
struct Page {
    entries: Vec<Entry>,
    total: usize,
    number: usize,
}

fn row_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    let details: String = row.get(4)?;
    Ok(Entry {
        id: row.get(0)?,
        track: SavedTrack {
            id: row.get(1)?,
            url: row.get(2)?,
            title: row.get(3)?,
            details: serde_json::from_str(&details).unwrap_or(Value::Null),
        },
        played_at: row.get(5)?,
    })
}

fn page(
    db: &db::Database,
    kind: Kind,
    guild: u64,
    user: u64,
    number: usize,
) -> rusqlite::Result<Page> {
    let conn = db.lock();
    let (count_sql, sql) = match kind {
        Kind::Favorites => (
            "SELECT COUNT(*) FROM music_favorites WHERE guild_id=?1 AND user_id=?2",
            "SELECT f.id,t.id,t.url,t.title,t.details,NULL FROM music_favorites f JOIN music_library_tracks t ON t.id=f.snapshot_id
             WHERE f.guild_id=?1 AND f.user_id=?2 ORDER BY t.id DESC LIMIT ?3 OFFSET ?4"),
        Kind::History => (
            "SELECT COUNT(*) FROM music_history WHERE guild_id=?1 AND ?2=?2",
            "SELECT h.id,t.id,t.url,t.title,t.details,h.played_at FROM music_history h JOIN music_library_tracks t ON t.id=h.snapshot_id
             WHERE h.guild_id=?1 AND ?2=?2 ORDER BY h.id DESC LIMIT ?3 OFFSET ?4"),
    };
    let total = conn.query_row(count_sql, params![guild as i64, user as i64], |r| {
        r.get::<_, i64>(0)
    })? as usize;
    let number = number.min(total.saturating_sub(1) / PAGE_SIZE);
    let mut st = conn.prepare(sql)?;
    let rows = st.query_map(
        params![
            guild as i64,
            user as i64,
            PAGE_SIZE as i64,
            (number * PAGE_SIZE) as i64
        ],
        row_entry,
    )?;
    Ok(Page {
        entries: rows.collect::<rusqlite::Result<_>>()?,
        total,
        number,
    })
}

fn entry(
    db: &db::Database,
    kind: Kind,
    guild: u64,
    user: u64,
    id: i64,
) -> rusqlite::Result<Option<Entry>> {
    let sql = match kind {
        Kind::Favorites => "SELECT f.id,t.id,t.url,t.title,t.details,NULL FROM music_favorites f JOIN music_library_tracks t ON t.id=f.snapshot_id
            WHERE f.guild_id=?1 AND f.user_id=?2 AND f.id=?3",
        Kind::History => "SELECT h.id,t.id,t.url,t.title,t.details,h.played_at FROM music_history h JOIN music_library_tracks t ON t.id=h.snapshot_id
            WHERE h.guild_id=?1 AND ?2=?2 AND h.id=?3",
    };
    db.lock()
        .query_row(sql, params![guild as i64, user as i64, id], row_entry)
        .optional()
}

struct View {
    id: String,
    guild: u64,
    user: u64,
    kind: Kind,
    page: usize,
    revision: i64,
    entries: Vec<i64>,
}

fn new_view(
    db: &db::Database,
    kind: Kind,
    guild: u64,
    user: u64,
    now: f64,
) -> rusqlite::Result<View> {
    let id = format!("{:032x}", rand::random::<u128>());
    let conn = db.lock();
    conn.execute("DELETE FROM music_library_views WHERE expires<=?1", [now])?;
    conn.execute("INSERT INTO music_library_views(id,guild_id,user_id,kind,page,revision,expires,entries) VALUES(?1,?2,?3,?4,0,0,?5,'[]')",
        params![id, guild as i64, user as i64, kind.key(), now + VIEW_SECONDS])?;
    Ok(View {
        id,
        guild,
        user,
        kind,
        page: 0,
        revision: 0,
        entries: vec![],
    })
}

fn claim_view(
    db: &db::Database,
    id: &str,
    guild: u64,
    user: u64,
    revision: i64,
    now: f64,
) -> rusqlite::Result<Option<View>> {
    let conn = db.lock();
    let found = conn.query_row("SELECT kind,page,entries FROM music_library_views WHERE id=?1 AND guild_id=?2 AND user_id=?3 AND revision=?4 AND expires>?5",
        params![id, guild as i64, user as i64, revision, now], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))
        }).optional()?;
    let Some((kind, page, entries)) = found else {
        return Ok(None);
    };
    conn.execute(
        "UPDATE music_library_views SET revision=revision+1 WHERE id=?1",
        [id],
    )?;
    Ok(Some(View {
        id: id.into(),
        guild,
        user,
        kind: Kind::from_key(&kind),
        page: page.max(0) as usize,
        revision: revision + 1,
        entries: serde_json::from_str(&entries).unwrap_or_default(),
    }))
}

/// Synchronous, short SQLite claim before choosing type 6 acknowledgement.
/// Passing the returned view to the handler avoids claiming its revision twice.
fn claim_callback(
    db: &db::Database,
    custom_id: &str,
    guild: Option<u64>,
    user: u64,
    now: f64,
) -> rusqlite::Result<Option<View>> {
    let parts: Vec<_> = custom_id.split(':').collect();
    let ["library", "view", id, revision, _] = parts.as_slice() else {
        return Ok(None);
    };
    let (Some(guild), Ok(revision)) = (guild, revision.parse::<i64>()) else {
        return Ok(None);
    };
    claim_view(db, id, guild, user, revision, now)
}

fn short(text: &str, limit: usize) -> String {
    let mut remaining = limit;
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take_while(|c| {
            if c.len_utf16() > remaining {
                return false;
            }
            remaining -= c.len_utf16();
            true
        })
        .collect()
}

fn render(
    db: &db::Database,
    view: &mut View,
) -> rusqlite::Result<(String, Vec<s::CreateActionRow>)> {
    let page = page(db, view.kind, view.guild, view.user, view.page)?;
    view.page = page.number;
    view.entries = page.entries.iter().map(|entry| entry.id).collect();
    db.lock().execute(
        "UPDATE music_library_views SET page=?2,entries=?3 WHERE id=?1 AND revision=?4",
        params![
            view.id,
            view.page as i64,
            json!(view.entries).to_string(),
            view.revision
        ],
    )?;
    let heading = match view.kind {
        Kind::Favorites => "❤️ Твоё избранное",
        Kind::History => "🕘 История сервера",
    };
    let mut text = format!(
        "{heading} · {} · страница {}/{}\n",
        page.total,
        page.number + 1,
        page.total.saturating_sub(1) / PAGE_SIZE + 1
    );
    for (n, entry) in page.entries.iter().enumerate() {
        text.push_str(&format!(
            "\n{}. {}",
            page.number * PAGE_SIZE + n + 1,
            short(&entry.track.title, 95)
        ));
        if let Some(time) = entry.played_at {
            let author = entry.track.details["requester"]
                .as_u64()
                .map_or("автоплей / старая заявка".into(), |id| {
                    format!("<@{id}>")
                });
            text.push_str(&format!(" · <t:{time}:t> · {author}"));
        }
    }
    let prefix = format!("library:view:{}:{}", view.id, view.revision);
    let mut rows = vec![];
    if !page.entries.is_empty() {
        let options: Vec<_> = page
            .entries
            .iter()
            .map(|e| {
                s::CreateSelectMenuOption::new(
                    {
                        let title = short(&e.track.title, 95);
                        if title.is_empty() {
                            "Без названия".into()
                        } else {
                            title
                        }
                    },
                    e.id.to_string(),
                )
            })
            .collect();
        rows.push(s::CreateActionRow::SelectMenu(
            s::CreateSelectMenu::new(
                format!("{prefix}:play"),
                s::CreateSelectMenuKind::String {
                    options: options.clone(),
                },
            )
            .placeholder("Добавить снова в очередь"),
        ));
        if matches!(view.kind, Kind::Favorites) {
            rows.push(s::CreateActionRow::SelectMenu(
                s::CreateSelectMenu::new(
                    format!("{prefix}:remove"),
                    s::CreateSelectMenuKind::String { options },
                )
                .placeholder("Удалить из своего избранного"),
            ));
        }
    } else {
        text.push_str("\nПока пусто.");
    }
    rows.push(s::CreateActionRow::Buttons(vec![
        s::CreateButton::new(format!("{prefix}:prev"))
            .label("←")
            .disabled(page.number == 0),
        s::CreateButton::new(format!("{prefix}:next"))
            .label("→")
            .disabled((page.number + 1) * PAGE_SIZE >= page.total),
    ]));
    if matches!(view.kind, Kind::Favorites) && page.total > 0 {
        rows.push(s::CreateActionRow::Buttons(vec![s::CreateButton::new(
            format!("{prefix}:all"),
        )
        .label("Играть всё")
        .style(s::ButtonStyle::Primary)]));
    }
    Ok((text, rows))
}

async fn show(ctx: Context<'_>, kind: Kind) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let mut view = new_view(
        &ctx.data().db,
        kind,
        ctx.guild_id().unwrap().get(),
        ctx.author().id.get(),
        db::now(),
    )?;
    let (text, rows) = render(&ctx.data().db, &mut view)?;
    ctx.send(
        poise::CreateReply::default()
            .content(text)
            .components(rows)
            .ephemeral(true)
            .allowed_mentions(s::CreateAllowedMentions::new()),
    )
    .await?;
    Ok(())
}

/// Личные любимые треки: воспроизведение и удаление
#[poise::command(slash_command, guild_only)]
pub async fn favorites(
    ctx: Context<'_>,
    #[description = "Добавить всё избранное в очередь (до 200 треков)"] play_all: Option<bool>,
) -> Result<(), Error> {
    if play_all != Some(true) {
        return show(ctx, Kind::Favorites).await;
    }
    ctx.defer_ephemeral().await?;
    let (tracks, total) = favorite_collection(
        &ctx.data().db,
        ctx.guild_id().unwrap().get(),
        ctx.author().id.get(),
    )?;
    if tracks.is_empty() {
        ctx.say("В твоём избранном пока пусто.").await?;
        return Ok(());
    }
    let count = crate::music::enqueue_saved_many(
        ctx.serenity_context(),
        ctx.data(),
        ctx.guild_id().unwrap(),
        ctx.author().id,
        ctx.channel_id(),
        &tracks,
    )
    .await?;
    ctx.send(
        poise::CreateReply::default()
            .content(bulk_notice(count, total))
            .ephemeral(true)
            .allowed_mentions(s::CreateAllowedMentions::new()),
    )
    .await?;
    Ok(())
}

/// Последние действительно игравшие треки сервера
#[poise::command(slash_command, guild_only)]
pub async fn history(ctx: Context<'_>) -> Result<(), Error> {
    show(ctx, Kind::History).await
}

/// Owned, live views defer an update to their existing private list. Hearts and
/// rejected views defer a new private response, never editing someone else's UI.
pub async fn pressed(ctx: &s::Context, data: &Shared, press: &s::ComponentInteraction) -> bool {
    if !press.data.custom_id.starts_with("library:") {
        return false;
    }
    let claimed = match claim_callback(
        &data.db,
        &press.data.custom_id,
        press.guild_id.map(|id| id.get()),
        press.user.id.get(),
        db::now(),
    ) {
        Ok(view) => view,
        Err(error) => {
            println!("Музыкальная библиотека: {error}");
            None
        }
    };
    let acknowledgement = if claimed.is_some() {
        s::CreateInteractionResponse::Acknowledge
    } else {
        s::CreateInteractionResponse::Defer(
            s::CreateInteractionResponseMessage::new().ephemeral(true),
        )
    };
    if let Err(error) = press.create_response(ctx, acknowledgement).await {
        println!(
            "Музыкальная библиотека: {}",
            crate::interactions::sanitize_error(&error.to_string())
        );
        return true;
    }
    if let Err(error) = handle_press(ctx, data, press, claimed).await {
        let _ = press
            .edit_response(
                ctx,
                s::EditInteractionResponse::new()
                    .content(format!(
                        "⚠️ {}",
                        short(
                            &crate::interactions::sanitize_error(&error.to_string()),
                            1400
                        )
                    ))
                    .allowed_mentions(s::CreateAllowedMentions::new()),
            )
            .await;
    }
    true
}

async fn respond(
    ctx: &s::Context,
    press: &s::ComponentInteraction,
    text: &str,
) -> Result<(), Error> {
    press
        .edit_response(
            ctx,
            s::EditInteractionResponse::new()
                .content(text)
                .allowed_mentions(s::CreateAllowedMentions::new()),
        )
        .await?;
    Ok(())
}

async fn handle_press(
    ctx: &s::Context,
    data: &Shared,
    press: &s::ComponentInteraction,
    claimed: Option<View>,
) -> Result<(), Error> {
    let Some(guild) = press.guild_id else {
        return respond(ctx, press, "Эта кнопка доступна только на сервере.").await;
    };
    let parts: Vec<_> = press.data.custom_id.split(':').collect();
    match parts.as_slice() {
        ["library", "favorite", bound_guild, id] => {
            if bound_guild.parse::<u64>().ok() != Some(guild.get()) {
                return respond(ctx, press, "Кнопка с другого сервера.").await;
            }
            let id = id.parse::<i64>()?;
            let exists = data
                .db
                .lock()
                .query_row(
                    "SELECT 1 FROM music_library_tracks WHERE guild_id=?1 AND id=?2",
                    params![guild.get() as i64, id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                return respond(ctx, press, "Этот трек больше недоступен.").await;
            }
            let added = add_favorite(&data.db, guild.get(), press.user.id.get(), id)?;
            respond(
                ctx,
                press,
                if added {
                    "❤️ Добавлено в твоё избранное."
                } else {
                    "❤️ Этот трек уже в твоём избранном."
                },
            )
            .await
        }
        ["library", "view", _, _, action] => {
            let Some(mut view) = claimed else {
                return respond(
                    ctx,
                    press,
                    "Чужая или устаревшая кнопка. Открой /favorites или /history заново.",
                )
                .await;
            };
            let mut notice = String::new();
            match *action {
                "prev" => view.page = view.page.saturating_sub(1),
                "next" => view.page = view.page.saturating_add(1),
                "all" => {
                    if !matches!(view.kind, Kind::Favorites) {
                        return respond(ctx, press, "Открой /favorites.").await;
                    }
                    let (tracks, total) = favorite_collection(&data.db, view.guild, view.user)?;
                    if tracks.is_empty() {
                        notice = "В твоём избранном пока пусто.\n\n".into();
                    } else {
                        notice = match crate::music::enqueue_saved_many(
                            ctx,
                            data,
                            guild,
                            press.user.id,
                            press.channel_id,
                            &tracks,
                        )
                        .await
                        {
                            Ok(count) => format!("{}\n\n", bulk_notice(count, total)),
                            Err(error) => format!(
                                "⚠️ {}\n\n",
                                short(
                                    &crate::interactions::sanitize_error(&error.to_string()),
                                    180
                                )
                            ),
                        };
                    }
                }
                "play" | "remove" => {
                    let selected = match &press.data.kind {
                        s::ComponentInteractionDataKind::StringSelect { values }
                            if values.len() == 1 =>
                        {
                            values[0].parse::<i64>().ok()
                        }
                        _ => None,
                    };
                    let Some(selected) = selected.filter(|id| view.entries.contains(id)) else {
                        return respond(ctx, press, "Выбор устарел. Открой список заново.").await;
                    };
                    let Some(selected) =
                        entry(&data.db, view.kind, view.guild, view.user, selected)?
                    else {
                        return respond(ctx, press, "Трек уже удалён. Открой список заново.").await;
                    };
                    if *action == "remove" {
                        if !matches!(view.kind, Kind::Favorites) {
                            return respond(ctx, press, "Историю нельзя удалить этой кнопкой.")
                                .await;
                        }
                        remove_favorite(&data.db, view.guild, view.user, selected.id)?;
                        notice = "🗑️ Удалено из твоего избранного.\n\n".into();
                    } else {
                        match crate::music::enqueue_saved(
                            ctx,
                            data,
                            guild,
                            press.user.id,
                            press.channel_id,
                            &selected.track,
                        )
                        .await
                        {
                            Ok(()) => notice = "➕ Трек добавлен с твоим авторством.\n\n".into(),
                            Err(error) => {
                                notice = format!(
                                    "⚠️ {}\n\n",
                                    short(
                                        &crate::interactions::sanitize_error(&error.to_string()),
                                        180
                                    )
                                )
                            }
                        }
                    }
                }
                _ => return respond(ctx, press, "Неизвестная кнопка. Открой список заново.").await,
            }
            let (text, rows) = render(&data.db, &mut view)?;
            press
                .edit_response(
                    ctx,
                    s::EditInteractionResponse::new()
                        .content(format!("{notice}{text}"))
                        .components(rows)
                        .allowed_mentions(s::CreateAllowedMentions::new()),
                )
                .await?;
            Ok(())
        }
        _ => respond(ctx, press, "Кнопка устарела. Открой список заново.").await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn database() -> db::Database {
        let db = db::Database::in_memory().unwrap();
        prepare(&db).unwrap();
        prepare(&db).unwrap();
        db
    }

    fn song(db: &db::Database, guild: u64, url: &str, original: &str) -> SavedTrack {
        snapshot(
            db,
            guild,
            url,
            "Selected song",
            &json!({
                "original": original, "requester": 42, "request_id": 99,
                "duration": 200, "spotify_query": "Artist song", "resolved": true
            }),
        )
        .unwrap()
    }

    // Removing owner/guild predicates must expose or delete someone else's favorite.
    #[test]
    fn favorites_dedupe_and_isolate_owners_and_guilds() {
        let db = database();
        let first = song(
            &db,
            1,
            "https://youtube.com/watch?v=one",
            "https://open.spotify.com/track/abc?si=old",
        );
        let second = song(
            &db,
            1,
            "https://youtube.com/watch?v=two",
            "https://open.spotify.com/intl-ru/track/abc?si=new",
        );
        assert!(add_favorite(&db, 1, 10, first.id).unwrap());
        assert!(!add_favorite(&db, 1, 10, second.id).unwrap());
        assert!(add_favorite(&db, 1, 11, first.id).unwrap());
        assert!(!add_favorite(&db, 2, 10, first.id).unwrap());
        assert_eq!(
            page(&db, Kind::Favorites, 1, 10, 0).unwrap().entries.len(),
            1
        );
        assert!(page(&db, Kind::Favorites, 2, 10, 0)
            .unwrap()
            .entries
            .is_empty());
        let own_id = page(&db, Kind::Favorites, 1, 10, 0).unwrap().entries[0].id;
        let other_id = page(&db, Kind::Favorites, 1, 11, 0).unwrap().entries[0].id;
        assert!(!remove_favorite(&db, 1, 12, own_id).unwrap());
        assert!(!remove_favorite(&db, 2, 10, own_id).unwrap());
        assert!(remove_favorite(&db, 1, 10, own_id).unwrap());
        assert!(entry(&db, Kind::Favorites, 1, 10, own_id)
            .unwrap()
            .is_none());
        assert!(entry(&db, Kind::Favorites, 1, 11, other_id)
            .unwrap()
            .is_some());
    }

    // Album/search originals must not collapse different selected songs or be replayed.
    #[test]
    fn snapshots_keep_selected_audio_and_reject_invalid_original_dedupe() {
        let db = database();
        for original in [
            "raw search",
            "https://open.spotify.com/album/abc",
            "https://open.spotify.com.evil/track/abc",
        ] {
            let a = song(&db, 1, "https://youtube.com/watch?v=one", original);
            let b = song(&db, 1, "https://youtube.com/watch?v=two", original);
            add_favorite(&db, 1, 10, a.id).unwrap();
            add_favorite(&db, 1, 10, b.id).unwrap();
        }
        let page = page(&db, Kind::Favorites, 1, 10, 0).unwrap();
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.entries[1].track.url, "https://youtube.com/watch?v=one");
        assert_eq!(page.entries[1].track.title, "Selected song");
        assert!(snapshot(&db, 1, "ytsearch1:bad", "bad", &json!({})).is_err());
    }

    // Progress gating and per-instance idempotence prevent queue/decoder failures and duplicates.
    #[test]
    fn history_confirms_real_progress_once_but_counts_repeats() {
        let db = database();
        let song = song(&db, 1, "https://youtube.com/watch?v=one", "search");
        for (instance, progress) in [("queued", 0), ("short", 3000), ("decoder", 100)] {
            assert!(!record_confirmed(
                &db,
                1,
                song.id,
                instance,
                Duration::from_millis(progress),
                100
            )
            .unwrap());
        }
        assert!(
            record_confirmed(&db, 1, song.id, "play1", Duration::from_millis(3020), 101).unwrap()
        );
        assert!(!record_confirmed(&db, 1, song.id, "play1", Duration::from_secs(8), 102).unwrap());
        assert!(record_confirmed(&db, 1, song.id, "repeat", Duration::from_secs(4), 103).unwrap());
        assert!(
            !record_confirmed(&db, 2, song.id, "wrong-guild", Duration::from_secs(4), 104).unwrap()
        );
        let history = page(&db, Kind::History, 1, 7, 0).unwrap();
        assert_eq!(history.total, 2);
        assert_eq!(history.entries[0].played_at, Some(103));
        assert_eq!(history.entries[0].track.details["requester"], 42);
        assert!(page(&db, Kind::History, 2, 7, 0)
            .unwrap()
            .entries
            .is_empty());
    }

    // Removing the guild-scoped prune or reversing the ordering breaks this boundary.
    #[test]
    fn history_prunes_to_500_per_guild_and_stale_ids_stay_missing() {
        let db = database();
        let a = song(&db, 1, "https://youtube.com/watch?v=one", "search");
        let b = song(&db, 2, "https://youtube.com/watch?v=two", "search");
        record_confirmed(&db, 2, b.id, "other", Duration::from_secs(4), 1).unwrap();
        record_confirmed(&db, 1, a.id, "first", Duration::from_secs(4), 2).unwrap();
        let stale = page(&db, Kind::History, 1, 1, 0).unwrap().entries[0].id;
        for n in 0..505 {
            record_confirmed(
                &db,
                1,
                a.id,
                &format!("play{n}"),
                Duration::from_secs(4),
                n + 3,
            )
            .unwrap();
        }
        let history = page(&db, Kind::History, 1, 1, 0).unwrap();
        assert_eq!(history.total, 500);
        assert_eq!(history.entries[0].played_at, Some(507));
        assert_eq!(page(&db, Kind::History, 2, 1, 0).unwrap().total, 1);
        assert!(entry(&db, Kind::History, 1, 1, stale).unwrap().is_none());
        assert_eq!(
            page(&db, Kind::History, 1, 1, usize::MAX).unwrap().number,
            49
        );
    }

    // Old hearts must resolve the displayed snapshot even after a different song starts.
    #[test]
    fn durable_heart_keeps_displayed_song() {
        let db = database();
        let old = song(&db, 1, "https://youtube.com/watch?v=old", "old query");
        let _new = song(&db, 1, "https://youtube.com/watch?v=new", "new query");
        add_favorite(&db, 1, 10, old.id).unwrap();
        assert_eq!(
            page(&db, Kind::Favorites, 1, 10, 0).unwrap().entries[0]
                .track
                .url,
            "https://youtube.com/watch?v=old"
        );
    }

    #[test]
    fn stale_favorite_remove_cannot_delete_a_readded_song() {
        let db = database();
        let saved = song(&db, 1, "https://youtube.com/watch?v=one", "search");
        add_favorite(&db, 1, 10, saved.id).unwrap();
        let old = page(&db, Kind::Favorites, 1, 10, 0).unwrap().entries[0].id;
        assert!(remove_favorite(&db, 1, 10, old).unwrap());
        add_favorite(&db, 1, 10, saved.id).unwrap();
        assert!(!remove_favorite(&db, 1, 10, old).unwrap());
        assert_eq!(page(&db, Kind::Favorites, 1, 10, 0).unwrap().total, 1);
        assert!(entry(&db, Kind::Favorites, 1, 10, old).unwrap().is_none());
    }

    #[test]
    fn views_reject_other_users_guilds_expired_and_replayed_callbacks() {
        let db = database();
        let mut view = new_view(&db, Kind::Favorites, 1, 10, 100.).unwrap();
        render(&db, &mut view).unwrap();
        assert!(claim_view(&db, &view.id, 1, 11, 0, 101.).unwrap().is_none());
        assert!(claim_view(&db, &view.id, 2, 10, 0, 101.).unwrap().is_none());
        assert!(claim_view(&db, &view.id, 1, 10, 0, 1001.)
            .unwrap()
            .is_none());
        assert!(claim_view(&db, &view.id, 1, 10, 0, 101.).unwrap().is_some());
        assert!(claim_view(&db, &view.id, 1, 10, 0, 101.).unwrap().is_none());
    }

    #[test]
    fn callback_routes_claim_only_owned_live_view_once_before_acknowledgement() {
        let db = database();
        let view = new_view(&db, Kind::Favorites, 1, 10, 100.).unwrap();
        let callback = format!("library:view:{}:0:next", view.id);
        assert!(claim_callback(&db, &callback, Some(1), 11, 101.)
            .unwrap()
            .is_none());
        assert!(claim_callback(&db, &callback, Some(2), 10, 101.)
            .unwrap()
            .is_none());
        assert!(claim_callback(&db, &callback, None, 10, 101.)
            .unwrap()
            .is_none());
        assert!(claim_callback(&db, &callback, Some(1), 10, 1001.)
            .unwrap()
            .is_none());
        assert!(
            claim_callback(&db, "library:favorite:1:1", Some(1), 10, 101.)
                .unwrap()
                .is_none()
        );
        let claimed = claim_callback(&db, &callback, Some(1), 10, 101.)
            .unwrap()
            .unwrap();
        assert_eq!(claimed.revision, 1);
        assert!(claim_callback(&db, &callback, Some(1), 10, 101.)
            .unwrap()
            .is_none());
    }

    #[test]
    fn terminal_decoder_error_after_three_and_a_half_seconds_is_history() {
        let db = database();
        let saved = song(&db, 1, "https://youtube.com/watch?v=one", "search");
        assert!(record_confirmed(
            &db,
            1,
            saved.id,
            "terminal-error",
            Duration::from_millis(3500),
            101
        )
        .unwrap());
        assert!(!record_confirmed(
            &db,
            1,
            saved.id,
            "terminal-error",
            Duration::from_secs(4),
            102
        )
        .unwrap());
        for progress in [0, 2999, 3000] {
            assert!(!record_confirmed(
                &db,
                1,
                saved.id,
                &format!("startup-error-{progress}"),
                Duration::from_millis(progress),
                103
            )
            .unwrap());
        }
        assert_eq!(page(&db, Kind::History, 1, 10, 0).unwrap().total, 1);
    }

    // The actual queue insertion must reset author/request id while retaining selected media.
    #[test]
    fn requeue_uses_new_requester_and_request_id() {
        let db = database();
        let song = song(
            &db,
            1,
            "https://youtube.com/watch?v=one",
            "https://open.spotify.com/album/abc",
        );
        let data = crate::Data::new(
            db,
            &crate::config::Settings::default(),
            Default::default(),
            false,
        );
        enqueue_track(&data, 1, 77, &song).unwrap();
        enqueue_track(&data, 1, 88, &song).unwrap();
        let rows = crate::music_qol::items(&data, 1).unwrap();
        assert_eq!(rows[0].url, "https://youtube.com/watch?v=one");
        assert_eq!(rows[0].details["requester"], 77);
        assert_eq!(rows[1].details["requester"], 88);
        assert_ne!(rows[0].details["request_id"], 99);
        assert_ne!(rows[0].details["request_id"], rows[1].details["request_id"]);
        assert_eq!(rows[0].details["duration"], 200);
        assert_eq!(song.details["requester"], 42);
    }

    // Bulk collection must remain bounded and cannot include another owner/guild.
    #[test]
    fn play_all_collects_at_most_200_personal_tracks_with_fresh_requests() {
        let db = database();
        for n in 0..201 {
            let song = song(
                &db,
                1,
                &format!("https://youtube.com/watch?v={n}"),
                "search",
            );
            add_favorite(&db, 1, 10, song.id).unwrap();
        }
        let other = song(&db, 2, "https://youtube.com/watch?v=other", "search");
        add_favorite(&db, 2, 10, other.id).unwrap();
        let (tracks, total) = favorite_collection(&db, 1, 10).unwrap();
        assert_eq!(total, 201);
        assert_eq!(tracks.len(), 200);
        assert_eq!(tracks[0].url, "https://youtube.com/watch?v=200");
        assert!(favorite_collection(&db, 1, 11).unwrap().0.is_empty());
        let data = crate::Data::new(
            db,
            &crate::config::Settings::default(),
            Default::default(),
            false,
        );
        for track in &tracks {
            enqueue_track(&data, 1, 77, track).unwrap();
        }
        let rows = crate::music_qol::items(&data, 1).unwrap();
        assert_eq!(rows.len(), 200);
        assert!(rows.iter().all(|row| row.details["requester"] == 77));
        let ids: std::collections::HashSet<_> = rows
            .iter()
            .map(|row| row.details["request_id"].as_u64().unwrap())
            .collect();
        assert_eq!(ids.len(), 200);
    }

    // Astral Unicode must not overflow Discord's text or select-label limits.
    #[test]
    fn pages_keep_unicode_labels_and_message_within_discord_limits() {
        let db = database();
        for n in 0..10 {
            let saved = snapshot(
                &db,
                1,
                &format!("https://youtube.com/watch?v={n}"),
                &"🎵".repeat(200),
                &json!({"requester": u64::MAX}),
            )
            .unwrap();
            record_confirmed(
                &db,
                1,
                saved.id,
                &format!("{n}"),
                Duration::from_secs(4),
                1794787200,
            )
            .unwrap();
        }
        let mut view = new_view(&db, Kind::History, 1, 10, 100.).unwrap();
        let (text, rows) = render(&db, &mut view).unwrap();
        assert!(text.encode_utf16().count() <= 1800);
        let payload = serde_json::to_value(rows).unwrap();
        let options = payload[0]["components"][0]["options"].as_array().unwrap();
        assert_eq!(options.len(), 10);
        assert!(options.iter().all(|option| option["label"]
            .as_str()
            .unwrap()
            .encode_utf16()
            .count()
            <= 100));
    }
}
