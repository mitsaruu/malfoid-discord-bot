//! Опросы (/poll): вопрос и до 5 вариантов. Голосуют кнопками, голос можно поменять.
//! Когда время выйдет, бот подводит итог. Кнопки работают и после перезапуска.

use crate::reply::whisper;
use crate::reminders::{clear_delivery_retry, delivery_schema, destination_gone, retry_delivery};
use crate::{db::{self, Database}, Context, Data, Error};
use poise::serenity_prelude as serenity;
use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::Mutex;

const MAX_OPTIONS: usize = 5;
const CLOSE_BATCH: usize = 32;
const POLL_JOB: &str = "poll";
static MESSAGE_LOCKS: [Mutex<()>; 64] = [const { Mutex::const_new(()) }; 64];

/// Варианты из строки «Пицца | Суши | Бургеры». None — их меньше 2, больше 5 или есть одинаковые.
pub fn parse_options(text: &str) -> Option<Vec<String>> {
    let options: Vec<String> = text
        .split(['|', ';', '\n'])
        .map(|option| option.trim().chars().take(80).collect::<String>())
        .filter(|option| !option.is_empty())
        .collect();
    let unique = options.iter().collect::<std::collections::HashSet<_>>().len();
    ((2..=MAX_OPTIONS).contains(&options.len()) && unique == options.len()).then_some(options)
}

