//! Утренние и вечерние сообщения (/daily_setup, /daily_off, /daily_test): пожелание и интересный факт.
//! Текст пишет ИИ, а без него — запасной факт. Раз в минуту бот проверяет, не пора ли отправить.

use crate::admin::{bot_permissions, report_error};
use crate::db::{self, DailySettings, Database};
use crate::reminders::{delivery_message, destination_gone, retry_delay};
use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::Mutex;
use crate::reply::whisper;
use crate::{ai, Context, Data, Error, Shared};
use poise::serenity_prelude as serenity;
use rand::seq::IndexedRandom;

const WINDOW_MINUTES: i64 = 30; // если бот был выключен в нужную минуту, сообщение ещё отправится в течение 30 минут
const DELIVERY_BATCH: usize = 16;
const DELIVERY_CONCURRENCY: usize = 4;
static SCHEDULE_LOCKS: [Mutex<()>; 64] = [const { Mutex::const_new(()) }; 64];

/// О чём рассказывать факты (тема выбирается случайно).
const TOPICS: [&str; 8] = ["математика", "физика", "инженерия", "электроника и радиотехника", "астрономия",
                           "информатика и программирование", "химия", "история изобретений"];

/// Запасные факты — если ИИ не настроен или недоступен.
const FALLBACK_FACTS: [&str; 14] = [
    "число π иррационально: его десятичная запись бесконечна и никогда не повторяется. Это доказал Иоганн Ламберт в XVIII веке.",
    "свет от Солнца летит до Земли около 8 минут 20 секунд — мы всегда видим Солнце немного «в прошлом».",
    "сумма углов треугольника, нарисованного на сфере, всегда больше 180°.",
    "первый транзистор создали в 1947 году в Bell Labs Джон Бардин, Уолтер Браттейн и Уильям Шокли.",
    "запись 0,999… с бесконечными девятками — это ровно 1, а не «чуть меньше единицы».",
    "звук в стали распространяется более чем в 15 раз быстрее, чем в воздухе.",
    "Эйлер в 1736 году решил задачу о семи мостах Кёнигсберга — с неё началась теория графов.",
    "спутники GPS учитывают эффекты теории относительности: без поправок ошибка накапливалась бы примерно на 10 км в сутки.",
    "в группе всего из 23 человек вероятность того, что у двоих совпадут дни рождения, больше 50%.",
    "в оптоволокне свет идёт примерно на треть медленнее, чем в вакууме.",
    "канал молнии нагревается примерно до 30 000 °C — это в несколько раз горячее поверхности Солнца.",
    "простых чисел бесконечно много — Евклид доказал это больше 2000 лет назад.",
    "вода имеет наибольшую плотность при температуре около 4 °C, поэтому зимой у дна водоёмов вода теплее, а замерзают они сверху.",
    "Томас Хейлс в 1999 году доказал, что шестиугольные соты — способ разбить плоскость на равные ячейки с наименьшей общей длиной границ.",
];

/// Часовые пояса для /daily_setup.
#[derive(poise::ChoiceParameter, Clone, Copy)]
pub enum Zone {
    #[name = "Израиль"]
    Israel,
    #[name = "Москва"]
    Moscow,
    #[name = "Киев"]
    Kyiv,
    #[name = "Минск"]
    Minsk,
    #[name = "Казахстан (Алматы)"]
    Almaty,
    #[name = "Ташкент"]
    Tashkent,
    #[name = "Центральная Европа (Берлин)"]
    Berlin,
    #[name = "Лондон"]
    London,
    #[name = "Нью-Йорк"]
    NewYork,
}

impl Zone {
    /// Код пояса.
    fn code(self) -> &'static str {
        match self {
            Zone::Israel => "Asia/Jerusalem",
            Zone::Moscow => "Europe/Moscow",
            Zone::Kyiv => "Europe/Kyiv",
            Zone::Minsk => "Europe/Minsk",
            Zone::Almaty => "Asia/Almaty",
            Zone::Tashkent => "Asia/Tashkent",
            Zone::Berlin => "Europe/Berlin",
            Zone::London => "Europe/London",
            Zone::NewYork => "America/New_York",
        }
    }
}

