//! Музыка: /play (YouTube, SoundCloud, Spotify), очередь в базе (переживает /leave и перезапуск), /skip, /pause,
//! /resume, /queue, /remove_all, /setvolume, /leave, кнопки под «Играет» (пауза, дальше, повтор, перемешать),
//! /autoplay, плейлисты сервера, /lyrics и /top_chart_month — как в bot.py.
//!
//! Звук, как и в bot.py, качает yt-dlp и отдаёт его боту через «трубу» (pipe): по прямой ссылке YouTube
//! на серверах отвечает ошибкой 403.

use crate::admin::report_error;
use crate::cards::{Picture, Row};
use crate::reply::whisper;
use crate::text::plural;
use crate::{db, Context, Data, Error, Shared};
use poise::serenity_prelude as serenity;
use rand::seq::{IndexedRandom, SliceRandom};
use rusqlite::{params, OptionalExtension};
use songbird::events::{Event, EventContext, EventData, TrackEvent};
use songbird::input::{AudioStream, Input, LiveInput};
#[cfg(test)]
use songbird::input::ChildContainer;
use songbird::tracks::{PlayMode, Track, TrackHandle};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) const TRACK_TOO_SHORT: Duration = Duration::from_secs(3); // трек «закончился» быстрее — значит, звук не загрузился
type PlaylistTrack = (String, String, serde_json::Value);

#[derive(Debug, PartialEq, Eq)]
enum SavePlaylist {
    Saved,
    Denied,
    Limit,
}

const MAX_PLAYLISTS: usize = 25; // сколько плейлистов может сохранить один сервер
const MAX_PLAYLIST_SAVE: usize = 100; // сколько треков помещается в один плейлист
const NOT_PLAYING: &str = "Сейчас ничего не играет.";
const MONTH_NAMES: [&str; 12] = [
    "Январь",
    "Февраль",
    "Март",
    "Апрель",
    "Май",
    "Июнь",
    "Июль",
    "Август",
    "Сентябрь",
    "Октябрь",
    "Ноябрь",
    "Декабрь",
];

/// Only page URLs for the supported playback providers may cross the downloader boundary.
pub(crate) fn provider_url(text: &str) -> Option<reqwest::Url> {
    let url = reqwest::Url::parse(text.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let host = url.host_str()?;
    matches!(
        host,
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com"
            | "youtu.be" | "www.youtube-nocookie.com"
            | "soundcloud.com" | "www.soundcloud.com" | "m.soundcloud.com"
            | "on.soundcloud.com" | "snd.sc"
    )
    .then_some(url)
}

pub(crate) fn ytdlp_target(query: &str, count: usize) -> Result<String, Error> {
    let query = query.trim();
    if let Some(url) = provider_url(query) {
        return Ok(url.to_string());
    }
    if query.contains("://") || query.starts_with("//") {
        return Err("Поддерживаются ссылки YouTube, SoundCloud и Spotify.".into());
    }
    let query = query.strip_prefix("ytsearch1:").unwrap_or(query);
    if query.trim().is_empty() || query.chars().any(char::is_control) {
        return Err("Укажи название трека или ссылку.".into());
    }
    Ok(format!("ytsearch{}:{query}", count.clamp(1, 5)))
}

fn tracks_text(count: usize) -> String {
    format!(
        "{count} {}",
        plural(count as i64, "трек", "трека", "треков")
    )
}

// ---------- Что играет на каждом сервере (после перезапуска — с нуля, очередь — в базе) ----------

#[derive(Default)]
pub(crate) struct GuildMusic {
    pub(crate) now_playing: Option<(String, String)>, // (ссылка, название) — при /leave вернётся в начало очереди
    pub(crate) playing_url: Option<String>, // страница трека, который играет (для автоплея)
    track: Option<TrackHandle>,
    instance: Option<String>,
    pub(crate) snapshot_id: Option<i64>,
    voice_generation: u64,
    autoplay_generation: u64,
    pub(crate) details: serde_json::Value,
    skipped: bool,       // трек остановили сами (/skip, /leave) — это не ошибка звука
    repeat: bool,        // кнопка 🔁
    volume: Option<f32>, // 1.0 = 100%
}

#[derive(Default)]
pub struct MusicState {
    guilds: Mutex<HashMap<u64, GuildMusic>>,
    /// «Замки»: не дают включить два трека одновременно, если play_next вызвали сразу из двух мест
    locks: Mutex<HashMap<u64, Arc<tokio::sync::Mutex<()>>>>,
    /// Serializes voice destination checks/joins, independently of long track resolution.
    voice_operations: Mutex<HashMap<u64, Arc<tokio::sync::Mutex<()>>>>,
    /// Сервер → ссылки, которые недавно играли (чтобы автоплей не ходил по кругу)
    autoplay_recent: Mutex<HashMap<u64, Vec<String>>>,
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl MusicState {
    pub(crate) fn with<T>(&self, guild: u64, action: impl FnOnce(&mut GuildMusic) -> T) -> T {
        action(locked(&self.guilds).entry(guild).or_default())
    }

    pub(crate) fn lock_for(&self, guild: u64) -> Arc<tokio::sync::Mutex<()>> {
        locked(&self.locks).entry(guild).or_default().clone()
    }

    pub(crate) async fn voice_operation(&self, guild: u64) -> tokio::sync::OwnedMutexGuard<()> {
        let operation = locked(&self.voice_operations)
            .entry(guild)
            .or_default()
            .clone();
        operation.lock_owned().await
    }
}

/// Retire only this playing instance. Callers serialize the durable transition
/// with play_next using lock_for; a late End/Error must leave a newer track intact.
fn take_playback(state: &mut GuildMusic, instance: &str) -> Option<(Option<String>, bool, bool)> {
    if state.instance.as_deref() != Some(instance) {
        return None;
    }
    state.instance = None;
    state.track = None;
    state.now_playing = None;
    state.snapshot_id = None;
    Some((
        state.playing_url.take(),
        std::mem::take(&mut state.skipped),
        state.repeat,
    ))
}

/// Играет ли что-то (или стоит на паузе). Трек, который songbird уже выбросил (бота выкинули из канала), не в счёт.
async fn current_mode(data: &Data, guild: u64) -> Option<PlayMode> {
    let track = data.music.with(guild, |state| state.track.clone())?;
    match track.get_info().await {
        Ok(info) if matches!(info.playing, PlayMode::Play | PlayMode::Pause) => Some(info.playing),
        _ => None,
    }
}

// ---------- Очередь в базе (таблица queue, та же, что у bot.py) ----------

impl db::Database {
    pub fn queue_add(&self, guild: u64, url: &str, title: &str) -> rusqlite::Result<()> {
        self.lock().execute("INSERT INTO queue (guild_id,url,title,sort_order,details) SELECT ?1,?2,?3,COALESCE(MAX(COALESCE(sort_order,id)),0)+1,?4 FROM queue WHERE guild_id=?1", params![guild as i64,url,title,serde_json::json!({"request_id":rand::random::<u64>()}).to_string()])?;
        Ok(())
    }

    /// Трек в самое начало очереди (номер меньше, чем у всех остальных).
    pub fn queue_push_front(&self, guild: u64, url: &str, title: &str) -> rusqlite::Result<()> {
        let connection = self.lock();
        let smallest: Option<i64> =
            connection.query_row("SELECT MIN(id) FROM queue", [], |row| row.get(0))?;
        connection.execute(
            "INSERT INTO queue (id, guild_id, url, title) VALUES (?1, ?2, ?3, ?4)",
            params![smallest.unwrap_or(1) - 1, guild as i64, url, title],
        )?;
        Ok(())
    }

    /// Достаёт первый трек очереди и удаляет его оттуда.
    pub fn queue_pop(&self, guild: u64) -> rusqlite::Result<Option<(String, String)>> {
        let connection = self.lock();
        let row = connection
            .query_row("SELECT id, url, title FROM queue WHERE guild_id = ?1 ORDER BY COALESCE(sort_order,id),id LIMIT 1", params![guild as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?.unwrap_or_default()))
            })
            .optional()?;
        let Some((id, url, title)) = row else {
            return Ok(None);
        };
        connection.execute("DELETE FROM queue WHERE id = ?1", params![id])?;
        Ok(Some((url, title)))
    }

    pub fn queue_tracks(&self, guild: u64, limit: i64) -> rusqlite::Result<Vec<(String, String)>> {
        let connection = self.lock();
        let mut statement = connection.prepare("SELECT url, title FROM queue WHERE guild_id = ?1 ORDER BY COALESCE(sort_order,id),id LIMIT ?2")?;
        let rows = statement.query_map(params![guild as i64, limit], |row| {
            Ok((
                row.get(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })?;
        rows.collect()
    }

    pub fn queue_count(&self, guild: u64) -> rusqlite::Result<usize> {
        self.lock()
            .query_row(
                "SELECT COUNT(*) FROM queue WHERE guild_id = ?1",
                params![guild as i64],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n as usize)
    }

    pub fn queue_clear(&self, guild: u64) -> rusqlite::Result<usize> {
        self.lock().execute(
            "DELETE FROM queue WHERE guild_id = ?1",
            params![guild as i64],
        )
    }

    /// Перемешивает очередь: номера остаются, меняются местами сами треки. Возвращает, сколько в ней треков.
    pub fn queue_shuffle(&self, guild: u64) -> rusqlite::Result<usize> {
        let mut connection = self.lock();
        let tx = connection.transaction()?;
        let mut ids: Vec<i64> = {
            let mut st = tx.prepare(
                "SELECT id FROM queue WHERE guild_id=?1 ORDER BY COALESCE(sort_order,id),id",
            )?;
            let rows = st.query_map([guild as i64], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        ids.shuffle(&mut rand::rng());
        for (i, id) in ids.iter().enumerate() {
            tx.execute(
                "UPDATE queue SET sort_order=?1 WHERE id=?2",
                params![i as i64 + 1, id],
            )?;
        }
        tx.commit()?;
        Ok(ids.len())
    }

    /// Засчитывает прослушивание трека в этом месяце (прошлые месяцы стираются — топ сам обнуляется 1-го числа).
    pub fn record_play(&self, guild: u64, month: &str, title: &str) -> rusqlite::Result<()> {
        let connection = self.lock();
        connection.execute("DELETE FROM plays WHERE month != ?1", params![month])?;
        connection.execute(
            "INSERT INTO plays (guild_id, month, title, count) VALUES (?1, ?2, ?3, 1)
             ON CONFLICT (guild_id, month, title) DO UPDATE SET count = count + 1",
            params![guild as i64, month, title],
        )?;
        Ok(())
    }

    pub fn top_tracks(&self, guild: u64, month: &str) -> rusqlite::Result<Vec<(String, i64)>> {
        let connection = self.lock();
        connection.execute("DELETE FROM plays WHERE month != ?1", params![month])?;
        let mut statement = connection.prepare("SELECT title, count FROM plays WHERE guild_id = ?1 AND month = ?2 ORDER BY count DESC, title LIMIT 10")?;
        let rows = statement.query_map(params![guild as i64, month], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        rows.collect()
    }

    pub fn autoplay(&self, guild: u64) -> rusqlite::Result<bool> {
        let value = self
            .lock()
            .query_row(
                "SELECT autoplay FROM music_settings WHERE guild_id = ?1",
                params![guild as i64],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?;
        Ok(value.flatten().unwrap_or(0) != 0)
    }

    pub fn set_autoplay(&self, guild: u64, enabled: bool) -> rusqlite::Result<()> {
        self.lock().execute(
            "INSERT OR REPLACE INTO music_settings (guild_id, autoplay) VALUES (?1, ?2)",
            params![guild as i64, enabled as i64],
        )?;
        Ok(())
    }

    /// Плейлисты сервера: (название, сколько треков), по алфавиту.
    pub fn playlists(&self, guild: u64) -> rusqlite::Result<Vec<(String, i64)>> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT name, COUNT(*) FROM playlists WHERE guild_id = ?1 GROUP BY name ORDER BY name",
        )?;
        let rows =
            statement.query_map(params![guild as i64], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect()
    }

    fn playlist(&self, guild: u64, name: &str) -> rusqlite::Result<Vec<PlaylistTrack>> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT url,title,details FROM playlists WHERE guild_id=?1 AND name=?2 ORDER BY position",
        )?;
        let rows = statement.query_map(params![guild as i64, name], |row| {
            let url: String = row.get(0)?;
            let details = row.get::<_, Option<String>>(2)?
                .and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok())
                .filter(|value| value.is_object())
                .unwrap_or_else(|| serde_json::json!({"original":url}));
            Ok((url, row.get::<_, Option<String>>(1)?.unwrap_or_default(), details))
        })?;
        rows.collect()
    }

    fn save_playlist(
        &self,
        guild: u64,
        name: &str,
        tracks: &[PlaylistTrack],
        can_replace: bool,
    ) -> rusqlite::Result<SavePlaylist> {
        let mut connection = self.lock();
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM playlists WHERE guild_id=?1 AND name=?2)",
            params![guild as i64, name], |r| r.get(0),
        )?;
        if exists && !can_replace {
            return Ok(SavePlaylist::Denied);
        }
        if !exists {
            let count: i64 = tx.query_row(
                "SELECT COUNT(DISTINCT name) FROM playlists WHERE guild_id=?1",
                [guild as i64], |r| r.get(0),
            )?;
            if count >= MAX_PLAYLISTS as i64 {
                return Ok(SavePlaylist::Limit);
            }
        }
        tx.execute("DELETE FROM playlists WHERE guild_id=?1 AND name=?2", params![guild as i64, name])?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO playlists(guild_id,name,position,url,title,details) VALUES(?1,?2,?3,?4,?5,?6)",
            )?;
            for (position, (url, title, details)) in tracks.iter().enumerate() {
                insert.execute(params![guild as i64, name, position as i64, url, title, details.to_string()])?;
            }
        }
        tx.commit()?;
        Ok(SavePlaylist::Saved)
    }

    pub fn delete_playlist(&self, guild: u64, name: &str) -> rusqlite::Result<bool> {
        Ok(self.lock().execute(
            "DELETE FROM playlists WHERE guild_id = ?1 AND name = ?2",
            params![guild as i64, name],
        )? > 0)
    }
}

