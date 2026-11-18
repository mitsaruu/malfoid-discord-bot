//! Патчноуты. Что нового — в файле patchnotes.txt (в папке бота), новая запись сверху.
//! После перезапуска бот сам публикует записи, которых ещё не было, в канал из /patchnotes_channel.

use crate::admin::bot_permissions;
use crate::reminders::{clear_delivery_retry, delivery_message, delivery_schema, retry_delivery};
use crate::reply::whisper;
use crate::{db, Context, Data, Error, Shared};
use poise::serenity_prelude as serenity;
use rusqlite::{params, OptionalExtension};
use tokio::sync::Mutex;

const MAX_AT_ONCE: usize = 3;
const PATCHNOTES_JOB: &str = "patchnotes";
static GUILD_LOCKS: [Mutex<()>; 64] = [const { Mutex::const_new(()) }; 64];

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub id: String, // номер, например "v5" — по нему бот понимает, что запись ещё не публиковал
    pub date: String,
    pub title: String,
    pub items: Vec<String>,
}

/// Текст patchnotes.txt → список записей, новая — первая. Заголовок: «## номер | дата | название».
pub fn parse(text: &str) -> Vec<Note> {
    let mut notes: Vec<Note> = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(header) = line.strip_prefix("## ") {
            let mut parts = header.split('|').map(|part| part.trim().to_string());
            let mut next = || parts.next().unwrap_or_default();
            notes.push(Note { id: next(), date: next(), title: next(), items: Vec::new() });
        } else if !line.is_empty() && !line.starts_with('#') {
            if let Some(note) = notes.last_mut() {
                let item = line.strip_prefix("- ").or_else(|| line.strip_prefix("• ")).unwrap_or(line);
                note.items.push(item.to_string());
            }
        }
    }
    notes.retain(|note| !note.id.is_empty());
    notes
}

fn load(data: &Data) -> Vec<Note> {
    parse(&std::fs::read_to_string(data.dir.join("patchnotes.txt")).unwrap_or_default())
}

/// Записи новее last_note — от старой к новой. Если last_note в файле не нашёлся — только самая новая.
pub fn unpublished<'a>(notes: &'a [Note], last_note: Option<&str>) -> Vec<&'a Note> {
    let known = last_note.and_then(|last| notes.iter().position(|note| note.id == last));
    let fresh = match known {
        Some(position) => &notes[..position],
        None => &notes[..notes.len().min(1)],
    };
    fresh.iter().rev().take(MAX_AT_ONCE).collect()
}

fn embed(note: &Note) -> serenity::CreateEmbed {
    let title = if note.title.is_empty() { format!("📝 Обновление {}", note.id) } else { format!("📝 Обновление {} — {}", note.id, note.title) };
    let mut text: String = note.items.iter().map(|item| format!("• {item}")).collect::<Vec<_>>().join("\n").chars().take(4000).collect();
    if text.is_empty() {
        text = "Мелкие исправления".to_string();
    }
    let embed = serenity::CreateEmbed::new().color(0x7C6CF2).title(title).description(text);
    if note.date.is_empty() { embed } else { embed.footer(serenity::CreateEmbedFooter::new(&note.date)) }
}

fn nonce(guild: u64, channel: u64, note: &str) -> String {
    let hash = guild.to_le_bytes().into_iter().chain(channel.to_le_bytes()).chain(note.bytes())
        .fold(0xcbf29ce484222325_u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3));
    format!("patch-{hash:016x}")
}

async fn publish_one(
    http: &serenity::Http, data: &Data, guild: u64, channel: u64, previous: Option<&str>, note: &Note,
) -> Result<bool, Error> {
    let message = serenity::CreateMessage::new().embed(embed(note)).allowed_mentions(serenity::CreateAllowedMentions::new());
    if let Err(error) = serenity::ChannelId::new(channel)
        .send_message(http, delivery_message(message, nonce(guild, channel, &note.id))).await
    {
        eprintln!("Патчноуты: не получилось написать в канал {channel}, будет повтор: {error}");
        retry_delivery(&data.db.lock(), PATCHNOTES_JOB, guild as i64, db::now())?;
        return Ok(false);
    }
    let mut connection = data.db.lock();
    let transaction = connection.transaction()?;
    let changed = transaction.execute(
        "UPDATE patchnotes_channel SET last_note = ?1 WHERE guild_id = ?2 AND channel_id = ?3 AND last_note IS ?4",
        params![note.id, guild as i64, channel as i64, previous],
    )?;
    if changed == 1 {
        clear_delivery_retry(&transaction, PATCHNOTES_JOB, guild as i64)?;
    }
    transaction.commit()?;
    Ok(changed == 1)
}