#[derive(poise::ChoiceParameter, Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    #[name = "☀️ Утреннее"]
    Morning,
    #[name = "🌙 Вечернее"]
    Evening,
}

/// Текущее время (без пояса) в часовом поясе сервера. Неизвестный пояс — UTC.
pub fn local_now(zone: &str) -> chrono::NaiveDateTime {
    match zone.parse::<chrono_tz::Tz>() {
        Ok(zone) => chrono::Utc::now().with_timezone(&zone).naive_local(),
        Err(_) => chrono::Utc::now().naive_utc(),
    }
}

/// «8:00», «08.00» → «08:00». None — не время.
pub fn parse_time(text: &str) -> Option<String> {
    let (hours, minutes) = text.trim().split_once([':', '.'])?;
    if hours.is_empty() || hours.len() > 2 || minutes.len() != 2 || !(hours.chars().chain(minutes.chars())).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (hours, minutes): (u32, u32) = (hours.parse().ok()?, minutes.parse().ok()?);
    (hours <= 23 && minutes <= 59).then(|| format!("{hours:02}:{minutes:02}"))
}

/// Наступило ли уже время at («08:00») в момент now.
fn passed(now: chrono::NaiveDateTime, at: &str) -> bool {
    scheduled(now, at).is_some_and(|scheduled| now >= scheduled)
}

fn scheduled(now: chrono::NaiveDateTime, at: &str) -> Option<chrono::NaiveDateTime> {
    let (hours, minutes) = at.split_once(':')?;
    now.date().and_hms_opt(hours.parse().ok()?, minutes.parse().ok()?, 0)
}

/// Find the dated occurrence before midnight as well as today's. For a DST
/// overlap use the first occurrence; a skipped wall-clock time has no slot.
fn due_day(now: DateTime<Utc>, zone: chrono_tz::Tz, at: &str, last: &str) -> Option<NaiveDate> {
    let time = NaiveTime::parse_from_str(at, "%H:%M").ok()?;
    let today = now.with_timezone(&zone).date_naive();
    [Some(today), today.pred_opt()].into_iter().flatten().find(|day| {
        let Some(scheduled) = zone.from_local_datetime(&day.and_time(time)).earliest() else {
            return false;
        };
        day.to_string().as_str() > last
            && (0..WINDOW_MINUTES * 60).contains(&(now - scheduled.with_timezone(&Utc)).num_seconds())
    })
}

#[cfg(test)]
fn is_due(now: chrono::NaiveDateTime, at: &str, last: &str) -> bool {
    due_day(now.and_utc(), chrono_tz::UTC, at, last).is_some()
}

fn schema(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS daily_schedule_revision (
             guild_id INTEGER PRIMARY KEY, revision INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS daily_delivery (
             guild_id INTEGER NOT NULL, kind INTEGER NOT NULL, revision INTEGER NOT NULL,
             day TEXT NOT NULL, channel_id INTEGER NOT NULL, nonce TEXT NOT NULL,
             text TEXT, failures INTEGER NOT NULL DEFAULT 0, retry_at REAL NOT NULL,
             PRIMARY KEY (guild_id, kind)
         );
         CREATE INDEX IF NOT EXISTS daily_delivery_due ON daily_delivery (retry_at);",
    )
}

struct Delivery {
    guild: u64,
    kind: Kind,
    revision: i64,
    day: String,
    channel: u64,
    nonce: String,
    text: Option<String>,
    failures: i64,
}

impl Kind {
    fn code(self) -> i64 {
        i64::from(self == Self::Evening)
    }

    fn last_column(self) -> &'static str {
        match self {
            Self::Morning => "last_morning",
            Self::Evening => "last_evening",
        }
    }
}