// ---------- yt-dlp ----------

/// Команда yt-dlp («yt-dlp» или «python -m yt_dlp» из переменной YTDLP).
fn downloader_environment() -> impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)> {
    std::env::vars_os().filter(|(key, _)| {
        matches!(
            key.to_string_lossy().to_ascii_uppercase().as_str(),
            "PATH" | "PATHEXT" | "HOME" | "USERPROFILE" | "APPDATA" | "LOCALAPPDATA"
                | "SYSTEMROOT" | "WINDIR" | "TEMP" | "TMP" | "TMPDIR"
                | "LANG" | "LC_ALL" | "LC_CTYPE" | "XDG_CONFIG_HOME" | "XDG_CACHE_HOME"
                | "SSL_CERT_FILE" | "SSL_CERT_DIR" | "DENO_DIR" | "VIRTUAL_ENV"
        )
    })
}

fn downloader(data: &Data) -> std::process::Command {
    let (program, args) = crate::ytdlp_parts(&data.ytdlp);
    let mut command = std::process::Command::new(program);
    command
        .args(args.split_whitespace())
        .env_clear()
        .envs(downloader_environment())
        .stdin(Stdio::null())
        .args(["--use-extractors", "youtube.*,soundcloud.*", "--socket-timeout", "15"]);
    command
}

pub(crate) fn ytdlp_command(data: &Data) -> tokio::process::Command {
    let mut command = tokio::process::Command::from(downloader(data));
    command.kill_on_drop(true);
    command
}

pub(crate) async fn downloader_output(
    mut command: tokio::process::Command,
) -> Result<std::process::Output, Error> {
    use tokio::io::{AsyncRead, AsyncReadExt};

    async fn capped(reader: impl AsyncRead + Unpin, limit: u64) -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        reader.take(limit + 1).read_to_end(&mut bytes).await?;
        if bytes.len() as u64 > limit {
            return Err(std::io::Error::other("yt-dlp превысил лимит вывода"));
        }
        Ok(bytes)
    }

    static DOWNLOADS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
    tokio::time::timeout(Duration::from_secs(60), async {
        let _permit = DOWNLOADS.acquire().await?;
        let mut child = command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let stdout = child.stdout.take().ok_or("yt-dlp stdout недоступен")?;
        let stderr = child.stderr.take().ok_or("yt-dlp stderr недоступен")?;
        let result = tokio::try_join!(
            capped(stdout, 16 * 1024 * 1024),
            capped(stderr, 64 * 1024),
            child.wait(),
        );
        match result {
            Ok((stdout, stderr, status)) => Ok(std::process::Output {stdout, stderr, status}),
            Err(error) => {
                let _ = child.kill().await;
                Err(error.into())
            }
        }
    })
    .await
    .map_err(|_| "yt-dlp ждал дольше минуты")?
}

/// Ищет трек. Возвращает (название, ссылка на страницу трека) — звук по ней качается заново перед включением.
#[cfg(test)]
async fn search(data: &Data, query: &str) -> Result<(String, String), Error> {
    let mut command = ytdlp_command(data);
    command
        .args([
            "--no-playlist",
            "--no-warnings",
            "--format",
            "bestaudio/best",
            "--print",
            "title",
            "--print",
            "webpage_url",
        ])
        .arg("--")
        .arg(ytdlp_target(query, 1)?);
    let output = downloader_output(command)
        .await
        .map_err(|error| {
            format!(
                "не получилось запустить yt-dlp (`{}`): {error}. Задай переменную YTDLP",
                data.ytdlp
            )
        })?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    match (lines.next(), lines.next()) {
        (Some(title), Some(url)) if output.status.success() => {
            Ok((title.to_string(), url.to_string()))
        }
        _ => Err(format!(
            "yt-dlp ничего не нашёл: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into()),
    }
}

/// id видео YouTube из ссылки (v=…, youtu.be/…, /shorts/…).
pub fn youtube_id(url: &str) -> Option<String> {
    let url = provider_url(url)?;
    let id = match url.host_str()? {
        "youtu.be" => url.path_segments()?.next()?.to_owned(),
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com"
        | "www.youtube-nocookie.com" => {
            if url.path() == "/watch" {
                url.query_pairs().find(|(key, _)| key == "v")?.1.into_owned()
            } else {
                let mut parts = url.path_segments()?;
                if !matches!(parts.next()?, "shorts" | "live" | "embed" | "v") {
                    return None;
                }
                parts.next()?.to_owned()
            }
        }
        _ => return None,
    };
    (id.len() == 11 && id.bytes().all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)))
        .then_some(id)
}

/// Строки yt-dlp «id, название, id, название…» → треки без самого видео.
pub fn parse_mix(output: &str, video: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = output.lines().map(str::trim).collect();
    lines
        .chunks(2)
        .filter(|pair| pair.len() == 2 && !pair[0].is_empty() && pair[0] != video)
        .map(|pair| {
            (
                format!("https://www.youtube.com/watch?v={}", pair[0]),
                if pair[1].is_empty() || pair[1] == "NA" {
                    "Без названия".into()
                } else {
                    pair[1].to_string()
                },
            )
        })
        .collect()
}

/// Похожие треки с YouTube: «микс» (автоматический плейлист YouTube) для этого видео.
async fn similar_tracks(data: &Data, url: &str) -> Vec<(String, String)> {
    let Some(video) = youtube_id(url) else {
        return Vec::new();
    };
    let mut command = ytdlp_command(data);
    command
        .args([
            "--flat-playlist",
            "--yes-playlist",
            "--playlist-end",
            "15",
            "--no-warnings",
            "--print",
            "id",
            "--print",
            "title",
        ])
        .arg("--")
        .arg(format!(
            "https://www.youtube.com/watch?v={video}&list=RD{video}"
        ));
    match downloader_output(command).await {
        Ok(output) if output.status.success() => parse_mix(&String::from_utf8_lossy(&output.stdout), &video),
        other => {
            println!("Автоплей: не получилось найти похожие треки: {other:?}");
            Vec::new()
        }
    }
}

