//! Напоминания: /remind и /reminders. Раз в 20 секунд бот проверяет, не пришло ли время.

use crate::reply::whisper;
use crate::{db::{self, Database}, Context, Data, Error};
use poise::serenity_prelude as serenity;
use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::Mutex;

const DELIVERY_BATCH: usize = 32;
const REMINDER_JOB: &str = "reminder";
static REMINDER_LOCKS: [Mutex<()>; 64] = [const { Mutex::const_new(()) }; 64];

pub(crate) fn delivery_schema(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS delivery_retry (
            job TEXT NOT NULL, id INTEGER NOT NULL, failures INTEGER NOT NULL, retry_at REAL NOT NULL,
            PRIMARY KEY (job, id)
        );",
    )
}

pub(crate) fn retry_delay(failures: i64) -> f64 {
    (30_u64 << failures.saturating_sub(1).clamp(0, 7)).min(3600) as f64
}

pub(crate) fn retry_delivery(connection: &Connection, job: &str, id: i64, now: f64) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO delivery_retry (job, id, failures, retry_at) VALUES (?1, ?2, 1, ?3 + ?4)
         ON CONFLICT (job, id) DO UPDATE SET
             failures = MIN(failures + 1, 8),
             retry_at = ?3 + MIN(30 * (1 << MIN(failures, 7)), 3600)",
        params![job, id, now, retry_delay(1)],
    )?;
    Ok(())
}

pub(crate) fn clear_delivery_retry(connection: &Connection, job: &str, id: i64) -> rusqlite::Result<()> {
    connection.execute("DELETE FROM delivery_retry WHERE job = ?1 AND id = ?2", params![job, id])?;
    Ok(())
}

pub(crate) fn destination_gone(error: &serenity::Error) -> bool {
    matches!(error, serenity::Error::Http(serenity::HttpError::UnsuccessfulRequest(response))
        if matches!(response.error.code, 10003 | 10008))
}

pub(crate) fn delivery_message(message: serenity::CreateMessage, nonce: String) -> serenity::CreateMessage {
    message.nonce(serenity::Nonce::String(nonce)).enforce_nonce(true)
}

fn insert_reminder(db: &Database, guild: u64, channel: u64, user: u64, due: f64, text: &str) -> rusqlite::Result<bool> {
    let changes = db.lock().execute(
        "INSERT INTO reminders (guild_id, channel_id, user_id, due, text)
         SELECT ?1, ?2, ?3, ?4, ?5
         WHERE (SELECT COUNT(*) FROM reminders WHERE guild_id = ?1 AND user_id = ?3) < ?6",
        params![guild as i64, channel as i64, user as i64, due, text, MAX_REMINDERS],
    )?;
    Ok(changes == 1)
}

fn finish_reminder(db: &Database, id: i64) -> rusqlite::Result<bool> {
    let mut connection = db.lock();
    delivery_schema(&connection)?;
    let transaction = connection.transaction()?;
    let changed = transaction.execute("DELETE FROM reminders WHERE id = ?1", [id])?;
    clear_delivery_retry(&transaction, REMINDER_JOB, id)?;
    transaction.commit()?;
    Ok(changed == 1)
}

fn reminder_batch(db: &Database, now: f64) -> rusqlite::Result<Vec<i64>> {
    let connection = db.lock();
    delivery_schema(&connection)?;
    connection.execute_batch(
        "CREATE INDEX IF NOT EXISTS reminders_due ON reminders (due);
         CREATE INDEX IF NOT EXISTS reminders_owner_due ON reminders (guild_id, user_id, due);",
    )?;
    let mut statement = connection.prepare(
        "SELECT r.id FROM reminders r
         LEFT JOIN delivery_retry d ON d.job = 'reminder' AND d.id = r.id
         WHERE r.due <= ?1 AND COALESCE(d.retry_at, 0) <= ?1 ORDER BY r.due, r.id LIMIT ?2",
    )?;
    let rows = statement.query_map(params![now, DELIVERY_BATCH as i64], |row| row.get(0))?;
    rows.collect()
}

const MAX_REMINDERS: i64 = 10; // сколько напоминаний может ждать у одного человека