/// Текст карточки опроса: варианты с полосками. votes — сколько голосов у каждого варианта.
pub fn poll_lines(options: &[String], votes: &[i64], closed: bool) -> String {
    let total: i64 = votes.iter().sum();
    let best = votes.iter().copied().max().unwrap_or(0);
    options
        .iter()
        .zip(votes)
        .map(|(option, count)| {
            let share = if total > 0 { *count as f64 / total as f64 } else { 0.0 };
            let mark = if closed && *count == best && *count > 0 { " 🏆" } else { "" };
            let bar = "█".repeat((share * 12.0).round_ties_even() as usize);
            format!("**{option}**{mark}\n`{bar:<12}` {}% · {count}", (share * 100.0).round_ties_even())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn embed(question: &str, options: &[String], votes: &[i64], ends: f64, author: u64, closed: bool) -> serenity::CreateEmbed {
    let total: i64 = votes.iter().sum();
    let status = if closed { "завершён".to_string() } else { format!("закроется <t:{}:R>", ends as i64) };
    serenity::CreateEmbed::new()
        .color(0x7C6CF2)
        .title(format!("{}{question}", if closed { "📊 Итоги: " } else { "📊 " }))
        .description(poll_lines(options, votes, closed))
        .field("\u{200b}", format!("Проголосовало: {total} · опрос от <@{author}> · {status}"), false)
}

fn buttons(options: &[String]) -> Vec<serenity::CreateActionRow> {
    let buttons = options
        .iter()
        .enumerate()
        .map(|(index, option)| serenity::CreateButton::new(format!("poll:{index}")).label(option).style(serenity::ButtonStyle::Secondary))
        .collect();
    vec![serenity::CreateActionRow::Buttons(buttons)]
}

struct Poll {
    channel: u64,
    author: u64,
    question: String,
    options: Vec<String>,
    ends: f64,
}

impl Poll {
    fn load(connection: &Connection, message: u64) -> rusqlite::Result<Option<Self>> {
        connection.query_row(
            "SELECT channel_id, author_id, question, options, ends FROM polls WHERE message_id = ?1",
            [message as i64],
            |row| {
                let options: String = row.get(3)?;
                Ok(Self {
                    channel: row.get::<_, i64>(0)? as u64,
                    author: row.get::<_, i64>(1)? as u64,
                    question: row.get(2)?,
                    options: serde_json::from_str(&options).unwrap_or_default(),
                    ends: row.get(4)?,
                })
            },
        ).optional()
    }

    fn tally(&self, connection: &Connection, message: u64) -> rusqlite::Result<Vec<i64>> {
        let mut votes = vec![0; self.options.len()];
        let mut statement = connection.prepare(
            "SELECT option, COUNT(*) FROM poll_votes WHERE message_id = ?1 GROUP BY option",
        )?;
        let rows = statement.query_map([message as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (option, count) = row?;
            if let Some(slot) = votes.get_mut(option as usize) {
                *slot = count;
            }
        }
        Ok(votes)
    }

    fn card(&self, votes: &[i64], closed: bool) -> serenity::CreateEmbed {
        embed(&self.question, &self.options, votes, self.ends, self.author, closed)
    }
}

fn accept_vote(
    db: &Database, message: u64, user: u64, option: i64, now: f64,
) -> rusqlite::Result<Option<(Poll, Vec<i64>)>> {
    let mut connection = db.lock();
    let transaction = connection.transaction()?;
    let Some(poll) = Poll::load(&transaction, message)?
        .filter(|poll| poll.ends > now && (0..poll.options.len() as i64).contains(&option))
    else {
        return Ok(None);
    };
    transaction.execute(
        "INSERT INTO poll_votes (message_id, user_id, option) VALUES (?1, ?2, ?3)
         ON CONFLICT (message_id, user_id) DO UPDATE SET option = excluded.option",
        params![message as i64, user as i64, option],
    )?;
    let votes = poll.tally(&transaction, message)?;
    transaction.commit()?;
    Ok(Some((poll, votes)))
}

fn finish_poll(db: &Database, message: u64) -> rusqlite::Result<()> {
    let mut connection = db.lock();
    let transaction = connection.transaction()?;
    transaction.execute("DELETE FROM poll_votes WHERE message_id = ?1", [message as i64])?;
    transaction.execute("DELETE FROM polls WHERE message_id = ?1", [message as i64])?;
    clear_delivery_retry(&transaction, POLL_JOB, message as i64)?;
    transaction.commit()
}

fn close_batch(db: &Database, now: f64) -> rusqlite::Result<Vec<u64>> {
    let connection = db.lock();
    delivery_schema(&connection)?;
    connection.execute_batch("CREATE INDEX IF NOT EXISTS polls_ends ON polls (ends);")?;
    let mut statement = connection.prepare(
        "SELECT p.message_id FROM polls p
         LEFT JOIN delivery_retry d ON d.job = 'poll' AND d.id = p.message_id
         WHERE p.ends <= ?1 AND COALESCE(d.retry_at, 0) <= ?1 ORDER BY p.ends, p.message_id LIMIT ?2",
    )?;
    let rows = statement.query_map(params![now, CLOSE_BATCH as i64], |row| row.get::<_, i64>(0).map(|id| id as u64))?;
    rows.collect()
}

/// Опрос с кнопками: вопрос и до 5 вариантов ответа
#[poise::command(slash_command, guild_only)]
pub async fn poll(
    ctx: Context<'_>,
    #[description = "Вопрос"]
    #[max_length = 200]
    question: String,
    #[description = "Варианты через | — например: Пицца | Суши | Бургеры"]
    #[max_length = 500]
    options: String,
    #[description = "Сколько минут идёт опрос (по умолчанию 60)"]
    #[min = 1]
    #[max = 10080]
    minutes: Option<i64>,
) -> Result<(), Error> {
    let Some(choices) = parse_options(&options) else {
        return whisper(ctx, format!(
            "Нужно от 2 до {MAX_OPTIONS} разных вариантов через `|`. Пример: `Пицца | Суши | Бургеры`")).await;
    };
    let (ends, author) = (db::now() + minutes.unwrap_or(60) as f64 * 60.0, ctx.author().id.get());
    let reply = ctx
        .send(poise::CreateReply::default()
            .embed(embed(&question, &choices, &vec![0; choices.len()], ends, author, false))
            .allowed_mentions(serenity::CreateAllowedMentions::new()))
        .await?;
    let message = reply.message().await?.id.get();
    let _update = MESSAGE_LOCKS[message as usize % MESSAGE_LOCKS.len()].lock().await;
    let channel = ctx.channel_id();
    if let Err(error) = ctx.data().db.add_poll(message, channel.get(), author, &question, &serde_json::to_string(&choices)?, ends) {
        let _ = channel.delete_message(ctx.http(), serenity::MessageId::new(message)).await;
        return Err(error.into());
    }
    channel.edit_message(ctx.http(), serenity::MessageId::new(message),
        serenity::EditMessage::new().components(buttons(&choices))).await?;
    Ok(())
}

/// Нажатие кнопки варианта. Возвращает true, если это была кнопка опроса.
pub async fn vote_pressed(ctx: &serenity::Context, data: &Data, press: &serenity::ComponentInteraction) -> bool {
    let Some(option) = press.data.custom_id.strip_prefix("poll:").and_then(|index| index.parse::<i64>().ok()) else {
        return false;
    };
    if let Err(error) = vote(ctx, data, press, option).await {
        println!("Опрос: {}", crate::interactions::sanitize_error(&error.to_string()));
    }
    true
}

async fn vote(ctx: &serenity::Context, data: &Data, press: &serenity::ComponentInteraction, option: i64) -> Result<(), Error> {
    press.create_response(ctx, serenity::CreateInteractionResponse::Acknowledge).await?;
    let message = press.message.id.get();
    let _update = MESSAGE_LOCKS[message as usize % MESSAGE_LOCKS.len()].lock().await;
    let Some((poll, votes)) = accept_vote(&data.db, message, press.user.id.get(), option, db::now())? else {
        let note = serenity::CreateInteractionResponseFollowup::new().content("Этот опрос уже закончился или вариант недоступен.").ephemeral(true);
        press.create_followup(ctx, note).await?;
        return Ok(());
    };
    // Serialize edits with closure so an earlier vote cannot restore an open card.
    press.edit_response(ctx, serenity::EditInteractionResponse::new().embed(poll.card(&votes, false))).await?;
    Ok(())
}

/// Публикует итог. При временном отказе закрытый опрос и голоса остаются в БД.
pub async fn close_finished(http: &serenity::Http, data: &Data) -> Result<(), Error> {
    for message in close_batch(&data.db, db::now())? {
        let _update = MESSAGE_LOCKS[message as usize % MESSAGE_LOCKS.len()].lock().await;
        let result = {
            let connection = data.db.lock();
            Poll::load(&connection, message)?
                .filter(|poll| poll.ends <= db::now())
                .map(|poll| poll.tally(&connection, message).map(|votes| (poll, votes)))
                .transpose()?
        };
        let Some((poll, votes)) = result else { continue };
        let edit = serenity::EditMessage::new().embed(poll.card(&votes, true)).components(Vec::new());
        match serenity::ChannelId::new(poll.channel).edit_message(http, serenity::MessageId::new(message), edit).await {
            Ok(_) => finish_poll(&data.db, message)?,
            Err(error) if destination_gone(&error) => {
                eprintln!("Опрос {message}: удалён канал или сообщение: {error}");
                finish_poll(&data.db, message)?;
            }
            Err(error) => {
                eprintln!("Опрос {message}: не удалось опубликовать итог, будет повтор: {error}");
                retry_delivery(&data.db.lock(), POLL_JOB, message as i64, db::now())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closure_preserves_votes_for_retry_and_rejects_late_votes() {
        let db = Database::open(std::path::Path::new(":memory:")).unwrap();
        db.add_poll(42, 3, 2, "Question", r#"["Yes","No"]"#, 5.0).unwrap();
        let (_, votes) = accept_vote(&db, 42, 9, 1, 4.0).unwrap().unwrap();
        assert_eq!(votes, [0, 1]);
        assert!(accept_vote(&db, 42, 9, 0, 5.0).unwrap().is_none());
        assert!(accept_vote(&db, 42, 10, -1, 4.0).unwrap().is_none());
        assert_eq!(close_batch(&db, 5.0).unwrap(), [42]);
        retry_delivery(&db.lock(), POLL_JOB, 42, 5.0).unwrap();
        assert!(close_batch(&db, 34.0).unwrap().is_empty());
        assert_eq!(close_batch(&db, 35.0).unwrap(), [42]);
        let votes = db.poll_votes(42).unwrap();
        assert_eq!(votes.get(&1), Some(&1));
        finish_poll(&db, 42).unwrap();
        assert!(close_batch(&db, 40.0).unwrap().is_empty());
        assert!(db.poll_votes(42).unwrap().is_empty());
    }

    #[test]
    fn options() {
        assert_eq!(parse_options("Пицца | Суши|Бургеры ").unwrap(), ["Пицца", "Суши", "Бургеры"]);
        assert_eq!(parse_options("да;нет").unwrap(), ["да", "нет"]);
        assert_eq!(parse_options("один"), None);
        assert_eq!(parse_options("a|a"), None);
        assert_eq!(parse_options("1|2|3|4|5|6"), None);
        assert_eq!(parse_options("a| |b").unwrap().len(), 2);
    }

    #[test]
    fn lines() {
        let options: Vec<String> = ["Пицца", "Суши", "Бургеры"].iter().map(|option| option.to_string()).collect();
        assert_eq!(poll_lines(&options, &[0, 0, 0], false),
                   "**Пицца**\n`            ` 0% · 0\n**Суши**\n`            ` 0% · 0\n**Бургеры**\n`            ` 0% · 0");
        assert_eq!(poll_lines(&options, &[3, 1, 0], true),
                   "**Пицца** 🏆\n`█████████   ` 75% · 3\n**Суши**\n`███         ` 25% · 1\n**Бургеры**\n`            ` 0% · 0");
        assert!(!poll_lines(&options, &[3, 1, 0], false).contains("🏆")); // пока опрос идёт, победителя не отмечаем
    }
}