/// Убирает длинные ссылки из сообщений yt-dlp.
pub fn hide_links(text: &str) -> String {
    text.split(' ')
        .map(|word| {
            if word.starts_with("http://") || word.starts_with("https://") {
                "<ссылка>"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Главная причина ошибки: строка yt-dlp «ERROR: …», иначе последняя строка.
pub fn failure_reason(details: &str, fallback: &str) -> String {
    let lines: Vec<&str> = details
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let reason = lines
        .iter()
        .rev()
        .find(|line| line.starts_with("ERROR:"))
        .or(lines.last())
        .copied()
        .unwrap_or(fallback);
    reason.chars().take(300).collect()
}

// ---------- Включение треков ----------

pub(crate) fn music_buttons() -> Vec<serenity::CreateActionRow> {
    let button = |id: &str, emoji: char| {
        serenity::CreateButton::new(id)
            .emoji(emoji)
            .style(serenity::ButtonStyle::Secondary)
    };
    vec![serenity::CreateActionRow::Buttons(vec![
        button("music:pause", '⏯'),
        button("music:skip", '⏭'),
        button("music:repeat", '🔁'),
        button("music:shuffle", '🔀'),
    ])]
}

async fn say_in(ctx: &serenity::Context, channel: serenity::ChannelId, text: &str) {
    let message = serenity::CreateMessage::new()
        .content(text)
        .allowed_mentions(serenity::CreateAllowedMentions::new());
    let _ = channel.send_message(ctx, message).await;
}

/// В каком голосовом канале бот на этом сервере (None — не в канале).
async fn bot_channel(
    ctx: &serenity::Context,
    guild: serenity::GuildId,
) -> Option<songbird::id::ChannelId> {
    let call = songbird::get(ctx).await?.get(guild)?;
    let channel = call.lock().await.current_channel();
    channel
}

/// Голосовой канал участника.
fn user_channel(
    ctx: &serenity::Context,
    guild: serenity::GuildId,
    user: serenity::UserId,
) -> Option<serenity::ChannelId> {
    ctx.cache.guild(guild)?.voice_states.get(&user)?.channel_id
}

/// Заходит в голосовой канал (если бот ещё не там). Текст ошибки — если не получилось.
async fn join(
    ctx: &serenity::Context,
    data: &Data,
    guild: serenity::GuildId,
    channel: serenity::ChannelId,
) -> Result<(), String> {
    let _operation = data.music.voice_operation(guild.get()).await;
    join_inner(ctx, guild, channel).await
}

/// Returns whether a join is needed; an existing connection may never be moved
/// to another caller's channel as a side effect of replay or /play.
fn voice_destination(current: Option<u64>, requested: u64) -> Result<bool, String> {
    match current {
        None => Ok(true),
        Some(current) if current == requested => Ok(false),
        Some(_) => Err("Зайди в голосовой канал бота.".into()),
    }
}

/// Caller holds MusicState::voice_operation; never take the play_next lock here.
async fn join_inner(
    ctx: &serenity::Context,
    guild: serenity::GuildId,
    channel: serenity::ChannelId,
) -> Result<(), String> {
    if !voice_destination(
        bot_channel(ctx, guild).await.map(|id| id.0.get()),
        channel.get(),
    )? {
        return Ok(());
    }
    let manager = songbird::get(ctx)
        .await
        .ok_or("голосовой модуль не запущен")?;
    manager
        .join(guild, channel)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Replay a selected saved song through the normal queue/resolver/play_next flow.
async fn join_for_command(ctx: Context<'_>, guild: serenity::GuildId) -> Result<(), Error> {
    let discord = ctx.serenity_context();
    let data = ctx.data();
    let _operation = data.music.voice_operation(guild.get()).await;
    if !crate::music_channels::ctx_allowed(ctx)? {
        return Err("Используй настроенный музыкальный канал.".into());
    }
    let voice = user_channel(discord, guild, ctx.author().id).ok_or("Сначала зайди в голосовой канал.")?;
    join_inner(discord, guild, voice).await.map_err(|error| format!("Не получилось зайти в голосовой канал: {error}"))?;
    let bot = bot_channel(discord, guild).await;
    if bot.is_none_or(|bot| bot.0.get() != voice.get())
        || user_channel(discord, guild, ctx.author().id) != Some(voice)
        || !crate::music_channels::ctx_allowed(ctx)?
    {
        return Err("Голосовой или музыкальный канал изменился.".into());
    }
    Ok(())
}

/// Callers must defer their interaction before awaiting this helper.
pub async fn enqueue_saved(
    ctx: &serenity::Context,
    data: &Shared,
    guild: serenity::GuildId,
    user: serenity::UserId,
    channel: serenity::ChannelId,
    track: &crate::music_library::SavedTrack,
) -> Result<(), Error> {
    enqueue_saved_many(ctx, data, guild, user, channel, std::slice::from_ref(track)).await?;
    Ok(())
}

/// Join/check voice once and start the normal queue once for a bounded collection.
/// Returns the number queued; at most MAX_BULK_FAVORITES (200) are accepted per call.
pub async fn enqueue_saved_many(
    ctx: &serenity::Context,
    data: &Shared,
    guild: serenity::GuildId,
    user: serenity::UserId,
    channel: serenity::ChannelId,
    tracks: &[crate::music_library::SavedTrack],
) -> Result<usize, Error> {
    if tracks.is_empty() {
        return Ok(0);
    }
    let operation = data.music.voice_operation(guild.get()).await;
    let voice = user_channel(ctx, guild, user).ok_or("Сначала зайди в голосовой канал.")?;
    join_inner(ctx, guild, voice)
        .await
        .map_err(|error| format!("Не получилось зайти в голосовой канал: {error}"))?;
    // Joining is asynchronous: reject users who left/moved before queue insertion.
    let current_voice = user_channel(ctx, guild, user)
        .ok_or("Ты вышел из голосового канала — трек не добавлен.")?;
    if current_voice != voice
        || bot_channel(ctx, guild)
            .await
            .is_none_or(|bot| bot.0.get() != current_voice.get())
    {
        return Err("Зайди в голосовой канал бота — трек не добавлен.".into());
    }
    let mut count = 0;
    for track in tracks.iter().take(crate::music_library::MAX_BULK_FAVORITES) {
        crate::music_library::enqueue_track(data, guild.get(), user.get(), track)?;
        crate::music_runtime::queue_changed(data, guild.get());
        count += 1;
    }
    drop(operation);
    play_next(ctx, data, guild, channel).await;
    Ok(count)
}

/// A single confirmation guard is shared by progress, End and Error callbacks.
/// Its identity belongs to the playing instance, not the queue request/repeat URL.
#[derive(Clone)]
struct LibraryPlayback {
    data: Shared,
    guild: u64,
    snapshot_id: i64,
    instance: String,
    confirmed: Arc<Mutex<bool>>,
}

impl LibraryPlayback {
    fn observe(&self, event: &EventContext<'_>) -> bool {
        let EventContext::Track(tracks) = event else {
            return false;
        };
        let Some((state, _)) = tracks.first() else {
            return false;
        };
        let mut confirmed = locked(&self.confirmed);
        if *confirmed {
            return true;
        }
        // Songbird play_time advances with decoded frames, unlike time since spawn.
        match crate::music_library::record_confirmed(
            &self.data.db,
            self.guild,
            self.snapshot_id,
            &self.instance,
            state.play_time,
            chrono::Utc::now().timestamp(),
        ) {
            Ok(true) => {
                *confirmed = true;
                // Preserve the existing monthly chart, but only count confirmed audio.
                let title = self
                    .data
                    .db
                    .lock()
                    .query_row(
                        "SELECT title FROM music_library_tracks WHERE id=?1",
                        [self.snapshot_id],
                        |r| r.get::<_, String>(0),
                    )
                    .ok();
                if let Some(title) = title {
                    let _ = self.data.db.record_play(
                        self.guild,
                        &chrono::Local::now().format("%Y-%m").to_string(),
                        &title,
                    );
                }
                true
            }
            Ok(false) => false,
            Err(error) => {
                println!("История музыки: {error}");
                false
            }
        }
    }
}

#[async_trait::async_trait]
impl songbird::EventHandler for LibraryPlayback {
    async fn act(&self, event: &EventContext<'_>) -> Option<Event> {
        if self.observe(event) {
            Some(Event::Cancel)
        } else {
            None
        }
    }
}

struct AudioDownload(Mutex<Option<std::process::Child>>);

impl AudioDownload {
    fn stop(&self) {
        if let Some(mut child) = locked(&self.0).take() {
            let mut cleanup = move || {
                let _ = child.kill();
                if let Err(error) = child.wait() {
                    eprintln!("yt-dlp cleanup: {error}");
                }
            };
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn_blocking(cleanup);
            } else {
                cleanup();
            }
        }
    }
}

struct PlaybackPipe {
    stdout: std::process::ChildStdout,
    download: Arc<AudioDownload>,
}

impl std::io::Read for PlaybackPipe {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.stdout.read(buffer)
    }
}

impl Drop for PlaybackPipe {
    fn drop(&mut self) {
        self.download.stop();
    }
}

const STDERR_TAIL: usize = 20_000;

fn append_tail(log: &mut Vec<u8>, bytes: &[u8]) {
    let bytes = &bytes[bytes.len().saturating_sub(STDERR_TAIL)..];
    let remove = (log.len() + bytes.len()).saturating_sub(STDERR_TAIL);
    log.drain(..remove);
    log.extend_from_slice(bytes);
}

fn audio_download(
    data: &Data,
    url: &reqwest::Url,
) -> std::io::Result<(Input, Arc<AudioDownload>, Arc<Mutex<Vec<u8>>>)> {
    use std::io::Read;

    let mut child = downloader(data)
        .args([
            "--format", "bestaudio/best", "--no-playlist", "--quiet", "--no-warnings",
            "--no-part", "--output", "-", "--",
        ])
        .arg(url.as_str())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().expect("yt-dlp output is piped");
    let stderr = child.stderr.take().expect("yt-dlp errors are piped");
    let download = Arc::new(AudioDownload(Mutex::new(Some(child))));
    let log = Arc::new(Mutex::new(Vec::new()));
    let tail = log.clone();
    std::thread::spawn(move || {
        let mut stderr = stderr;
        let mut buffer = [0; 4096];
        while let Ok(size @ 1..) = stderr.read(&mut buffer) {
            append_tail(&mut locked(&tail), &buffer[..size]);
        }
    });
    let pipe = PlaybackPipe { stdout, download: download.clone() };
    let input = Input::Live(
        LiveInput::Raw(AudioStream {
            input: Box::new(symphonia::core::io::ReadOnlySource::new(pipe)),
        }),
        None,
    );
    Ok((input, download, log))
}

/// Check actual playout progress, including a decoder stalled on the input pipe.
async fn watch_playback(handle: TrackHandle, end: TrackEnd) {
    let mut progress = Instant::now();
    let mut played = Duration::ZERO;
    while !end.done.load(std::sync::atomic::Ordering::Acquire) {
        tokio::time::sleep(Duration::from_secs(1)).await;
        match tokio::time::timeout(Duration::from_secs(2), handle.get_info()).await {
            Ok(Ok(info)) if matches!(info.playing, PlayMode::Play | PlayMode::Pause) => {
                if info.play_time > played || info.playing == PlayMode::Pause {
                    progress = Instant::now();
                }
                played = info.play_time;
            }
            Ok(Ok(info)) => {
                let error = match info.playing {
                    PlayMode::Errored(error) => Some(error.to_string()),
                    _ => None,
                };
                end.complete(info.play_time, error);
                return;
            }
            Ok(Err(_)) => {
                end.complete(played, Some("источник аудио исчез".into()));
                return;
            }
            Err(_) => {}
        }
        if progress.elapsed() >= Duration::from_secs(45) && end.complete(
            played,
            Some("yt-dlp не передавал звук 45 секунд".into()),
        ) {
            let _ = handle.stop();
            return;
        }
    }
}

/// Обработчик «трек закончился» (или сломался).
#[derive(Clone)]
struct TrackEnd {
    ctx: serenity::Context,
    data: Shared,
    guild: serenity::GuildId,
    channel: serenity::ChannelId,
    title: String,
    log: Arc<Mutex<Vec<u8>>>,
    download: Arc<AudioDownload>,
    done: Arc<std::sync::atomic::AtomicBool>, // End и Error могут прийти оба — обрабатываем один раз
    library: Option<LibraryPlayback>,
    instance: String,
}

#[async_trait::async_trait]
impl songbird::EventHandler for TrackEnd {
    async fn act(&self, event: &EventContext<'_>) -> Option<Event> {
        if let Some(library) = &self.library {
            library.observe(event); // catches a manual skip after 3s but before the next periodic tick
        }
        let (played, error) = match event {
            EventContext::Track(tracks) => {
                tracks.first().map_or((Duration::ZERO, None), |(state, _)| {
                    let error = match &state.playing {
                        PlayMode::Errored(error) => Some(error.to_string()),
                        _ => None,
                    };
                    (state.play_time, error)
                })
            }
            _ => (Duration::ZERO, None),
        };
        self.complete(played, error);
        Some(Event::Cancel)
    }
}

impl TrackEnd {
    fn complete(&self, played: Duration, error: Option<String>) -> bool {
        if self.done.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return false;
        }
        self.download.stop();
        let end = self.clone();
        tokio::spawn(async move {
            let details = String::from_utf8_lossy(&locked(&end.log)).into_owned();
            on_track_end(
                &end.ctx, &end.data, end.guild, end.channel, &end.title, played,
                error, &details, &end.instance,
            ).await;
        });
        true
    }
}

/// Трек закончился (или его пропустили). Если он оборвался в первые секунды не из-за /skip — пишем причину.
#[allow(clippy::too_many_arguments)]
async fn on_track_end(
    ctx: &serenity::Context,
    data: &Shared,
    guild: serenity::GuildId,
    channel: serenity::ChannelId,
    title: &str,
    played: Duration,
    error: Option<String>,
    details: &str,
    instance: &str,
) {
    let mut delay = Duration::from_secs(1);
    let (playing_url, skipped, repeat, failed, disconnected, voice_generation) = loop {
        let lock = data.music.lock_for(guild.get());
        let guard = lock.lock().await;
        let (matches, skipped, repeat, voice_generation) = data.music.with(guild.get(), |state| {
            (state.instance.as_deref() == Some(instance), state.skipped, state.repeat, state.voice_generation)
        });
        if !matches {
            return;
        }
        let disconnected = bot_channel(ctx, guild).await.is_none();
        let failed = !skipped && (error.is_some() || played < TRACK_TOO_SHORT);
        let transition = if disconnected {
            crate::music_runtime::return_current(&data.db, guild.get()).map(|_| ())
        } else {
            crate::music_runtime::finish(&data.db, guild.get(), repeat && !skipped && !failed)
        };
        if let Err(error) = transition {
            eprintln!("Playback retirement: {error}");
            drop(guard);
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(30));
            continue;
        }
        let Some((playing_url, skipped, repeat)) = data.music.with(
            guild.get(), |state| take_playback(state, instance),
        ) else { return };
        crate::music_runtime::queue_changed(data, guild.get());
        break (playing_url, skipped, repeat, failed, disconnected, voice_generation);
    };
    if disconnected {
        return;
    }
    if failed {
        let details = hide_links(details);
        let reason = failure_reason(
            &details,
            error.as_deref().unwrap_or("yt-dlp ничего не написал"),
        );
        println!(
            "Трек «{title}» не проигрался (звука: {:.1} с). yt-dlp:\n{details}",
            played.as_secs_f32()
        );
        let report = format!(
            "{title}: {}\n{}",
            error.as_deref().unwrap_or(""),
            details
                .chars()
                .rev()
                .take(1500)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<String>()
        );
        report_error(&ctx.http, data, Some(guild), "Музыка", &report).await;
        say_in(ctx, channel, &format!("⚠️ **{title}** не проигрался: `{reason}`\nОбычно помогает обновить yt-dlp на сервере: `pip install -U yt-dlp`.")).await;
    }
    if !repeat || skipped || failed {
        if let (Some(url), false) = (&playing_url, failed) {
            if data.db.queue_count(guild.get()).unwrap_or(1) == 0 {
                autoplay_add(ctx, data, guild, channel, url, voice_generation).await;
            }
        }
    }
    play_next(ctx, data, guild, channel).await;
}

/// Включает следующий трек из очереди, если бот в канале и сейчас ничего не играет.
pub async fn play_next(
    ctx: &serenity::Context,
    data: &Shared,
    guild: serenity::GuildId,
    channel: serenity::ChannelId,
) {
    let lock = data.music.lock_for(guild.get());
    let _guard = lock.lock().await; // пока один вызов здесь, второй ждёт
    let Some(manager) = songbird::get(ctx).await else {
        return;
    };
    let Some(call) = manager.get(guild) else {
        return;
    }; // бот не в канале — очередь сохранится
    if call.lock().await.current_channel().is_none()
        || data
            .music
            .with(guild.get(), |state| state.instance.is_some())
        || current_mode(data, guild.get()).await.is_some()
    {
        return; // не в канале или что-то уже играет
    }
    // With no live instance under the playback lock, a current row is an
    // abandoned selection. It can be put back without touching active audio.
    if let Err(error) = crate::music_runtime::return_current(&data.db, guild.get()) {
        eprintln!("Playback claim recovery: {error}");
        return;
    }
    let (item, title, url, details) = loop {
        let item = match crate::music_runtime::claim(&data.db, guild.get()) {
            Ok(Some(item)) => item,
            Ok(None) => return,
            Err(error) => {
                eprintln!("Playback claim: {error}");
                return;
            }
        };
        let prefetched = crate::music_runtime::take_prefetched(data, guild.get(), &item);
        crate::music_runtime::queue_changed(data, guild.get());
        let resolved = match prefetched {
            Some(found) => Ok(found),
            None => crate::music_qol::resolve(data, item.clone()).await,
        };
        match resolved {
            Ok(found) => {
                if let Err(error) = crate::music_runtime::selected(
                    &data.db,
                    guild.get(),
                    &found.2,
                    &found.1,
                    &found.3,
                ) {
                    println!("Playback selection: {error}");
                    return;
                }
                break found;
            }
            Err(error) => {
                if let Some(choice) = error.downcast_ref::<crate::music_qol::Ambiguous>() {
                    // offer owns the atomic durable handoff to music_choices.
                    // Keep the claim recoverable until that handoff succeeds.
                    if let Err(e) =
                        crate::music_qol::offer(ctx, data, guild.get(), channel, choice).await
                    {
                        println!("Предложение версий: {e}");
                        if let Err(error) =
                            crate::music_runtime::return_current(&data.db, guild.get())
                        {
                            println!("Playback choice return: {error}");
                        }
                        crate::music_runtime::queue_changed(data, guild.get());
                        return; // Do not immediately claim and retry this request.
                    }
                    continue;
                }
                if let Err(retire_error) =
                    crate::music_runtime::finish(&data.db, guild.get(), false)
                {
                    println!("Playback resolution retirement: {retire_error}");
                    return;
                }
                crate::diagnostics::record_error(data, guild.get(), "загрузка трека", &error.to_string());
                println!("Не нашла «{}»: {error}", item.title);
                say_in(
                    ctx,
                    channel,
                    &format!(
                        "⚠️ Не получилось загрузить **{}**, пропускаю: {error}",
                        item.title
                    ),
                )
                .await;
            }
        }
    };
    if call.lock().await.current_channel().is_none() {
        if let Err(error) = crate::music_runtime::return_current(&data.db, guild.get()) {
            println!("Playback return: {error}");
        }
        crate::music_runtime::queue_changed(data, guild.get());
        return;
    }
    let Some(target) = provider_url(&url) else {
        if let Err(error) = crate::music_runtime::finish(&data.db, guild.get(), false) {
            eprintln!("Playback invalid target retirement: {error}");
        }
        return;
    };
    let (input, download, log) = match audio_download(data, &target) {
        Ok(stream) => stream,
        Err(error) => {
            if let Err(error) = crate::music_runtime::return_current(&data.db, guild.get()) {
                eprintln!("Playback return: {error}");
            }
            crate::music_runtime::queue_changed(data, guild.get());
            drop(_guard);
            say_in(ctx, channel, &format!("⚠️ Не получилось запустить yt-dlp: {error}")).await;
            return;
        }
    };
    let volume = data.music.with(guild.get(), |state| state.volume.unwrap_or(1.0));
    let mut track = Track::new(input).volume(volume);
    let instance = track.uuid.to_string();
    let snapshot = match crate::music_library::snapshot(&data.db, guild.get(), &url, &title, &details) {
        Ok(snapshot) => Some(snapshot),
        Err(error) => {
            eprintln!("Снимок трека: {error}");
            None
        }
    };
    let library = snapshot.as_ref().map(|snapshot| LibraryPlayback {
        data: data.clone(),
        guild: guild.get(),
        snapshot_id: snapshot.id,
        instance: instance.clone(),
        confirmed: Arc::new(Mutex::new(false)),
    });
    if let Some(library) = &library {
        track.events.add_event(
            EventData::new(Event::Periodic(Duration::from_secs(1), None), library.clone()),
            Duration::ZERO,
        );
    }
    let end = TrackEnd {
        ctx: ctx.clone(),
        data: data.clone(),
        guild,
        channel,
        title: title.clone(),
        log,
        download,
        done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        library,
        instance: instance.clone(),
    };
    for event in [TrackEvent::End, TrackEvent::Error] {
        track.events.add_event(
            EventData::new(Event::Track(event), end.clone()),
            Duration::ZERO,
        );
    }
    {
        let mut call = call.lock().await;
        if call.current_channel().is_none() {
            drop(track);
            if let Err(error) = crate::music_runtime::return_current(&data.db, guild.get()) {
                eprintln!("Playback voice loss: {error}");
            }
            crate::music_runtime::queue_changed(data, guild.get());
            return;
        }
        let handle = call.play(track);
        data.music.with(guild.get(), |state| {
            state.details = details.clone();
            state.instance = Some(instance);
            state.snapshot_id = snapshot.as_ref().map(|snapshot| snapshot.id);
            state.track = Some(handle.clone());
            state.now_playing = Some((item.0, title.clone()));
            state.playing_url = Some(url.clone());
            state.skipped = false;
        });
        tokio::spawn(watch_playback(handle, end));
    }
    crate::music_runtime::queue_changed(data, guild.get());
    drop(_guard);
    let message = serenity::CreateMessage::new()
        .embed(crate::music_qol::card(
            &title,
            &url,
            &details,
            data.db.queue_count(guild.get()).unwrap_or(0),
        ))
        .components({
            let mut rows = music_buttons();
            if let Some(snapshot) = &snapshot {
                rows.push(crate::music_library::favorite_button(
                    guild.get(),
                    snapshot.id,
                ));
            }
            if let Some(link) = crate::music_qol::original_button(&details) {
                rows.push(serenity::CreateActionRow::Buttons(vec![link]));
            }
            rows
        })
        .allowed_mentions(serenity::CreateAllowedMentions::new());
    let _ = channel.send_message(ctx, message).await;
}

/// Очередь кончилась: если автоплей включён и бота кто-то слушает — добавляет в очередь похожий трек.
async fn autoplay_add(
    ctx: &serenity::Context,
    data: &Shared,
    guild: serenity::GuildId,
    channel: serenity::ChannelId,
    url: &str,
    voice_generation: u64,
) {
    if !data.db.autoplay(guild.get()).unwrap_or(false) {
        return;
    }
    let Some(voice) = bot_channel(ctx, guild).await else {
        return;
    };
    let listeners = ctx.cache.guild(guild).is_some_and(|cached| {
        cached.voice_states.values().any(|state| {
            state.channel_id.map(|id| id.get()) == Some(voice.0.get())
                && !cached
                    .members
                    .get(&state.user_id)
                    .is_some_and(|member| member.user.bot)
                && state.user_id != ctx.cache.current_user().id
        })
    });
    if !listeners {
        return; // в канале никого нет — не играем в пустоту
    }
    let generation = data.music.with(guild.get(), |state| state.autoplay_generation);
    let recent = {
        let mut all = locked(&data.music.autoplay_recent);
        let recent = all.entry(guild.get()).or_default();
        recent.push(url.to_string());
        if recent.len() > 40 {
            recent.drain(..recent.len() - 40); // помним последние 40 треков
        }
        recent.clone()
    };
    let fresh: Vec<(String, String)> = similar_tracks(data, url)
        .await
        .into_iter()
        .filter(|(link, _)| !recent.contains(link))
        .collect();
    let Some((link, title)) = fresh[..fresh.len().min(6)]
        .choose(&mut rand::rng())
        .cloned()
    else {
        return;
    };
    let lock = data.music.lock_for(guild.get());
    let guard = lock.lock().await;
    if bot_channel(ctx, guild).await != Some(voice) || !data.music.with(guild.get(), |state| {
        state.voice_generation == voice_generation && state.autoplay_generation == generation
            && state.instance.is_none()
    }) {
        return;
    }
    let added = data.db.lock().execute(
        "INSERT INTO queue(guild_id,url,title,sort_order,details)
         SELECT ?1,?2,?3,1,?4
         WHERE EXISTS (SELECT 1 FROM music_settings WHERE guild_id=?1 AND autoplay=1)
         AND NOT EXISTS (SELECT 1 FROM queue WHERE guild_id=?1)
         AND NOT EXISTS (SELECT 1 FROM music_current WHERE guild_id=?1)",
        params![guild.get() as i64, link, title, serde_json::json!({"request_id": rand::random::<u64>()}).to_string()],
    );
    drop(guard);
    if matches!(added, Ok(1)) {
        crate::music_runtime::queue_changed(data, guild.get());
        say_in(
            ctx,
            channel,
            &format!("📻 Автоплей: дальше **{title}**. Выключить — /autoplay."),
        )
        .await;
    }
}

// ---------- Команды ----------

/// Включить песню по названию или ссылке
#[poise::command(slash_command, guild_only)]
pub async fn play(
    ctx: Context<'_>,
    #[description = "Название песни или ссылка (YouTube, SoundCloud, Spotify)"]
    #[max_length = 300]
    query: String,
) -> Result<(), Error> {
    let (data, guild) = (ctx.data(), ctx.guild_id().unwrap());
    ctx.defer().await?;
    let was_playing = current_mode(data, guild.get()).await.is_some();
    if let Err(error) = join_for_command(ctx, guild).await {
        return whisper(ctx, error.to_string()).await;
    }
    let mut details = serde_json::json!({"requester":ctx.author().id.get(),"original":query});
    let mut query = query;
    if let Some((kind, id)) = crate::spotify::parse_link(&query) {
        let metadata = crate::spotify::metadata(&data.web, kind, &id).await;
        let mut found = None;
        let keys = (
            std::env::var("SPOTIFY_CLIENT_ID").unwrap_or_default(),
            std::env::var("SPOTIFY_CLIENT_SECRET").unwrap_or_default(),
        );
        // Треки и альбомы — через Spotify API (если ключи заданы), плейлисты и всё остальное — со страницы плеера
        if !keys.0.is_empty() && !keys.1.is_empty() && kind != crate::spotify::Kind::Playlist {
            match crate::spotify::api_lookup(&data.web, &keys.0, &keys.1, kind, &id).await {
                Ok(result) => found = Some(result),
                Err(error) => {
                    report_error(
                        ctx.http(),
                        data,
                        Some(guild),
                        "Spotify API /play",
                        &error.to_string(),
                    )
                    .await
                }
            }
        }
        if found.is_none() {
            match crate::spotify::embed_lookup(&data.web, kind, &id).await {
                Ok(result) => found = Some(result),
                Err(error) => {
                    report_error(
                        ctx.http(),
                        data,
                        Some(guild),
                        "Spotify (страница плеера) /play",
                        &error.to_string(),
                    )
                    .await;
                    let what = if kind == crate::spotify::Kind::Playlist {
                        "плейлист приватный или удалён"
                    } else {
                        "ссылка неправильная или трек удалён"
                    };
                    ctx.say(format!(
                        "Не получилось получить данные из Spotify: возможно, {what}."
                    ))
                    .await?;
                    return Ok(());
                }
            }
        }
        let (queries, name) = found.unwrap_or_default();
        if kind != crate::spotify::Kind::Track {
            // Искать треки на YouTube бот будет по одному, перед включением
            for query in &queries {
                let mut meta = metadata
                    .get(query)
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({"original":details["original"]}));
                meta["requester"] = ctx.author().id.get().into();
                meta["spotify_query"] = query.clone().into();
                crate::music_qol::enqueue(
                    data,
                    guild.get(),
                    &format!("ytsearch1:{query}"),
                    query,
                    &meta,
                    false,
                )?;
                crate::music_runtime::queue_changed(data, guild.get());
            }
            let label = if kind == crate::spotify::Kind::Playlist {
                "📜 Плейлист"
            } else {
                "💿 Альбом"
            };
            ctx.say(format!(
                "{label} **{name}**: добавил в очередь треков — {}.",
                queries.len()
            ))
            .await?;
            play_next(ctx.serenity_context(), data, guild, ctx.channel_id()).await;
            return Ok(());
        }
        query = queries.into_iter().next().unwrap_or(query);
        if let Some(meta) = metadata.get(&query) {
            details["expected_duration"] = meta["expected_duration"].clone();
        }
        details["spotify_query"] = query.clone().into(); // один трек ищем как «Исполнитель - Название»
    }
    let explicit_search = !query.trim().starts_with("http://")
        && !query.trim().starts_with("https://")
        && !details["spotify_query"].is_string();
    let candidates = crate::music_qol::candidates(
        data,
        &query,
        if details["spotify_query"].is_string() || explicit_search {
            5
        } else {
            1
        },
    )
    .await?;
    let selected = if details["spotify_query"].is_string() {
        crate::music_qol::choose(
            ctx,
            &query,
            details["expected_duration"].as_f64(),
            candidates,
        )
        .await?
    } else if explicit_search {
        crate::music_qol::choose_explicit(ctx, &query, candidates).await?
    } else {
        candidates.into_iter().next()
    };
    let Some(selected) = selected else {
        return Ok(());
    };
    if !crate::music_qol::mutation_allowed(ctx)? {
        return whisper(
            ctx,
            "Голосовой или музыкальный канал изменился — трек не добавлен.",
        )
        .await;
    }
    let title = selected["title"].as_str().unwrap_or(&query).to_string();
    let url = selected["webpage_url"]
        .as_str()
        .ok_or("У найденного трека нет ссылки")?
        .to_string();
    details["duration"] = selected["duration"].clone();
    details["thumbnail"] = selected["thumbnail"].clone();
    details["resolved"] = true.into();
    let waiting = data.db.queue_count(guild.get())?;
    crate::music_qol::enqueue(data, guild.get(), &url, &title, &details, false)?;
    crate::music_runtime::queue_changed(data, guild.get());
    let text = match (was_playing, waiting) {
        (false, 0) => format!("🔎 Нашёл: **{title}**"),
        (false, _) => format!(
            "➕ Добавил **{title}** в конец очереди. Сначала сыграю сохранённые треки: {waiting}."
        ),
        (true, _) => format!("➕ Добавлено в очередь: **{title}**"),
    };
    ctx.send(
        poise::CreateReply::default()
            .content(text)
            .allowed_mentions(serenity::CreateAllowedMentions::new()),
    )
    .await?;
    play_next(ctx.serenity_context(), data, guild, ctx.channel_id()).await;
    Ok(())
}