fn schedule(db: &Database, now: DateTime<Utc>) -> rusqlite::Result<()> {
    let mut connection = db.lock();
    schema(&connection)?;
    let transaction = connection.transaction()?;
    let mut rows = transaction.prepare(
        "SELECT s.guild_id, s.channel_id, s.timezone, s.morning, s.evening,
                s.last_morning, s.last_evening, COALESCE(r.revision, 0)
         FROM daily_settings s LEFT JOIN daily_schedule_revision r USING (guild_id)
         WHERE s.guild_id > 0 AND s.channel_id > 0",
    )?;
    let mut jobs = rows.query([])?;
    while let Some(row) = jobs.next()? {
        let guild = row.get::<_, i64>(0)? as u64;
        let channel = row.get::<_, i64>(1)? as u64;
        let zone = row.get::<_, Option<String>>(2)?.as_deref()
            .and_then(|zone| zone.parse().ok()).unwrap_or(chrono_tz::UTC);
        let revision: i64 = row.get(7)?;
        for (kind, at, last) in [(Kind::Morning, 3, 5), (Kind::Evening, 4, 6)] {
            let at = row.get::<_, Option<String>>(at)?.unwrap_or_default();
            let last = row.get::<_, Option<String>>(last)?.unwrap_or_default();
            if let Some(day) = due_day(now, zone, &at, &last) {
                transaction.execute(
                    "INSERT INTO daily_delivery (guild_id, kind, revision, day, channel_id, nonce, retry_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(guild_id, kind) DO UPDATE SET revision=excluded.revision,
                         day=excluded.day, channel_id=excluded.channel_id, nonce=excluded.nonce,
                         text=NULL, failures=0, retry_at=excluded.retry_at
                     WHERE daily_delivery.revision != excluded.revision OR daily_delivery.day < excluded.day",
                    params![guild as i64, kind.code(), revision, day.to_string(), channel as i64,
                            format!("daily-{:016x}", rand::random::<u64>()), now.timestamp() as f64],
                )?;
            }
        }
    }
    drop(jobs);
    drop(rows);
    transaction.commit()
}

fn pending(db: &Database, now: f64) -> rusqlite::Result<Vec<Delivery>> {
    let connection = db.lock();
    let mut statement = connection.prepare(
        "SELECT guild_id, kind, revision, day, channel_id, nonce, text, failures
         FROM daily_delivery WHERE retry_at <= ?1 ORDER BY retry_at, guild_id, kind LIMIT ?2",
    )?;
    let rows = statement.query_map(params![now, DELIVERY_BATCH as i64], |row| {
        Ok(Delivery {
            guild: row.get::<_, i64>(0)? as u64,
            kind: if row.get::<_, i64>(1)? == 0 { Kind::Morning } else { Kind::Evening },
            revision: row.get(2)?,
            day: row.get(3)?,
            channel: row.get::<_, i64>(4)? as u64,
            nonce: row.get(5)?,
            text: row.get(6)?,
            failures: row.get(7)?,
        })
    })?;
    rows.collect()
}

fn save_delivery_text(db: &Database, job: &Delivery, text: &str) -> rusqlite::Result<bool> {
    let updated = db.lock().execute(
        "UPDATE daily_delivery SET text = ?1 WHERE guild_id = ?2 AND kind = ?3 AND nonce = ?4",
        params![text, job.guild as i64, job.kind.code(), job.nonce],
    )?;
    Ok(updated == 1)
}