/// «2ч 30м», «45m», «1d», «90» (минуты) → секунды. Не получилось разобрать — None.
pub fn parse_duration(text: &str) -> Option<i64> {
    let characters: Vec<char> = text.trim().to_lowercase().chars().collect();
    let (mut total, mut index) = (0i64, 0);
    while index < characters.len() {
        if characters[index].is_whitespace() {
            index += 1;
            continue;
        }
        let start = index;
        while index < characters.len() && characters[index].is_ascii_digit() {
            index += 1;
        }
        if start == index {
            return None; // ждали число, а там что-то другое
        }
        let amount: i64 = characters[start..index].iter().collect::<String>().parse().ok()?;
        while index < characters.len() && characters[index].is_whitespace() {
            index += 1;
        }
        let unit_start = index;
        while index < characters.len() && characters[index].is_alphabetic() {
            index += 1;
        }
        // Единица — по первой букве: с/s — секунды, м/m — минуты, ч/h — часы, д/d — дни. Без единицы — минуты.
        let unit = match characters[unit_start..index].first() {
            None | Some('м') | Some('m') => 60,
            Some('с') | Some('s') => 1,
            Some('ч') | Some('h') => 3600,
            Some('д') | Some('d') => 86400,
            Some(_) => return None,
        };
        total = total.checked_add(amount.checked_mul(unit)?)?;
    }
    (total > 0).then_some(total)
}

/// Напомнить о чём-нибудь через указанное время
#[poise::command(slash_command, guild_only)]
pub async fn remind(
    ctx: Context<'_>,
    #[description = "Через сколько: 30м, 2ч, 1д, 1ч 30м (просто число — минуты)"]
    #[max_length = 30]
    when: String,
    #[description = "О чём напомнить"]
    #[max_length = 500]
    text: String,
) -> Result<(), Error> {
    let (data, guild, user) = (ctx.data(), ctx.guild_id().unwrap().get(), ctx.author().id.get());
    let Some(seconds) = parse_duration(&when).filter(|seconds| (30..=365 * 86400).contains(seconds)) else {
        return whisper(ctx, "Не понял время. Примеры: `30м`, `2ч`, `1д`, `1ч 30м` (от 30 секунд до года).").await;
    };
    let due = db::now() + seconds as f64;
    if !insert_reminder(&data.db, guild, ctx.channel_id().get(), user, due, &text)? {
        return whisper(ctx, format!("У тебя уже {MAX_REMINDERS} напоминаний — отмени лишние: /reminders.")).await;
    }
    whisper(ctx, format!("⏰ Напомню <t:{0}:R> (<t:{0}:f>): {text}", due as i64)).await
}

/// Твои напоминания (можно отменить одно по номеру)
#[poise::command(slash_command, guild_only)]
pub async fn reminders(
    ctx: Context<'_>,
    #[description = "Номер напоминания, которое нужно отменить"]
    #[min = 1]
    #[max = 10]
    cancel: Option<usize>,
) -> Result<(), Error> {
    let (data, guild, user) = (ctx.data(), ctx.guild_id().unwrap().get(), ctx.author().id.get());
    let rows = data.db.user_reminders(guild, user)?;
    if rows.is_empty() {
        return whisper(ctx, "Напоминаний нет. Поставить — /remind.").await;
    }
    if let Some(number) = cancel {
        let Some((id, _, text)) = rows.get(number - 1) else {
            return whisper(ctx, "Напоминания с таким номером нет.").await;
        };
        let cancelled = {
            let _update = REMINDER_LOCKS[*id as usize % REMINDER_LOCKS.len()].lock().await;
            finish_reminder(&data.db, *id)?
        };
        if cancelled {
            return whisper(ctx, format!("🗑️ Отменил напоминание: {text}")).await;
        }
        return whisper(ctx, "Это напоминание уже отправлено или отменено.").await;
    }
    for page in reminder_pages(&rows) {
        whisper(ctx, page).await?;
    }
    Ok(())
}