/// Пропустить текущую песню
#[poise::command(slash_command, guild_only)]
pub async fn skip(ctx: Context<'_>) -> Result<(), Error> {
    let (data, guild) = (ctx.data(), ctx.guild_id().unwrap().get());
    let expected = data.music.with(guild, |state| state.instance.clone());
    let lock = data.music.lock_for(guild);
    let guard = lock.lock().await;
    if !crate::music_qol::mutation_allowed(ctx)? {
        drop(guard);
        return whisper(ctx, "Зайди в голосовой канал бота и используй музыкальный канал.").await;
    }
    let skipped = stop_current(data, guild, expected.as_deref())?;
    drop(guard);
    if !skipped {
        return whisper(ctx, NOT_PLAYING).await;
    }
    ctx.say("⏭️ Пропущено.").await?;
    Ok(())
}

/// Caller holds the guild playback lock, including the permission check.
fn stop_current(data: &Shared, guild: u64, expected: Option<&str>) -> Result<bool, Error> {
    let Some(expected) = expected else { return Ok(false) };
    let Some(track) = data.music.with(guild, |state| {
        (state.instance.as_deref() == Some(expected)).then(|| state.track.clone()).flatten()
    }) else { return Ok(false) };
    crate::music_runtime::finish(&data.db, guild, false)?;
    data.music.with(guild, |state| state.skipped = true);
    // The watchdog also retires the instance if its event source has disappeared.
    let _ = track.stop();
    crate::music_runtime::queue_changed(data, guild);
    Ok(true)
}