fn finish_delivery(db: &Database, job: &Delivery) -> rusqlite::Result<()> {
    let mut connection = db.lock();
    let transaction = connection.transaction()?;
    let removed = transaction.execute(
        "DELETE FROM daily_delivery WHERE guild_id = ?1 AND kind = ?2 AND nonce = ?3",
        params![job.guild as i64, job.kind.code(), job.nonce],
    )?;
    if removed == 1 {
        let last = job.kind.last_column();
        transaction.execute(
            &format!("UPDATE daily_settings SET {last} = ?1 WHERE guild_id = ?2
             AND COALESCE((SELECT revision FROM daily_schedule_revision WHERE guild_id = ?2), 0) = ?3
             AND COALESCE({last}, '') < ?1"),
            params![job.day, job.guild as i64, job.revision],
        )?;
    }
    transaction.commit()
}

fn change_schedule(db: &Database, guild: u64, settings: Option<&DailySettings>) -> rusqlite::Result<bool> {
    let mut connection = db.lock();
    schema(&connection)?;
    let transaction = connection.transaction()?;
    let changed = match settings {
        Some(row) => transaction.execute(
            "INSERT INTO daily_settings (guild_id, channel_id, timezone, morning, evening, last_morning, last_evening)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (guild_id) DO UPDATE SET channel_id = excluded.channel_id,
                 timezone = excluded.timezone, morning = excluded.morning, evening = excluded.evening,
                 last_morning = excluded.last_morning, last_evening = excluded.last_evening",
            params![guild as i64, row.channel as i64, row.timezone, row.morning, row.evening, row.last_morning, row.last_evening],
        )?,
        None => transaction.execute("DELETE FROM daily_settings WHERE guild_id = ?1", [guild as i64])?,
    };
    transaction.execute(
        "INSERT INTO daily_schedule_revision (guild_id, revision) VALUES (?1, 1)
         ON CONFLICT(guild_id) DO UPDATE SET revision = revision + 1",
        [guild as i64],
    )?;
    transaction.execute("DELETE FROM daily_delivery WHERE guild_id = ?1", [guild as i64])?;
    transaction.commit()?;
    Ok(changed == 1)
}

/// Текст утреннего или вечернего сообщения. Пишет ИИ, а без него — запасной вариант.
async fn make_text(
    http: &serenity::Http, data: &Data, kind: Kind, guild: Option<serenity::GuildId>, today: NaiveDate,
) -> String {
    let (topic, fact) = {
        let mut random = rand::rng();
        (*TOPICS.choose(&mut random).unwrap(), *FALLBACK_FACTS.choose(&mut random).unwrap())
    };
    let task = match kind {
        Kind::Morning => format!(
            "Напиши утреннее сообщение для Discord-чата друзей. Пожелай всем доброго утра и удачи на сегодня, \
             затем расскажи один интересный факт из области «{topic}». Факт должен быть широко известным \
             и достоверным — без выдуманных чисел, дат и имён. Всего 3–6 предложений."),
        Kind::Evening => format!(
            "Напиши вечернее сообщение для Discord-чата друзей. Расскажи интересный «факт дня» из области «{topic}» \
             (если точно знаешь событие из этой области, связанное с датой {}, — можно о нём, \
             иначе просто интересный факт). Факт должен быть достоверным — без выдуманных чисел, дат и имён. \
             Потом пожелай всем удачного завтрашнего дня и спокойной ночи. Всего 3–6 предложений.", today.format("%d.%m")),
    };
    if data.ai.ready() {
        let system = format!("{}\nСегодня {}.", ai::system_prompt(data), today.format("%d.%m.%Y"));
        match ai::generate(data, &system, &[ai::Turn::user(task)]).await {
            Ok(answer) => {
                let text = ai::without_tags(&answer.text);
                if !text.is_empty() {
                    return text;
                }
            }
            Err(error) => report_error(http, data, guild, "ИИ (ежедневное сообщение)", &error.to_string()).await,
        }
    }
    match kind {
        Kind::Morning => format!("Доброе утро! Удачи всем сегодня — пусть всё получится 💪\n\n🧠 **Интересный факт:** {fact}"),
        Kind::Evening => format!("🧠 **Факт дня:** {fact}\n\nПусть завтрашний день будет удачным. Спокойной ночи! 🌙"),
    }
}

fn message(kind: Kind, text: String) -> serenity::CreateMessage {
    let description: String = text.chars().take(2000).collect();
    let (title, color) = match kind {
        Kind::Morning => ("☀️ Доброе утро!", 0xF59E0B),
        Kind::Evening => ("🌙 Спокойной ночи!", 0x6366F1),
    };
    serenity::CreateMessage::new()
        .embed(serenity::CreateEmbed::new().title(title).description(description).color(color))
        .allowed_mentions(serenity::CreateAllowedMentions::new())
}

async fn send_scheduled(http: &serenity::Http, data: &Data, job: Delivery) -> Result<(), Error> {
    let guild = Some(serenity::GuildId::new(job.guild));
    let text = match &job.text {
        Some(text) => text.clone(),
        None => make_text(http, data, job.kind, guild, job.day.parse()?).await,
    };
    // Settings can change while Gemini is working. Serialize only the final
    // send against settings commands and test that this delivery still exists.
    let _update = SCHEDULE_LOCKS[job.guild as usize % SCHEDULE_LOCKS.len()].lock().await;
    if !save_delivery_text(&data.db, &job, &text)? {
        return Ok(());
    }
    let message = delivery_message(message(job.kind, text), job.nonce.clone());
    match serenity::ChannelId::new(job.channel).send_message(http, message).await {
        Ok(_) => finish_delivery(&data.db, &job)?,
        Err(error) if destination_gone(&error) => {
            eprintln!("Ежедневное сообщение для сервера {}: канал удалён: {error}", job.guild);
            finish_delivery(&data.db, &job)?;
        }
        Err(error) => {
            let failures = job.failures.saturating_add(1).min(8);
            data.db.lock().execute(
                "UPDATE daily_delivery SET failures = ?1, retry_at = ?2
                 WHERE guild_id = ?3 AND kind = ?4 AND nonce = ?5",
                params![failures, db::now() + retry_delay(failures), job.guild as i64, job.kind.code(), job.nonce],
            )?;
            eprintln!("Ежедневное сообщение для сервера {}: будет повтор: {error}", job.guild);
        }
    }
    Ok(())
}

/// Schedule catch-up slots independently of slow generation or Discord delivery.
pub fn start_loop(http: std::sync::Arc<serenity::Http>, data: Shared) {
    let state = data.clone();
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(60));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            if let Err(error) = schedule(&state.db, Utc::now()) {
                eprintln!("Ежедневные сообщения: {error}");
            }
        }
    });
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(10));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            let jobs = match pending(&data.db, db::now()) {
                Ok(jobs) => jobs,
                Err(error) => {
                    eprintln!("Ежедневные сообщения: {error}");
                    continue;
                }
            };
            let mut workers = tokio::task::JoinSet::new();
            for job in jobs {
                let http = http.clone();
                let data = data.clone();
                workers.spawn(async move { send_scheduled(&http, &data, job).await });
                if workers.len() == DELIVERY_CONCURRENCY {
                    log_worker_result(workers.join_next().await);
                }
            }
            while let Some(result) = workers.join_next().await {
                log_worker_result(Some(result));
            }
        }
    });
}