/// Публикует одну ограниченную пачку на сервер и оставляет курсор при отказе.
pub async fn post_new(http: &serenity::Http, data: &Data) -> Result<(), Error> {
    let notes = load(data);
    let guilds = {
        let connection = data.db.lock();
        delivery_schema(&connection)?;
        let mut statement = connection.prepare(
            "SELECT p.guild_id FROM patchnotes_channel p
             LEFT JOIN delivery_retry d ON d.job = 'patchnotes' AND d.id = p.guild_id
             WHERE p.guild_id > 0 AND p.channel_id > 0 AND COALESCE(d.retry_at, 0) <= ?1",
        )?;
        let rows = statement.query_map([db::now()], |row| row.get::<_, i64>(0).map(|id| id as u64))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for guild in guilds {
        let _update = GUILD_LOCKS[guild as usize % GUILD_LOCKS.len()].lock().await;
        let current = data.db.lock().query_row(
            "SELECT p.channel_id, p.last_note FROM patchnotes_channel p
             LEFT JOIN delivery_retry d ON d.job = 'patchnotes' AND d.id = p.guild_id
             WHERE p.guild_id = ?1 AND p.channel_id > 0 AND COALESCE(d.retry_at, 0) <= ?2",
            params![guild as i64, db::now()],
            |row| Ok((row.get::<_, i64>(0).map(|id| id as u64)?, row.get::<_, Option<String>>(1)?)),
        ).optional()?;
        let Some((channel, cursor)) = current else { continue };
        let mut previous = cursor.as_deref();
        for note in unpublished(&notes, previous) {
            if !publish_one(http, data, guild, channel, previous, note).await? {
                break;
            }
            previous = Some(&note.id);
        }
    }
    Ok(())
}

pub fn start_loop(http: std::sync::Arc<serenity::Http>, data: Shared) {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(30));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            if let Err(error) = post_new(&http, &data).await {
                eprintln!("Патчноуты: {error}");
            }
        }
    });
}

async fn version_autocomplete(ctx: Context<'_>, partial: &str) -> serenity::CreateAutocompleteResponse {
    let partial = partial.to_lowercase();
    let choices = load(ctx.data())
        .into_iter()
        .filter(|note| format!("{} {}", note.id, note.title).to_lowercase().contains(&partial))
        .take(25)
        .map(|note| {
            let label: String = format!("{} — {}", note.id, note.title).chars().take(100).collect();
            serenity::AutocompleteChoice::new(label, note.id)
        })
        .collect();
    serenity::CreateAutocompleteResponse::new().set_choices(choices)
}

/// Что нового в боте: последний патчноут или любой прошлый
#[poise::command(slash_command, rename = "patchnotes")]
pub async fn show(
    ctx: Context<'_>,
    #[description = "Какой патчноут показать (если не выбрать — последний)"]
    #[autocomplete = "version_autocomplete"]
    version: Option<String>,
) -> Result<(), Error> {
    let notes = load(ctx.data());
    let note = match &version {
        Some(version) => notes.iter().find(|note| note.id == *version),
        None => notes.first(),
    };
    match note {
        Some(note) => {
            ctx.send(poise::CreateReply::default().embed(embed(note)).ephemeral(true)).await?;
            Ok(())
        }
        None => whisper(ctx, "Такого патчноута нет.").await,
    }
}