/// Поставить на паузу
#[poise::command(slash_command, guild_only)]
pub async fn pause(ctx: Context<'_>) -> Result<(), Error> {
    let (data, guild) = (ctx.data(), ctx.guild_id().unwrap().get());
    let expected = data.music.with(guild, |state| state.instance.clone());
    let lock = data.music.lock_for(guild);
    let guard = lock.lock().await;
    let mode = current_mode(data, guild).await;
    if !crate::music_qol::mutation_allowed(ctx)? {
        drop(guard);
        return whisper(ctx, "Зайди в голосовой канал бота и используй музыкальный канал.").await;
    }
    let track = data.music.with(guild, |state| {
        (state.instance == expected).then(|| state.track.clone()).flatten()
    });
    match (mode, track) {
        (Some(PlayMode::Play), Some(track)) => track.pause()?,
        _ => {
            drop(guard);
            return whisper(ctx, NOT_PLAYING).await;
        }
    }
    drop(guard);
    ctx.say("⏸️ Пауза.").await?;
    Ok(())
}

/// Продолжить после паузы или запустить сохранённую очередь
#[poise::command(slash_command, guild_only)]
pub async fn resume(ctx: Context<'_>) -> Result<(), Error> {
    let (data, guild) = (ctx.data(), ctx.guild_id().unwrap());
    let expected = data.music.with(guild.get(), |state| state.instance.clone());
    let lock = data.music.lock_for(guild.get());
    let guard = lock.lock().await;
    let mode = current_mode(data, guild.get()).await;
    if mode == Some(PlayMode::Pause) {
        if !crate::music_qol::mutation_allowed(ctx)? {
            drop(guard);
            return whisper(ctx, "Зайди в голосовой канал бота и используй музыкальный канал.").await;
        }
        let track = data.music.with(guild.get(), |state| {
            (state.instance == expected).then(|| state.track.clone()).flatten()
        });
        drop(guard);
        if let Some(track) = track {
            track.play()?;
            ctx.say("▶️ Продолжаю.").await?;
            return Ok(());
        }
        return whisper(ctx, "Трек уже сменился.").await;
    }
    let waiting = data.db.queue_count(guild.get())?
        + usize::from(expected.is_none() && crate::music_runtime::has_current(&data.db, guild.get())?);
    drop(guard);
    if mode.is_some() {
        return whisper(ctx, "Музыка и так играет 🙂").await;
    }
    if waiting == 0 {
        return whisper(ctx, "Очередь пуста — включи что-нибудь через /play.").await;
    }
    ctx.defer().await?;
    if let Err(error) = join_for_command(ctx, guild).await {
        return whisper(ctx, error.to_string()).await;
    }
    ctx.say(format!(
        "▶️ Продолжаю сохранённую очередь: {}.",
        tracks_text(waiting)
    ))
    .await?;
    play_next(ctx.serenity_context(), data, guild, ctx.channel_id()).await;
    Ok(())
}