fn log_worker_result(result: Option<Result<Result<(), Error>, tokio::task::JoinError>>) {
    match result {
        Some(Ok(Err(error))) => eprintln!("Ежедневные сообщения: {error}"),
        Some(Err(error)) => eprintln!("Ежедневные сообщения: {error}"),
        _ => (),
    }
}

/// Включить утренние и вечерние сообщения
#[poise::command(slash_command, guild_only, required_permissions = "MANAGE_GUILD", default_member_permissions = "MANAGE_GUILD")]
pub async fn daily_setup(
    ctx: Context<'_>,
    #[description = "В какой канал писать"]
    #[channel_types("Text")]
    channel: serenity::GuildChannel,
    #[description = "Часовой пояс сервера"] timezone: Zone,
    #[description = "Время утреннего сообщения, например 08:00"] morning: Option<String>,
    #[description = "Время вечернего сообщения, например 22:00"] evening: Option<String>,
) -> Result<(), Error> {
    let mut times = Vec::new();
    for value in [morning.unwrap_or_else(|| "08:00".into()), evening.unwrap_or_else(|| "22:00".into())] {
        match parse_time(&value) {
            Some(time) => times.push(time),
            None => return whisper(ctx, format!("Не понял время «{value}». Пиши так: 08:00")).await,
        }
    }
    let allowed = bot_permissions(ctx, &channel).is_some_and(|p| p.send_messages() && p.embed_links());
    if !allowed {
        return whisper(ctx, format!("Мне нельзя писать в <#{}>: нужны права «Отправлять сообщения» и «Встраивать ссылки».", channel.id)).await;
    }
    // Если сегодня это время уже прошло — первое сообщение будет завтра (отмечаем как будто уже отправили)
    let code = timezone.code();
    let now = local_now(code);
    let today = now.date();
    let yesterday = today.pred_opt().unwrap_or(today);
    let mark = |at: &str| if passed(now, at) { today } else { yesterday }.to_string();
    let settings = DailySettings {
        guild: ctx.guild_id().unwrap().get(),
        channel: channel.id.get(),
        timezone: code.to_string(),
        last_morning: mark(&times[0]),
        last_evening: mark(&times[1]),
        morning: times[0].clone(),
        evening: times[1].clone(),
    };
    {
        let _update = SCHEDULE_LOCKS[settings.guild as usize % SCHEDULE_LOCKS.len()].lock().await;
        change_schedule(&ctx.data().db, settings.guild, Some(&settings))?;
    }
    ctx.say(format!(
        "✅ Готово! Буду писать в <#{}>: утром в **{}** и вечером в **{}** ({}).\nПроверить прямо сейчас: /daily_test",
        channel.id, settings.morning, settings.evening, poise::ChoiceParameter::name(&timezone))).await?;
    Ok(())
}

