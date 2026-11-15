//! Метаданные заявок, подбор Spotify, редактирование очереди и автовыход.
use crate::{db, Context, Data, Error, Shared};
use poise::serenity_prelude as s;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Item {
    pub id: i64,
    pub url: String,
    pub title: String,
    pub details: Value,
}
pub fn items(data: &Data, guild: u64) -> Result<Vec<Item>, Error> {
    let conn = data.db.lock();
    let mut st=conn.prepare("SELECT id,url,title,details FROM queue WHERE guild_id=?1 ORDER BY COALESCE(sort_order,id),id")?;
    let rows = st.query_map([guild as i64], |r| {
        Ok(Item {
            id: r.get(0)?,
            url: r.get(1)?,
            title: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            details: r
                .get::<_, Option<String>>(3)?
                .and_then(|v| serde_json::from_str(&v).ok())
                .unwrap_or(Value::Null),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
pub fn enqueue(
    data: &Data,
    guild: u64,
    url: &str,
    title: &str,
    details: &Value,
    front: bool,
) -> Result<(), Error> {
    let mut details = if details.is_object() {
        details.clone()
    } else {
        json!({})
    };
    details["request_id"] = rand::random::<u64>().into();
    let conn = data.db.lock();
    let next: i64 = conn.query_row(
        if front {
            "SELECT COALESCE(MIN(COALESCE(sort_order,id)),0)-1 FROM queue WHERE guild_id=?1"
        } else {
            "SELECT COALESCE(MAX(COALESCE(sort_order,id)),0)+1 FROM queue WHERE guild_id=?1"
        },
        [guild as i64],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO queue(guild_id,url,title,sort_order,details) VALUES(?1,?2,?3,?4,?5)",
        params![guild as i64, url, title, next, details.to_string()],
    )?;
    Ok(())
}
pub fn pop(data: &Data, guild: u64) -> Result<Option<Item>, Error> {
    // Вызывается под замком плеера; SQL-удаление проверяет, что строка не изменилась.
    for item in items(data, guild)? {
        if remove_snapshot(data, guild, std::slice::from_ref(&item))? > 0 {
            return Ok(Some(item));
        }
    }
    Ok(None)
}
pub fn remove_snapshot(data: &Data, guild: u64, rows: &[Item]) -> Result<usize, Error> {
    let mut conn = data.db.lock();
    let tx = conn.transaction()?;
    let mut count = 0;
    for row in rows {
        count+=tx.execute("DELETE FROM queue WHERE guild_id=?1 AND id=?2 AND url=?3 AND COALESCE(title,'')=?4 AND COALESCE(details,'null')=?5",params![guild as i64,row.id,row.url,row.title,row.details.to_string()])?;
    }
    tx.commit()?;
    Ok(count)
}
pub fn empty_seconds(data: &Data, guild: u64) -> i64 {
    data.db
        .lock()
        .query_row(
            "SELECT empty_seconds FROM music_preferences WHERE guild_id=?1",
            [guild as i64],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .ok()
        .flatten()
        .unwrap_or(180)
        .clamp(0, 3600)
}
pub(crate) fn same_voice(ctx: Context<'_>) -> bool {
    let Some(guild) = ctx.guild() else {
        return false;
    };
    let bot = ctx.framework().bot_id();
    let a = guild
        .voice_states
        .get(&ctx.author().id)
        .and_then(|v| v.channel_id);
    let b = guild.voice_states.get(&bot).and_then(|v| v.channel_id);
    a.is_some() && a == b
}
/// Удалить отдельный трек из очереди (номер из /queue)
#[poise::command(slash_command, guild_only)]
pub async fn queue_remove(
    ctx: Context<'_>,
    #[description = "Номер трека"]
    #[min = 1]
    position: i64,
) -> Result<(), Error> {
    if !same_voice(ctx) {
        return crate::reply::whisper(ctx, "Зайди в голосовой канал бота.").await;
    }
    let guild = ctx.guild_id().unwrap().get();
    let rows = items(ctx.data(), guild)?;
    let Some(item) = rows.get((position - 1) as usize) else {
        return crate::reply::whisper(ctx, "Такого номера в очереди нет.").await;
    };
    let moderator = ctx.author_member().await.is_some_and(|m| {
        crate::moderation::permissions(ctx.serenity_context(), ctx.guild_id().unwrap(), &m)
            .manage_guild()
    });
    if item.details["requester"].as_u64() != Some(ctx.author().id.get()) && !moderator {
        return crate::reply::whisper(
            ctx,
            "Удалить чужую или старую заявку может участник с правом «Управлять сервером».",
        )
        .await;
    }
    remove_snapshot(ctx.data(), guild, std::slice::from_ref(item))?;
    crate::reply::say(ctx, format!("🗑️ Удалён трек: {}", item.title)).await
}
/// Переместить трек в очереди
#[poise::command(slash_command, guild_only)]
pub async fn queue_move(
    ctx: Context<'_>,
    #[description = "Текущий номер"]
    #[min = 1]
    from: i64,
    #[description = "Новый номер"]
    #[min = 1]
    to: i64,
) -> Result<(), Error> {
    if !same_voice(ctx) {
        return crate::reply::whisper(ctx, "Зайди в голосовой канал бота.").await;
    }
    let guild = ctx.guild_id().unwrap().get();
    let mut rows = items(ctx.data(), guild)?;
    if from as usize > rows.len() || to as usize > rows.len() {
        return crate::reply::whisper(ctx, "Проверь номера в /queue.").await;
    }
    let row = rows.remove(from as usize - 1);
    rows.insert(to as usize - 1, row);
    {
        let mut conn = ctx.data().db.lock();
        let tx = conn.transaction()?;
        for (i, row) in rows.iter().enumerate() {
            tx.execute(
                "UPDATE queue SET sort_order=?1 WHERE guild_id=?2 AND id=?3",
                params![i as i64 + 1, guild as i64, row.id],
            )?;
        }
        tx.commit()?;
    }
    crate::reply::say(ctx, format!("↕️ Трек перемещён: {from} → {to}.")).await
}
/// Удалить из очереди только свои заявки
#[poise::command(slash_command, guild_only)]
pub async fn queue_clear_mine(ctx: Context<'_>) -> Result<(), Error> {
    if !same_voice(ctx) {
        return crate::reply::whisper(ctx, "Зайди в голосовой канал бота.").await;
    }
    let guild = ctx.guild_id().unwrap().get();
    let rows: Vec<_> = items(ctx.data(), guild)?
        .into_iter()
        .filter(|i| i.details["requester"].as_u64() == Some(ctx.author().id.get()))
        .collect();
    let n = remove_snapshot(ctx.data(), guild, &rows)?;
    crate::reply::say(ctx, format!("🗑️ Удалено твоих заявок: {n}.")).await
}
pub fn duration(seconds: Option<f64>) -> String {
    seconds
        .filter(|n| n.is_finite() && *n >= 0.)
        .map_or("неизвестна".into(), |s| {
            format!("{}:{:02}", s as u64 / 60, s as u64 % 60)
        })
}
pub fn card(title: &str, url: &str, meta: &Value, waiting: usize) -> s::CreateEmbed {
    let source = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| "неизвестен".into());
    let author = meta["requester"]
        .as_u64()
        .map_or("автоплей / старая заявка".into(), |id| {
            format!("<@{id}>")
        });
    let mut embed = s::CreateEmbed::new()
        .title(title.chars().take(250).collect::<String>())
        .url(url)
        .color(0x8B5CF6)
        .field("Длительность", duration(meta["duration"].as_f64()), true)
        .field("Заказал", author, true)
        .field("В очереди", waiting.to_string(), true)
        .field("Источник звука", source, true);
    if let Some(pic) = meta["thumbnail"].as_str() {
        if pic.starts_with("https://") {
            embed = embed.thumbnail(pic);
        }
    }
    embed
}
pub fn original_button(meta: &Value) -> Option<s::CreateButton> {
    let url = meta["original"].as_str()?;
    let parsed = reqwest::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    Some(
        s::CreateButton::new_link(url).label(if parsed.host_str() == Some("open.spotify.com") {
            "Открыть в Spotify"
        } else {
            "Открыть оригинал"
        }),
    )
}
/// Показать текущий трек и источник звука
#[poise::command(slash_command, guild_only)]
pub async fn nowplaying(ctx: Context<'_>) -> Result<(), Error> {
    let guild = ctx.guild_id().unwrap().get();
    let current = ctx.data().music.with(guild, |st| {
        st.now_playing.as_ref().map(|(_, title)| {
            (
                title.clone(),
                st.playing_url.clone().unwrap_or_default(),
                st.details.clone(),
            )
        })
    });
    let Some((title, url, details)) = current else {
        return crate::reply::whisper(ctx, "Сейчас ничего не играет.").await;
    };
    let mut buttons = crate::music::music_buttons();
    buttons.push(crate::music_library::favorite_button_for(
        &ctx.data().db, guild, &url, &title, &details,
    )?);
    if let Some(link) = original_button(&details) {
        buttons.push(s::CreateActionRow::Buttons(vec![link]));
    }
    ctx.send(
        poise::CreateReply::default()
            .embed(card(
                &title,
                &url,
                &details,
                ctx.data().db.queue_count(guild)?,
            ))
            .components(buttons)
            .allowed_mentions(s::CreateAllowedMentions::new()),
    )
    .await?;
    Ok(())
}

/// Получает названия, длительность и обложки без скачивания аудио.
pub async fn candidates(data: &Data, query: &str, count: usize) -> Result<Vec<Value>, Error> {
    let target = if query.starts_with("http") {
        query.to_owned()
    } else {
        format!(
            "ytsearch{count}:{}",
            query.strip_prefix("ytsearch1:").unwrap_or(query)
        )
    };
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        crate::music::ytdlp_command(data)
            .args([
                "--no-playlist",
                "--no-warnings",
                "--skip-download",
                "--dump-single-json",
            ])
            .arg(target)
            .output(),
    )
    .await??;
    if !output.status.success() {
        return Err(format!(
            "yt-dlp: {}",
            crate::music::hide_links(&String::from_utf8_lossy(&output.stderr))
        )
        .into());
    }
    let info: Value = serde_json::from_slice(&output.stdout)?;
    Ok(info
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| vec![info]))
}
fn words(s: &str) -> std::collections::BTreeSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}
pub fn score(query: &str, expected: Option<f64>, candidate: &Value) -> f64 {
    let wanted = words(query);
    let title = candidate["title"].as_str().unwrap_or("");
    let found = words(&format!(
        "{title} {}",
        candidate["uploader"].as_str().unwrap_or("")
    ));
    let overlap = wanted.intersection(&found).count() as f64 / wanted.len().max(1) as f64;
    let mut score = overlap * 100.;
    for variant in [
        "live",
        "remix",
        "cover",
        "karaoke",
        "sped",
        "slowed",
        "nightcore",
        "instrumental",
    ] {
        if found.contains(variant) && !wanted.contains(variant) {
            score -= 45.;
        }
    }
    if let (Some(expected), Some(actual)) = (expected, candidate["duration"].as_f64()) {
        score -= (expected - actual).abs().min(180.) * 0.8;
    }
    if candidate["is_live"].as_bool() == Some(true) {
        score -= 100.;
    }
    score
}
pub fn ranked(query: &str, expected: Option<f64>, mut list: Vec<Value>) -> Vec<Value> {
    list.retain(|v| v["webpage_url"].as_str().is_some());
    list.sort_by(|a, b| score(query, expected, b).total_cmp(&score(query, expected, a)));
    list
}
pub fn confident(query: &str, expected: Option<f64>, list: &[Value]) -> bool {
    list.first().is_some_and(|best| {
        score(query, expected, best) >= 80.
            && expected.is_none_or(|e| {
                best["duration"]
                    .as_f64()
                    .is_some_and(|a| (e - a).abs() <= 8.)
            })
    })
}
#[derive(Debug)]
pub struct Ambiguous {
    pub title: String,
    pub details: Value,
    pub list: Vec<Value>,
}
impl std::fmt::Display for Ambiguous {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "нужно выбрать версию трека")
    }
}
impl std::error::Error for Ambiguous {}
pub async fn offer(
    ctx: &s::Context,
    data: &Data,
    guild: u64,
    channel: s::ChannelId,
    choice: &Ambiguous,
) -> Result<(), Error> {
    let Some(user) = choice.details["requester"].as_u64() else {
        return Ok(());
    };
    let nonce = rand::random::<u64>().to_string();
    let payload = json!({"title":choice.title,"details":choice.details,"list":choice.list});
    data.db
        .lock()
        .execute("DELETE FROM music_choices WHERE expires<?1", [db::now()])?;
    data.db.lock().execute(
        "INSERT INTO music_choices(id,guild_id,user_id,expires,payload) VALUES(?1,?2,?3,?4,?5)",
        params![
            nonce,
            guild as i64,
            user as i64,
            db::now() + 600.,
            payload.to_string()
        ],
    )?;
    let text = choice
        .list
        .iter()
        .take(5)
        .enumerate()
        .map(|(i, v)| {
            format!(
                "{}. {} ({})",
                i + 1,
                v["title"].as_str().unwrap_or("трек"),
                duration(v["duration"].as_f64())
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let buttons = (0..choice.list.len().min(5))
        .map(|i| s::CreateButton::new(format!("version:{nonce}:{i}")).label((i + 1).to_string()))
        .collect();
    channel.send_message(ctx,s::CreateMessage::new().content(format!("<@{user}>, выбери версию **{}** (10 минут). Остальная очередь продолжает играть.\n{text}",choice.title)).components(vec![s::CreateActionRow::Buttons(buttons)]).allowed_mentions(s::CreateAllowedMentions::new())).await?;
    Ok(())
}
pub async fn version_pressed(
    ctx: &s::Context,
    data: &Shared,
    press: &s::ComponentInteraction,
) -> bool {
    let Some(rest) = press.data.custom_id.strip_prefix("version:") else {
        return false;
    };
    if let Err(e) = version_choice(ctx, data, press, rest).await {
        println!(
            "Выбор версии: {}",
            crate::interactions::sanitize_error(&e.to_string())
        );
    }
    true
}
async fn version_choice(
    ctx: &s::Context,
    data: &Shared,
    press: &s::ComponentInteraction,
    rest: &str,
) -> Result<(), Error> {
    press
        .create_response(ctx, s::CreateInteractionResponse::Acknowledge)
        .await?;
    let Some((nonce, index)) = rest.split_once(':') else {
        return Ok(());
    };
    let Some(guild) = press.guild_id else {
        return Ok(());
    };
    let row: Option<(u64, f64, String)> = data
        .db
        .lock()
        .query_row(
            "SELECT user_id,expires,payload FROM music_choices WHERE id=?1 AND guild_id=?2",
            params![nonce, guild.get() as i64],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let mut text = "Выбор уже сделан или срок истёк.".to_owned();
    if let Some((user, expires, payload)) = row {
        let same = ctx.cache.guild(guild).is_some_and(|g| {
            let a = g
                .voice_states
                .get(&press.user.id)
                .and_then(|v| v.channel_id);
            let b = g
                .voice_states
                .get(&ctx.cache.current_user().id)
                .and_then(|v| v.channel_id);
            a.is_some() && (b.is_none() || a == b)
        });
        if user != press.user.id.get() {
            text = "Это заявка другого участника.".into();
        } else if !same {
            text = "Сначала зайди в голосовой канал бота.".into();
        } else if expires > db::now() {
            let payload: Value = serde_json::from_str(&payload)?;
            if let Some(best) = index
                .parse::<usize>()
                .ok()
                .and_then(|i| payload["list"].as_array().and_then(|a| a.get(i)))
            {
                let title = best["title"].as_str().unwrap_or("трек");
                let url = best["webpage_url"].as_str().ok_or("нет ссылки")?;
                let mut meta = payload["details"].clone();
                meta["duration"] = best["duration"].clone();
                meta["thumbnail"] = best["thumbnail"].clone();
                meta["resolved"] = true.into();
                meta["request_id"] = rand::random::<u64>().into();
                let inserted = {
                    let mut conn = data.db.lock();
                    let tx = conn.transaction()?;
                    let deleted = tx.execute(
                        "DELETE FROM music_choices WHERE id=?1 AND expires>?2",
                        params![nonce, db::now()],
                    )?;
                    if deleted > 0 {
                        tx.execute("INSERT INTO queue(guild_id,url,title,sort_order,details) SELECT ?1,?2,?3,COALESCE(MAX(COALESCE(sort_order,id)),0)+1,?4 FROM queue WHERE guild_id=?1",params![guild.get() as i64,url,title,meta.to_string()])?;
                    }
                    tx.commit()?;
                    deleted > 0
                };
                if inserted {
                    press
                        .edit_response(
                            ctx,
                            s::EditInteractionResponse::new()
                                .content(format!("✅ Выбран трек: {title}"))
                                .components(vec![]),
                        )
                        .await?;
                    crate::music::play_next(ctx, data, guild, press.channel_id).await;
                    text = "Трек добавлен в очередь.".into();
                }
            }
        }
    }
    press
        .create_followup(
            ctx,
            s::CreateInteractionResponseFollowup::new()
                .content(text)
                .ephemeral(true),
        )
        .await?;
    Ok(())
}
pub async fn resolve(
    data: &Data,
    item: Item,
) -> Result<((String, String), String, String, Value), Error> {
    let mut details = if item.details.is_object() {
        item.details
    } else {
        json!({})
    };
    if details["resolved"] == true {
        return Ok((
            (item.url.clone(), item.title.clone()),
            item.title,
            item.url,
            details,
        ));
    }
    let query = details["spotify_query"]
        .as_str()
        .unwrap_or(&item.url)
        .to_owned();
    let expected = details["expected_duration"].as_f64();
    let list = candidates(
        data,
        &query,
        if details["spotify_query"].is_string() {
            5
        } else {
            1
        },
    )
    .await?;
    let list = ranked(&query, expected, list);
    if details["spotify_query"].is_string()
        && !confident(&query, expected, &list)
        && !list.is_empty()
    {
        return Err(Box::new(Ambiguous {
            title: item.title,
            details,
            list,
        }));
    }
    let best = list.first().ok_or("трек не найден")?;
    let url = best["webpage_url"].as_str().ok_or("нет ссылки")?.to_owned();
    let title = best["title"].as_str().unwrap_or(&item.title).to_owned();
    details["duration"] = best["duration"].clone();
    details["thumbnail"] = best["thumbnail"].clone();
    details["resolved"] = true.into();
    Ok(((url.clone(), title.clone()), title, url, details))
}
pub async fn choose(
    ctx: Context<'_>,
    query: &str,
    expected: Option<f64>,
    list: Vec<Value>,
) -> Result<Option<Value>, Error> {
    let list = ranked(query, expected, list);
    if confident(query, expected, &list) {
        return Ok(list.into_iter().next());
    }
    if list.is_empty() {
        return Err("Не найдено подходящих вариантов".into());
    }
    let body = list
        .iter()
        .take(5)
        .enumerate()
        .map(|(i, v)| {
            format!(
                "{}. {} ({})",
                i + 1,
                v["title"]
                    .as_str()
                    .unwrap_or("Без названия")
                    .chars()
                    .take(180)
                    .collect::<String>(),
                duration(v["duration"].as_f64())
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let buttons = (0..list.len().min(5))
        .map(|i| s::CreateButton::new(format!("match:{i}")).label((i + 1).to_string()))
        .chain(std::iter::once(
            s::CreateButton::new("match:cancel").label("Отмена"),
        ))
        .collect();
    let reply = ctx
        .send(
            poise::CreateReply::default()
                .content(format!(
                    "Не уверен в совпадении **{query}**. Выбери версию (60 секунд):\n{body}"
                ))
                .components(vec![s::CreateActionRow::Buttons(buttons)])
                .allowed_mentions(s::CreateAllowedMentions::new()),
        )
        .await?;
    let id = reply.message().await?.id;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let selected = loop {
        let press = s::ComponentInteractionCollector::new(ctx.serenity_context())
            .message_id(id)
            .timeout(deadline.saturating_duration_since(tokio::time::Instant::now()))
            .await;
        let Some(press) = press else { break None };
        if press.user.id != ctx.author().id {
            press
                .create_response(
                    ctx,
                    s::CreateInteractionResponse::Message(
                        s::CreateInteractionResponseMessage::new()
                            .content("Выбрать версию может только автор заявки.")
                            .ephemeral(true),
                    ),
                )
                .await?;
            continue;
        }
        press
            .create_response(ctx, s::CreateInteractionResponse::Acknowledge)
            .await?;
        break press
            .data
            .custom_id
            .strip_prefix("match:")
            .and_then(|s| s.parse::<usize>().ok())
            .and_then(|i| list.get(i).cloned());
    };
    reply
        .edit(
            ctx,
            poise::CreateReply::default()
                .content(if selected.is_some() {
                    "✅ Версия выбрана."
                } else {
                    "Выбор отменён, трек не добавлен."
                })
                .components(vec![]),
        )
        .await?;
    Ok(selected)
}

pub fn start_idle_loop(ctx: s::Context, data: Shared) {
    tokio::spawn(async move {
        let mut empty: std::collections::HashMap<u64, (u64, Instant)> = Default::default();
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            for guild in ctx.cache.guilds() {
                let limit = empty_seconds(&data, guild.get());
                let snapshot = ctx.cache.guild(guild).and_then(|g| {
                    let channel = g
                        .voice_states
                        .get(&ctx.cache.current_user().id)?
                        .channel_id?;
                    let human = g.voice_states.values().any(|v| {
                        v.channel_id == Some(channel)
                            && v.user_id != ctx.cache.current_user().id
                            && !g
                                .members
                                .get(&v.user_id)
                                .or(v.member.as_ref())
                                .is_some_and(|m| m.user.bot)
                    });
                    Some((channel.get(), human))
                });
                let Some((channel, false)) = snapshot else {
                    empty.remove(&guild.get());
                    continue;
                };
                if limit == 0 {
                    empty.remove(&guild.get());
                    continue;
                }
                let entry = empty
                    .entry(guild.get())
                    .or_insert((channel, Instant::now()));
                if entry.0 != channel {
                    *entry = (channel, Instant::now());
                }
                if entry.1.elapsed() < Duration::from_secs(limit as u64) {
                    continue;
                }
                let lock = data.music.lock_for(guild.get());
                let _guard = lock.lock().await;
                // Повторная проверка под замком: человек мог зайти, пока плеер искал трек.
                let still_empty = ctx.cache.guild(guild).is_some_and(|g| {
                    g.voice_states
                        .get(&ctx.cache.current_user().id)
                        .and_then(|v| v.channel_id)
                        .map(|id| id.get())
                        == Some(channel)
                        && !g.voice_states.values().any(|v| {
                            v.channel_id.map(|id| id.get()) == Some(channel)
                                && v.user_id != ctx.cache.current_user().id
                                && !g
                                    .members
                                    .get(&v.user_id)
                                    .or(v.member.as_ref())
                                    .is_some_and(|m| m.user.bot)
                        })
                });
                if !still_empty {
                    empty.remove(&guild.get());
                    continue;
                }
                if let Err(e) = crate::music::disconnect_saved(&ctx, &data, guild).await {
                    println!("Автовыход: {e}");
                }
                empty.remove(&guild.get());
            }
            empty.retain(|g, _| ctx.cache.guild(s::GuildId::new(*g)).is_some());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions() {
        let good = json!({"title":"Artist Song official audio","duration":200,"webpage_url":"https://example.com/a"});
        let remix = json!({"title":"Artist Song remix","duration":201,"webpage_url":"https://example.com/b"});
        let list = ranked("Artist Song", Some(200.), vec![remix, good.clone()]);
        assert_eq!(list[0], good);
        assert!(confident("Artist Song", Some(200.), &list));
        assert!(!confident("Other song", Some(500.), &list));
    }
    #[test]
    fn stale_clear_does_not_remove_new_duplicate() {
        let data = Data::new(
            db::Database::in_memory().unwrap(),
            &crate::config::Settings::default(),
            Default::default(),
            false,
        );
        enqueue(&data, 1, "url", "same", &json!({"requester":1}), false).unwrap();
        let snapshot = items(&data, 1).unwrap();
        remove_snapshot(&data, 1, &snapshot).unwrap();
        enqueue(&data, 1, "url", "same", &json!({"requester":1}), false).unwrap();
        assert_eq!(remove_snapshot(&data, 1, &snapshot).unwrap(), 0);
        assert_eq!(items(&data, 1).unwrap().len(), 1);
    }
    #[test]
    fn duplicate_requests_and_move() {
        let data = Data::new(
            db::Database::in_memory().unwrap(),
            &crate::config::Settings::default(),
            Default::default(),
            false,
        );
        enqueue(&data, 1, "url", "song", &json!({"requester":1}), false).unwrap();
        enqueue(&data, 1, "url", "song", &json!({"requester":2}), false).unwrap();
        let rows = items(&data, 1).unwrap();
        remove_snapshot(&data, 1, &rows[..1]).unwrap();
        assert_eq!(items(&data, 1).unwrap()[0].details["requester"], 2);
        enqueue(&data, 1, "front", "first", &Value::Null, true).unwrap();
        assert_eq!(pop(&data, 1).unwrap().unwrap().url, "front");
    }
}