/// Показать очередь песен
#[poise::command(slash_command, guild_only, rename = "queue")]
pub async fn show_queue(ctx: Context<'_>) -> Result<(), Error> {
    crate::music_queue::show(ctx).await
}

/// Удалить все треки из очереди
#[poise::command(slash_command, guild_only)]
pub async fn remove_all(ctx: Context<'_>) -> Result<(), Error> {
    if !crate::music_qol::same_voice(ctx) {
        return whisper(ctx, "Зайди в голосовой канал бота.").await;
    }
    let (data, guild) = (ctx.data(), ctx.guild_id().unwrap().get());
    let snapshot = crate::music_qol::items(data, guild)?;
    if snapshot.is_empty() {
        return whisper(ctx, "Очередь уже пуста.").await;
    }
    if !crate::qol::confirm(ctx,format!("Очистить очередь? Удалится треков: {}. Новые заявки, добавленные после этого вопроса, останутся.",snapshot.len())).await?{return Ok(())}
    let can_clear_others = ctx.author_member().await.is_some_and(|member| {
        crate::moderation::permissions(ctx.serenity_context(), ctx.guild_id().unwrap(), &member).manage_guild()
    });
    if !crate::music_qol::mutation_allowed(ctx)? {
        return whisper(ctx, "Голосовой или музыкальный канал изменился.").await;
    }
    if !can_clear_others && snapshot.iter().any(|track| track.details["requester"].as_u64() != Some(ctx.author().id.get())) {
        return whisper(ctx, "Для удаления чужих треков нужно право «Управлять сервером». Свои треки можно удалить через /queue_clear_mine.").await;
    }
    let removed = crate::music_qol::remove_snapshot(data, guild, &snapshot)?;
    crate::music_runtime::queue_changed(data, guild);
    let mut text = format!("🗑️ Очередь очищена, удалено треков: {removed}.");
    if current_mode(data, guild).await.is_some() {
        text.push_str("\nТекущий трек доиграет — чтобы выключить его, используй /skip.");
    }
    ctx.say(text).await?;
    Ok(())
}

/// Изменить громкость (от 0 до 200%)
#[poise::command(slash_command, guild_only)]
pub async fn setvolume(
    ctx: Context<'_>,
    #[description = "Громкость в процентах: 100 — обычная, 50 — тише, 150 — громче"]
    #[min = 0]
    #[max = 200]
    level: u32,
) -> Result<(), Error> {
    let lock = ctx.data().music.lock_for(ctx.guild_id().unwrap().get());
    let guard = lock.lock().await;
    if !crate::music_qol::mutation_allowed(ctx)? {
        drop(guard);
        return whisper(ctx, "Зайди в голосовой канал бота и используй музыкальный канал.").await;
    }
    let volume = level.min(200) as f32 / 100.0;
    let track = ctx
        .data()
        .music
        .with(ctx.guild_id().unwrap().get(), |state| {
            state.volume = Some(volume); // для следующих песен
            state.track.clone()
        });
    if let Some(track) = track {
        let _ = track.set_volume(volume); // и для той, что играет
    }
    drop(guard);
    ctx.say(format!("🔊 Громкость: **{}%**", level.min(200)))
        .await?;
    Ok(())
}

/// Выйти из канала (очередь сохранится)
#[poise::command(slash_command, guild_only)]
pub async fn leave(ctx: Context<'_>) -> Result<(), Error> {
    let (data, guild) = (ctx.data(), ctx.guild_id().unwrap());
    let manager = songbird::get(ctx.serenity_context())
        .await
        .ok_or("голосовой модуль не запущен")?;
    if manager.get(guild).is_none() {
        return whisper(ctx, "Я и так не в канале.").await;
    }
    ctx.defer().await?;
    {
        let _operation = data.music.voice_operation(guild.get()).await;
        let lock = data.music.lock_for(guild.get());
        let guard = lock.lock().await;
        if !crate::music_qol::mutation_allowed(ctx)? {
            drop(guard);
            return whisper(ctx, "Зайди в голосовой канал бота и используй музыкальный канал.").await;
        }
        disconnect_saved_locked(ctx.serenity_context(), data, guild).await?;
    }
    let saved = data.db.queue_count(guild.get())?;
    let mut text = "👋 Вышел из канала.".to_string();
    if saved > 0 {
        text.push_str(&format!(
            " Очередь сохранена ({}) — продолжить: /resume",
            tracks_text(saved)
        ));
    }
    ctx.say(text).await?;
    Ok(())
}

/// Автоплей: когда очередь кончилась, бот сам включает похожие треки
#[poise::command(slash_command, guild_only)]
pub async fn autoplay(ctx: Context<'_>) -> Result<(), Error> {
    let (data, guild) = (ctx.data(), ctx.guild_id().unwrap().get());
    let enabled = !data.db.autoplay(guild)?;
    data.db.set_autoplay(guild, enabled)?;
    data.music.with(guild, |state| state.autoplay_generation = state.autoplay_generation.wrapping_add(1));
    ctx.say(if enabled {
        "📻 Автоплей включён: когда очередь закончится, я сам подберу похожий трек (работает для треков с YouTube)."
    } else {
        "📻 Автоплей выключен."
    })
    .await?;
    Ok(())
}

async fn playlist_autocomplete(
    ctx: Context<'_>,
    current: &str,
) -> serenity::CreateAutocompleteResponse {
    if !crate::music_channels::ctx_allowed(ctx).unwrap_or(false) {
        return serenity::CreateAutocompleteResponse::new();
    }
    let names = ctx
        .guild_id()
        .and_then(|guild| ctx.data().db.playlists(guild.get()).ok())
        .unwrap_or_default();
    let current = current.to_lowercase();
    let choices: Vec<serenity::AutocompleteChoice> = names
        .into_iter()
        .filter(|(name, _)| name.to_lowercase().contains(&current))
        .take(25)
        .map(|(name, _)| {
            serenity::AutocompleteChoice::new(name.chars().take(100).collect::<String>(), name)
        })
        .collect();
    serenity::CreateAutocompleteResponse::new().set_choices(choices)
}