/// Канал, куда бот пишет патчноуты после обновлений (пусто — не писать)
#[poise::command(slash_command, guild_only, required_permissions = "MANAGE_GUILD", default_member_permissions = "MANAGE_GUILD", rename = "patchnotes_channel")]
pub async fn set_channel(
    ctx: Context<'_>,
    #[description = "Канал для патчноутов. Пусто — выключить"]
    #[channel_types("Text")]
    channel: Option<serenity::GuildChannel>,
) -> Result<(), Error> {
    let (data, guild) = (ctx.data(), ctx.guild_id().unwrap().get());
    let Some(channel) = channel else {
        {
            let _update = GUILD_LOCKS[guild as usize % GUILD_LOCKS.len()].lock().await;
            configure(&data.db, guild, None)?;
        }
        return whisper(ctx, "✅ Больше не пишу патчноуты.").await;
    };
    let allowed = bot_permissions(ctx, &channel).is_some_and(|p| p.view_channel() && p.send_messages() && p.embed_links());
    if !allowed {
        return whisper(ctx, format!(
            "Мне не хватает прав в <#{}>: нужны «Просматривать канал», «Отправлять сообщения» и «Встраивать ссылки».",
            channel.id)).await;
    }
    let notes = load(data);
    let delivered = {
        let _update = GUILD_LOCKS[guild as usize % GUILD_LOCKS.len()].lock().await;
        configure(&data.db, guild, Some(channel.id.get()))?;
        match notes.first() {
            Some(latest) => publish_one(ctx.http(), data, guild, channel.id.get(), None, latest).await?,
            None => true,
        }
    };
    let pending = if delivered { "" } else { " Последний патчноут отправится при следующей успешной попытке." };
    whisper(ctx, format!(
        "✅ Теперь после каждого обновления пишу патчноут в <#{}>. Выключить — /patchnotes_channel без канала.{pending}",
        channel.id)).await
}

fn configure(db: &db::Database, guild: u64, channel: Option<u64>) -> rusqlite::Result<()> {
    let mut connection = db.lock();
    delivery_schema(&connection)?;
    let transaction = connection.transaction()?;
    match channel {
        Some(channel) => transaction.execute(
            "INSERT INTO patchnotes_channel (guild_id, channel_id, last_note) VALUES (?1, ?2, NULL)
             ON CONFLICT (guild_id) DO UPDATE SET channel_id = excluded.channel_id, last_note = NULL",
            params![guild as i64, channel as i64],
        )?,
        None => transaction.execute("DELETE FROM patchnotes_channel WHERE guild_id = ?1", [guild as i64])?,
    };
    clear_delivery_retry(&transaction, PATCHNOTES_JOB, guild as i64)?;
    transaction.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "# Патчноуты бота\n\n## v3 | 10.11.2026 | Третье\n- раз\n• два\nтри\n\n## v2 | 09.11.2026 | Второе\n- а\n\n## v1 | 08.11.2026\n";

    #[test]
    fn parsing() {
        let notes = parse(FILE);
        assert_eq!(notes.len(), 3);
        assert_eq!(notes[0], Note { id: "v3".into(), date: "10.11.2026".into(), title: "Третье".into(),
                                    items: vec!["раз".into(), "два".into(), "три".into()] });
        assert_eq!((notes[2].id.as_str(), notes[2].title.as_str(), notes[2].items.len()), ("v1", "", 0));
        assert!(parse("строка без заголовка\n## | пусто").is_empty());
    }

    #[test]
    fn what_to_publish() {
        let notes = parse(FILE);
        let ids = |last: Option<&str>| unpublished(&notes, last).iter().map(|note| note.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(Some("v1")), ["v2", "v3"]); // от старой к новой
        assert!(ids(Some("v3")).is_empty()); // всё уже опубликовано
        assert_eq!(ids(Some("v0")), ["v3"]); // такой записи нет — только самая новая
        assert_eq!(ids(None), ["v3"]);
        assert!(unpublished(&[], None).is_empty());
    }


    #[test]
    fn backlog_starts_with_oldest_pending_entries() {
        let notes = parse("## v6 | date\n## v5 | date\n## v4 | date\n## v3 | date\n## v2 | date\n## v1 | date");
        let ids: Vec<_> = unpublished(&notes, Some("v1")).into_iter().map(|note| note.id.as_str()).collect();
        assert_eq!(ids, ["v2", "v3", "v4"]);
    }

    #[test]
    fn real_file_parses() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("patchnotes.txt");
        let notes = parse(&std::fs::read_to_string(path).unwrap());
        assert!(notes.len() >= 9 && notes.iter().all(|note| note.id.starts_with('v') && !note.items.is_empty()));
    }
}