/// Выключить утренние и вечерние сообщения
#[poise::command(slash_command, guild_only, required_permissions = "MANAGE_GUILD", default_member_permissions = "MANAGE_GUILD")]
pub async fn daily_off(ctx: Context<'_>) -> Result<(), Error> {
    let guild = ctx.guild_id().unwrap().get();
    let changed = {
        let _update = SCHEDULE_LOCKS[guild as usize % SCHEDULE_LOCKS.len()].lock().await;
        change_schedule(&ctx.data().db, guild, None)?
    };
    if changed {
        ctx.say("🔕 Утренние и вечерние сообщения выключены.").await?;
        Ok(())
    } else {
        whisper(ctx, "Они и так выключены.").await
    }
}

/// Прислать утреннее или вечернее сообщение прямо сейчас
#[poise::command(slash_command, guild_only, required_permissions = "MANAGE_GUILD", default_member_permissions = "MANAGE_GUILD")]
pub async fn daily_test(ctx: Context<'_>, #[description = "Какое сообщение прислать"] kind: Kind) -> Result<(), Error> {
    let data = ctx.data();
    let guild = ctx.guild_id().unwrap();
    let configured = data.db.lock().query_row(
        "SELECT channel_id, timezone FROM daily_settings WHERE guild_id = ?1",
        [guild.get() as i64],
        |row| Ok((row.get::<_, Option<i64>>(0)?.map(|id| id as u64), row.get::<_, Option<String>>(1)?)),
    ).optional()?;
    let (channel, timezone) = configured.unwrap_or_default();
    let channel = channel.filter(|channel| *channel > 0).map_or(ctx.channel_id(), serenity::ChannelId::new);
    ctx.defer_ephemeral().await?;
    let generated = make_text(ctx.http(), data, kind, Some(guild), local_now(timezone.as_deref().unwrap_or("UTC")).date()).await;
    let text = match channel.send_message(ctx.http(), message(kind, generated)).await {
        Ok(_) => "✅ Отправила.",
        Err(error) => {
            report_error(ctx.http(), data, Some(guild), "Отправка ежедневного сообщения", &error.to_string()).await;
            "Не получилось отправить — подробности в канале логов."
        }
    };
    whisper(ctx, text).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(hours: u32, minutes: u32) -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, 12).unwrap().and_hms_opt(hours, minutes, 0).unwrap()
    }

    #[test]
    fn times() {
        assert_eq!(parse_time("8:00").as_deref(), Some("08:00"));
        assert_eq!(parse_time(" 22.30 ").as_deref(), Some("22:30"));
        assert_eq!([parse_time("24:00"), parse_time("8:60"), parse_time("8"), parse_time("8:0"), parse_time("ab:cd"), parse_time("123:00")],
                   [None, None, None, None, None, None]);
        assert!(passed(at(8, 0), "08:00") && !passed(at(7, 59), "08:00"));
    }

    #[test]
    fn schedule() {
        assert!(is_due(at(8, 0), "08:00", ""));
        assert!(is_due(at(8, 29), "08:00", "2026-11-11")); // бот был выключен — ещё успеваем
        assert!(!is_due(at(8, 30), "08:00", "")); // окно прошло
        assert!(!is_due(at(7, 59), "08:00", ""));
        assert!(!is_due(at(8, 5), "08:00", "2026-11-12")); // сегодня уже отправили
        assert!(!is_due(at(8, 5), "чушь", ""));
    }


    #[test]
    fn catch_up_before_midnight_slot_expires() {
        assert!(is_due(at(0, 5), "23:50", "2026-11-10"));
        assert!(!is_due(at(0, 5), "23:50", "2026-11-11"));
        assert!(!is_due(at(0, 20), "23:50", ""));
    }

    #[test]
    fn persisted_job_cannot_mark_replaced_settings_sent() {
        let db = Database::open(std::path::Path::new(":memory:")).unwrap();
        let mut settings = DailySettings {
            guild: 1, channel: 2, timezone: "UTC".into(), morning: "08:00".into(),
            evening: "23:50".into(), last_morning: String::new(), last_evening: String::new(),
        };
        change_schedule(&db, 1, Some(&settings)).unwrap();
        let current = at(0, 5).and_utc();
        super::schedule(&db, current).unwrap();
        let old = pending(&db, current.timestamp() as f64).unwrap().pop().unwrap();
        assert_eq!(old.day, "2026-11-11");
        assert!(save_delivery_text(&db, &old, "text ready").unwrap());
        assert_eq!(pending(&db, current.timestamp() as f64).unwrap()[0].text.as_deref(), Some("text ready"));
        settings.channel = 3;
        change_schedule(&db, 1, Some(&settings)).unwrap();
        assert!(!save_delivery_text(&db, &old, "old text").unwrap());
        super::schedule(&db, current).unwrap();
        finish_delivery(&db, &old).unwrap();
        let replacement = pending(&db, current.timestamp() as f64).unwrap().pop().unwrap();
        assert_ne!(replacement.revision, old.revision);
        assert_eq!(replacement.channel, 3);
        finish_delivery(&db, &replacement).unwrap();
        super::schedule(&db, current).unwrap();
        assert!(pending(&db, current.timestamp() as f64).unwrap().is_empty());
    }

    #[test]
    fn daylight_saving_uses_first_occurrence_and_ignores_nonexistent_slot() {
        let berlin = chrono_tz::Europe::Berlin;
        let date = |value: &str| value.parse::<DateTime<Utc>>().unwrap();
        assert!(due_day(date("2026-03-29T01:10:00Z"), berlin, "02:00", "").is_none());
        assert!(due_day(date("2026-10-25T00:10:00Z"), berlin, "02:00", "").is_some());
        assert!(due_day(date("2026-10-25T01:10:00Z"), berlin, "02:00", "").is_none());
    }

    #[test]
    fn zones_known() {
        for zone in [Zone::Israel, Zone::Moscow, Zone::Kyiv, Zone::Minsk, Zone::Almaty, Zone::Tashkent, Zone::Berlin, Zone::London, Zone::NewYork] {
            assert!(zone.code().parse::<chrono_tz::Tz>().is_ok(), "{}", zone.code());
        }
    }
}