/// Сохранить текущий трек и очередь как плейлист сервера
#[poise::command(slash_command, guild_only)]
pub async fn playlist_save(
    ctx: Context<'_>,
    #[description = "Как назвать плейлист"]
    #[max_length = 40]
    name: String,
) -> Result<(), Error> {
    let (data, guild) = (ctx.data(), ctx.guild_id().unwrap());
    let name = name.trim();
    let can_replace = ctx.author_member().await.is_some_and(|member| {
        crate::moderation::permissions(ctx.serenity_context(), guild, &member).manage_guild()
    });
    if !crate::music_channels::ctx_allowed(ctx)? {
        return whisper(ctx, "Используй настроенный музыкальный канал.").await;
    }
    let lock = data.music.lock_for(guild.get());
    let guard = lock.lock().await;
    let mut tracks: Vec<_> = data.music.with(guild.get(), |state| {
        state.now_playing.as_ref().map(|(url, title)| (url.clone(), title.clone(), state.details.clone()))
    }).into_iter().collect();
    let queued = crate::music_qol::queue_slice(data, guild.get(), MAX_PLAYLIST_SAVE - tracks.len(), 0)?;
    tracks.extend(queued.into_iter().map(|item| (item.url, item.title, item.details)));
    if name.is_empty() || tracks.is_empty() {
        drop(guard);
        return whisper(ctx, "Сохранять нечего: укажи название и добавь трек.").await;
    }
    let saved = data.db.save_playlist(guild.get(), name, &tracks, can_replace)?;
    drop(guard);
    match saved {
        SavePlaylist::Saved => {}
        SavePlaylist::Denied => return whisper(ctx, "Заменять плейлист может участник с правом «Управлять сервером».").await,
        SavePlaylist::Limit => return whisper(ctx, format!("На сервере уже {MAX_PLAYLISTS} плейлистов. Удаление — /playlist_delete.")).await,
    }
    ctx.say(format!(
        "💾 Плейлист **{name}** сохранён: {}. Включить — /playlist_play.",
        tracks_text(tracks.len())
    ))
    .await?;
    Ok(())
}

/// Добавить в очередь сохранённый плейлист сервера
#[poise::command(slash_command, guild_only)]
pub async fn playlist_play(
    ctx: Context<'_>,
    #[description = "Какой плейлист включить"]
    #[autocomplete = "playlist_autocomplete"]
    name: String,
) -> Result<(), Error> {
    let (data, guild) = (ctx.data(), ctx.guild_id().unwrap());
    let tracks = data.db.playlist(guild.get(), &name)?;
    if tracks.is_empty() {
        return whisper(ctx, "Такого плейлиста нет — выбери из списка (/playlists).").await;
    }
    ctx.defer().await?;
    if let Err(error) = join_for_command(ctx, guild).await {
        return whisper(ctx, error.to_string()).await;
    }
    let count = crate::music_qol::enqueue_batch(
        data, guild.get(),
        tracks.into_iter().map(|(url, title, mut details)| {
            details["requester"] = ctx.author().id.get().into();
            (url, title, details)
        }),
        false,
    )?;
    crate::music_runtime::queue_changed(data, guild.get());
    ctx.say(format!(
        "📜 Плейлист **{name}**: добавил в очередь {}.",
        tracks_text(count)
    ))
    .await?;
    play_next(ctx.serenity_context(), data, guild, ctx.channel_id()).await;
    Ok(())
}

/// Сохранённые плейлисты сервера
#[poise::command(slash_command, guild_only, rename = "playlists")]
pub async fn show_playlists(ctx: Context<'_>) -> Result<(), Error> {
    let rows = ctx.data().db.playlists(ctx.guild_id().unwrap().get())?;
    if rows.is_empty() {
        return whisper(
            ctx,
            "Плейлистов пока нет. Собери очередь и сохрани её: /playlist_save.",
        )
        .await;
    }
    let lines: Vec<String> = rows
        .iter()
        .map(|(name, count)| format!("• **{name}** — {}", tracks_text(*count as usize)))
        .collect();
    ctx.say(format!("💾 Плейлисты сервера:\n{}", lines.join("\n")))
        .await?;
    Ok(())
}

/// Удалить сохранённый плейлист сервера
#[poise::command(
    slash_command, guild_only,
    default_member_permissions = "MANAGE_GUILD",
    required_permissions = "MANAGE_GUILD"
)]
pub async fn playlist_delete(
    ctx: Context<'_>,
    #[description = "Какой плейлист удалить"]
    #[autocomplete = "playlist_autocomplete"]
    name: String,
) -> Result<(), Error> {
    let removed = ctx
        .data()
        .db
        .delete_playlist(ctx.guild_id().unwrap().get(), &name)?;
    whisper(
        ctx,
        if removed {
            format!("🗑️ Плейлист **{name}** удалён.")
        } else {
            "Такого плейлиста нет.".to_string()
        },
    )
    .await
}

const LYRICS_NOISE: [&str; 10] = [
    "official",
    "video",
    "audio",
    "lyric",
    "клип",
    "премьера",
    "hd",
    "4k",
    "remaster",
    "visualizer",
];

/// Название трека без мусора вроде «(Official Video)» — чтобы поиск текста нашёл песню.
pub fn lyrics_query(title: &str) -> String {
    let chars: Vec<char> = title.chars().collect();
    let mut result = String::new();
    let mut index = 0;
    while index < chars.len() {
        if matches!(chars[index], '(' | '[') {
            if let Some(length) = chars[index + 1..]
                .iter()
                .position(|c| matches!(c, ')' | ']'))
            {
                let inside: String = chars[index + 1..index + 1 + length]
                    .iter()
                    .collect::<String>()
                    .to_lowercase();
                if LYRICS_NOISE.iter().any(|noise| inside.contains(noise)) {
                    result.push(' ');
                    index += length + 2;
                    continue;
                }
            }
        }
        result.push(chars[index]);
        index += 1;
    }
    result
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches([' ', '-', '|'])
        .to_string()
}

/// Найти текст песни (той, что играет, или по названию)
#[poise::command(slash_command, guild_only)]
pub async fn lyrics(
    ctx: Context<'_>,
    #[description = "Название песни (если не указать — та, что сейчас играет)"]
    #[max_length = 150]
    song: Option<String>,
) -> Result<(), Error> {
    let playing = ctx
        .data()
        .music
        .with(ctx.guild_id().unwrap().get(), |state| {
            state.now_playing.clone().map(|(_, title)| title)
        });
    let Some(title) = song.or(playing) else {
        return whisper(
            ctx,
            "Сейчас ничего не играет — напиши название песни в поле song.",
        )
        .await;
    };
    let cleaned = lyrics_query(&title);
    let query = if cleaned.is_empty() { title } else { cleaned };
    // Сами тексты бот в чат не пишет (они защищены авторским правом) — даёт кнопку на страницу с текстом
    let link =
        reqwest::Url::parse_with_params("https://genius.com/search", [("q", query.as_str())])?;
    let button = serenity::CreateButton::new_link(link.as_str()).label("Открыть текст на Genius");
    let reply = poise::CreateReply::default()
        .content(format!("🎤 Текст песни **{query}**:"))
        .components(vec![serenity::CreateActionRow::Buttons(vec![button])])
        .ephemeral(true);
    ctx.send(reply).await?;
    Ok(())
}

/// Топ-10 самых прослушиваемых треков за этот месяц
#[poise::command(slash_command, guild_only)]
pub async fn top_chart_month(ctx: Context<'_>) -> Result<(), Error> {
    let (data, guild) = (ctx.data(), ctx.guild_id().unwrap());
    let today = chrono::Local::now().date_naive();
    let rows = data
        .db
        .top_tracks(guild.get(), &today.format("%Y-%m").to_string())?;
    if rows.is_empty() {
        ctx.say("В этом месяце ещё ничего не слушали — включай /play!")
            .await?;
        return Ok(());
    }
    ctx.defer().await?;
    use chrono::Datelike;
    let month_title = format!("{} {}", MONTH_NAMES[today.month0() as usize], today.year());
    let most = rows[0].1.max(1); // больше всего прослушиваний у первого — от него длина полосок
    let rows: Vec<Row> = rows
        .into_iter()
        .map(|(title, plays)| Row {
            name: title,
            picture: Picture::Music,
            progress: plays as f64 / most as f64,
            pill: format!("{plays} {}", plural(plays, "раз", "раза", "раз")),
            hint: "прослушано".to_string(),
        })
        .collect();
    let server = guild.name(ctx).unwrap_or_default();
    let png = crate::levels::render_table(
        data,
        "ТОП ТРЕКОВ МЕСЯЦА",
        format!("{month_title} · {server}"),
        "по прослушиваниям",
        rows,
        "chart_bg",
    )
    .await;
    crate::levels::send_png(ctx, png, "top_chart.png").await
}

// ---------- Кнопки под «Играет» ----------