fn reminder_pages(rows: &[(i64, f64, String)]) -> Vec<String> {
    let mut pages = Vec::new();
    let mut page = "⏰ Твои напоминания:".to_owned();
    for (index, (_, due, text)) in rows.iter().enumerate() {
        let text: String = text.chars().take(500).collect();
        let line = format!("\n`{}.` <t:{}:R> — {text}", index + 1, *due as i64);
        if page.encode_utf16().count() + line.encode_utf16().count() > 2000 {
            pages.push(std::mem::take(&mut page));
        }
        page.push_str(&line);
    }
    pages.push(page);
    pages
}

/// Keep the reminder until delivery is acknowledged, including across a restart.
pub async fn send_due(http: &serenity::Http, data: &Data) -> Result<(), Error> {
    for id in reminder_batch(&data.db, db::now())? {
        let _update = REMINDER_LOCKS[id as usize % REMINDER_LOCKS.len()].lock().await;
        let reminder = data.db.lock().query_row(
            "SELECT channel_id, user_id, text FROM reminders WHERE id = ?1",
            [id],
            |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64, row.get::<_, String>(2)?)),
        ).optional()?;
        let Some((channel, user, text)) = reminder else { continue };
        if channel == 0 || user == 0 {
            eprintln!("Напоминание {id}: некорректный канал или получатель");
            retry_delivery(&data.db.lock(), REMINDER_JOB, id, db::now())?;
            continue;
        }
        let text: String = text.chars().take(500).collect();
        let message = serenity::CreateMessage::new()
            .content(format!("⏰ <@{user}>, напоминаю: {text}"))
            .allowed_mentions(serenity::CreateAllowedMentions::new().users(vec![serenity::UserId::new(user)]));
        let message = delivery_message(message, format!("reminder-{id:016x}"));
        match serenity::ChannelId::new(channel).send_message(http, message).await {
            Ok(_) => { finish_reminder(&data.db, id)?; }
            Err(error) if destination_gone(&error) => {
                eprintln!("Напоминание {id}: канал удалён: {error}");
                finish_reminder(&data.db, id)?;
            }
            Err(error) => {
                eprintln!("Напоминание {id}: будет повтор: {error}");
                retry_delivery(&data.db.lock(), REMINDER_JOB, id, db::now())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reminder_retries_survive_a_failed_delivery_without_consuming_the_job() {
        let db = Database::open(std::path::Path::new(":memory:")).unwrap();
        for _ in 0..MAX_REMINDERS {
            assert!(insert_reminder(&db, 1, 2, 3, 5.0, "reminder").unwrap());
        }
        assert!(!insert_reminder(&db, 1, 2, 3, 5.0, "extra").unwrap());
        let id = reminder_batch(&db, 5.0).unwrap()[0];
        retry_delivery(&db.lock(), REMINDER_JOB, id, 5.0).unwrap();
        assert!(!reminder_batch(&db, 34.0).unwrap().contains(&id));
        assert!(reminder_batch(&db, 35.0).unwrap().contains(&id));
        assert!(finish_reminder(&db, id).unwrap());
        assert!(!finish_reminder(&db, id).unwrap());
        assert!(!reminder_batch(&db, 100.0).unwrap().contains(&id));
    }

    #[test]
    fn reminder_list_keeps_all_numbers_within_the_message_limit() {
        let rows: Vec<_> = (1..=10).map(|id| (id, 123.0, "🚀".repeat(500))).collect();
        let pages = reminder_pages(&rows);
        assert!(pages.iter().all(|page| page.encode_utf16().count() <= 2000));
        for id in 1..=10 {
            assert!(pages.iter().any(|page| page.contains(&format!("`{id}.`"))));
        }
    }

    #[test]
    fn durations() {
        let cases = [("30м", Some(1800)), ("2ч", Some(7200)), ("1д", Some(86400)), ("1ч 30м", Some(5400)), ("90", Some(5400)),
                     ("45s", Some(45)), ("2h15m", Some(8100)), ("10 мин", Some(600)), ("1 час 5 минут", Some(3900)),
                     ("завтра", None), ("5 лет", None), ("", None), ("0", None), ("м30", None), ("3 дня", Some(259200))];
        for (text, expected) in cases {
            assert_eq!(parse_duration(text), expected, "{text:?}");
        }
    }
}