/// Нажали кнопку плеера. Возвращает false, если это не кнопка музыки.
pub async fn button_pressed(
    ctx: &serenity::Context,
    data: &Shared,
    press: &serenity::ComponentInteraction,
) -> bool {
    let Some(action) = press.data.custom_id.strip_prefix("music:") else {
        return false;
    };
    let Some(guild) = press.guild_id else {
        return true;
    };
    // Подтверждаем нажатие до ожидания состояния голосового плеера.
    if let Err(error) = press
        .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
        .await
    {
        println!(
            "Кнопка музыки {action}: {}",
            crate::interactions::sanitize_error(&error.to_string())
        );
        return true;
    }
    let answer = |text: String, private: bool| {
        let message = serenity::CreateInteractionResponseFollowup::new()
            .content(text)
            .ephemeral(private)
            .allowed_mentions(serenity::CreateAllowedMentions::new());
        press.create_followup(ctx, message)
    };
    let lock = data.music.lock_for(guild.get());
    let guard = lock.lock().await;
    let mode = current_mode(data, guild.get()).await;
    let (track, instance) = data.music.with(guild.get(), |state| (state.track.clone(), state.instance.clone()));
    if !crate::music_qol::user_in_bot_voice(ctx, guild, press.user.id)
        || !crate::music_channels::press_allowed(data, press).unwrap_or(false)
    {
        drop(guard);
        let _ = answer("Зайди в голосовой канал бота и используй музыкальный канал.".into(), true).await;
        return true;
    }
    let who = format!("<@{}>", press.user.id);
    let (text, private) = match (action, mode, track) {
        ("pause", Some(PlayMode::Pause), Some(track)) => {
            let _ = track.play();
            (format!("▶️ {who} продолжает музыку."), false)
        }
        ("pause", Some(PlayMode::Play), Some(track)) => {
            let _ = track.pause();
            (format!("⏸️ {who} ставит на паузу."), false)
        }
        ("skip", Some(_), Some(_)) => {
            match stop_current(data, guild.get(), instance.as_deref()) {
                Ok(true) => (format!("⏭️ {who} пропускает трек."), false),
                Ok(false) => (NOT_PLAYING.to_string(), true),
                Err(error) => (format!("Не получилось пропустить трек: {error}"), true),
            }
        }
        ("repeat", _, _) => {
            let repeat = data.music.with(guild.get(), |state| {
                state.repeat = !state.repeat;
                state.repeat
            });
            if repeat {
                (format!("🔁 {who} включает повтор: трек будет играть снова, пока не нажмут ⏭️ или 🔁."), false)
            } else {
                (format!("➡️ {who} выключает повтор."), false)
            }
        }
        ("shuffle", _, _) => match data
            .db
            .queue_shuffle(guild.get())
            .inspect(|_| crate::music_runtime::queue_changed(data, guild.get()))
        {
            Ok(count) if count >= 2 => (
                format!("🔀 {who} перемешивает очередь ({}).", tracks_text(count)),
                false,
            ),
            _ => ("В очереди нечего перемешивать.".to_string(), true),
        },
        _ => (NOT_PLAYING.to_string(), true),
    };
    drop(guard);
    let _ = answer(text, private).await;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_or_duplicate_end_does_not_take_new_playback() {
        let mut state = GuildMusic::default();
        state.instance = Some("new".into());
        state.now_playing = Some(("url".into(), "New".into()));
        assert!(take_playback(&mut state, "old").is_none());
        assert!(state.now_playing.is_some());
        assert!(take_playback(&mut state, "new").is_some());
        assert!(take_playback(&mut state, "new").is_none());
    }

    #[tokio::test]
    async fn concurrent_voice_operations_recheck_destination_before_queueing() {
        let music = MusicState::default();
        let destination = Mutex::new(None);
        let queued = Mutex::new(Vec::new());
        let attempt = |requested| {
            let (music, destination, queued) = (&music, &destination, &queued);
            async move {
                let _operation = music.voice_operation(1).await;
                let needs_join = voice_destination(*locked(&destination), requested)?;
                // Model the await in Songbird join: a second entry point must not
                // inspect the disconnected state until this operation enqueues.
                tokio::task::yield_now().await;
                if needs_join {
                    *locked(&destination) = Some(requested);
                }
                locked(&queued).push(requested);
                Ok::<_, String>(())
            }
        };
        let (first, second) = tokio::join!(attempt(10), attempt(20));
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        assert_eq!(
            locked(&queued).as_slice(),
            &[(*locked(&destination)).unwrap()]
        );
        // Guilds and play_next have independent locks.
        let _operation = music.voice_operation(1).await;
        let _other_guild = tokio::time::timeout(Duration::from_secs(1), music.voice_operation(2))
            .await
            .unwrap();
        assert!(music.lock_for(1).try_lock().is_ok());
    }

    #[test]
    fn joining_accepts_same_channel_and_refuses_another_channel() {
        assert!(voice_destination(None, 10).unwrap());
        assert!(!voice_destination(Some(10), 10).unwrap());
        assert!(voice_destination(Some(20), 10).is_err());
    }

    /// Проверка без Discord: yt-dlp качает настоящий трек, а бот распознаёт формат звука.
    /// Нужны интернет и yt-dlp, поэтому тест запускается только вручную: cargo test -- --ignored
    #[tokio::test]
    #[ignore]
    async fn decodes_ytdlp_stream() {
        let program = std::env::var("YTDLP").unwrap_or_else(|_| "yt-dlp".to_string());
        let mut parts = program.split_whitespace();
        let child = std::process::Command::new(parts.next().unwrap())
            .args(parts)
            .args([
                "--format",
                "bestaudio/best",
                "--no-playlist",
                "--quiet",
                "--no-warnings",
                "--no-part",
                "--output",
                "-",
            ])
            .arg("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("yt-dlp не запустился");
        let input: Input = ChildContainer::from(child).into();
        let playable = input
            .make_playable_async(
                songbird::input::codecs::get_codec_registry(),
                songbird::input::codecs::get_probe(),
            )
            .await;
        assert!(playable.is_ok(), "звук не распознан: {:?}", playable.err());
        assert!(playable.unwrap().is_playable());
    }

    /// Поиск и автоплей через настоящий yt-dlp (нужен интернет): cargo test -- --ignored
    #[tokio::test]
    #[ignore]
    async fn real_search_and_mix() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let data = Data::new(
            db::Database::in_memory().unwrap(),
            &crate::config::Settings::default(),
            dir,
            false,
        );
        let (title, url) = search(&data, "rick astley never gonna give you up")
            .await
            .unwrap();
        println!("{title} — {url}");
        assert!(youtube_id(&url).is_some(), "{url}");
        let mix = similar_tracks(&data, &url).await;
        println!("микс: {}", mix.len());
        assert!(!mix.is_empty());
    }

    #[test]
    fn helpers() {
        assert_eq!(ytdlp_target("  dark days ", 1).unwrap(), "ytsearch1:dark days");
        assert_eq!(ytdlp_target("https://youtu.be/abc", 1).unwrap(), "https://youtu.be/abc");
        assert_eq!(ytdlp_target("ytsearch1:A - B", 1).unwrap(), "ytsearch1:A - B");
        assert_eq!(
            youtube_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=5").as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(
            youtube_id("https://youtu.be/dQw4w9WgXcQ").as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(
            youtube_id("https://youtube.com/shorts/abcdefghijk").as_deref(),
            Some("abcdefghijk")
        );
        assert_eq!(youtube_id("https://soundcloud.com/x/y"), None);
        let mix = parse_mix(
            "dQw4w9WgXcQ\nСама песня\nAAAAAAAAAAA\nДругая\nBBBBBBBBBBB\nNA\n",
            "dQw4w9WgXcQ",
        );
        assert_eq!(
            mix,
            [
                (
                    "https://www.youtube.com/watch?v=AAAAAAAAAAA".to_string(),
                    "Другая".to_string()
                ),
                (
                    "https://www.youtube.com/watch?v=BBBBBBBBBBB".to_string(),
                    "Без названия".to_string()
                )
            ]
        );
        assert_eq!(
            lyrics_query(
                "Rick Astley - Never Gonna Give You Up (Official Music Video) [4K Remaster]"
            ),
            "Rick Astley - Never Gonna Give You Up"
        );
        assert_eq!(
            lyrics_query("Песня (feat. Друг) | Премьера клипа"),
            "Песня (feat. Друг) | Премьера клипа"
        );
        assert_eq!(
            lyrics_query("Кино - Группа крови [Клип]"),
            "Кино - Группа крови"
        );
        assert_eq!(
            failure_reason("[youtube] x\nERROR: Video unavailable\nпотом что-то", "?"),
            "ERROR: Video unavailable"
        );
        assert_eq!(failure_reason("", "ничего"), "ничего");
        assert_eq!(
            hide_links("ERROR: https://googlevideo.com/abc?x=1 403"),
            "ERROR: <ссылка> 403"
        );
    }

    #[test]
    fn queue_rows() {
        let db = db::Database::in_memory().unwrap();
        db.queue_add(1, "u1", "Первый").unwrap();
        db.queue_add(1, "u2", "Второй").unwrap();
        db.queue_add(2, "x", "Чужой").unwrap();
        db.queue_push_front(1, "u0", "Нулевой").unwrap();
        assert_eq!(db.queue_count(1).unwrap(), 3);
        assert_eq!(
            db.queue_pop(1).unwrap(),
            Some(("u0".into(), "Нулевой".into()))
        );
        assert_eq!(
            db.queue_tracks(1, 15)
                .unwrap()
                .iter()
                .map(|(_, t)| t.as_str())
                .collect::<Vec<_>>(),
            ["Первый", "Второй"]
        );
        assert_eq!(db.queue_shuffle(1).unwrap(), 2);
        assert_eq!(db.queue_clear(1).unwrap(), 2);
        assert_eq!(db.queue_pop(1).unwrap(), None);
        assert_eq!(db.queue_count(2).unwrap(), 1);

        db.record_play(1, "2026-10", "Старое").unwrap();
        db.record_play(1, "2026-11", "Хит").unwrap();
        db.record_play(1, "2026-11", "Хит").unwrap();
        db.record_play(1, "2026-11", "Другое").unwrap();
        assert_eq!(
            db.top_tracks(1, "2026-11").unwrap(),
            [("Хит".to_string(), 2), ("Другое".to_string(), 1)]
        );
        assert!(db.top_tracks(1, "2026-10").unwrap().is_empty()); // прошлый месяц стёрт

        assert!(!db.autoplay(1).unwrap());
        db.set_autoplay(1, true).unwrap();
        assert!(db.autoplay(1).unwrap());

        let tracks = vec![
            ("a".to_string(), "A".to_string(), serde_json::json!({"original":"a"})),
            ("b".to_string(), "B".to_string(), serde_json::json!({"original":"b","duration":120})),
        ];
        db.save_playlist(1, "вечер", &tracks, false).unwrap();
        assert!(matches!(db.save_playlist(1, "вечер", &tracks[..1], false).unwrap(), SavePlaylist::Denied));
        db.save_playlist(1, "вечер", &tracks[..1], true).unwrap();
        db.save_playlist(1, "утро", &tracks, false).unwrap();
        assert_eq!(
            db.playlists(1).unwrap(),
            [("вечер".to_string(), 1), ("утро".to_string(), 2)]
        );
        assert_eq!(db.playlist(1, "утро").unwrap(), tracks);
        assert!(db.delete_playlist(1, "утро").unwrap() && !db.delete_playlist(1, "утро").unwrap());
    }
}

/// Caller holds BOTH voice_operation and lock_for, acquired in that order.
/// Idle disconnect must recheck the empty voice channel under these guards.
/// This helper acquires neither lock, so it cannot reverse their order.
pub(crate) async fn disconnect_saved_locked(
    ctx: &serenity::Context,
    data: &Shared,
    guild: serenity::GuildId,
) -> Result<(), Error> {
    let manager = songbird::get(ctx)
        .await
        .ok_or("голосовой модуль не запущен")?;
    crate::music_runtime::return_current(&data.db, guild.get())?;
    let track = data.music.with(guild.get(), |state| {
        state.instance = None;
        state.skipped = true;
        state.now_playing = None;
        state.playing_url = None;
        state.snapshot_id = None;
        state.voice_generation = state.voice_generation.wrapping_add(1);
        state.track.take()
    });
    if let Some(track) = track {
        let _ = track.stop();
    }
    crate::music_runtime::queue_changed(data, guild.get());
    manager.remove(guild).await?;
    Ok(())
}
