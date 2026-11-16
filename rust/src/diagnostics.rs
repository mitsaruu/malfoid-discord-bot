//! Private, guild-scoped diagnostics. Integration: register `status()`, defer
//! `/status` privately before checks/work, and exempt it from channel restrictions.
//! `Data` owns `started_at` and a default `DiagnosticsState`; the central
//! error reporter calls `record_error` before attempting to send logs.

use crate::{Context, Data, Error};
use chrono::{DateTime, NaiveDateTime, Utc};
use poise::serenity_prelude as s;
use rusqlite::{Connection, OpenFlags};
use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    io::Read,
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

const STAMP: &str = "%Y%m%dT%H%M%S%.9fZ";
// A timeout cannot cancel spawn_blocking. Keep its permit inside the worker so
// repeated commands cannot accumulate SQLite scans after response timeouts.
static WORKER: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// At most one sanitized failure per guild, with oldest entries evicted at 256.
/// Raw context/messages are classified before storage and never retained.
#[derive(Default)]
pub struct DiagnosticsState {
    errors: Mutex<VecDeque<(u64, LastFailure)>>,
}

#[derive(Clone)]
struct LastFailure {
    at: DateTime<Utc>,
    context: &'static str,
    summary: &'static str,
}

impl DiagnosticsState {
    fn record(&self, guild: u64, context: &str, message: &str) {
        if guild == 0 {
            return;
        }
        // Bound classification work as well as retained memory. Never copy an
        // untrusted excerpt to Discord, even when no familiar secret is found.
        let context: String = context
            .chars()
            .take(4096)
            .collect::<String>()
            .to_lowercase();
        let message: String = message
            .chars()
            .take(4096)
            .collect::<String>()
            .to_lowercase();
        let context = if [
            "music",
            "музык",
            "трек",
            "voice",
            "голос",
            "/play",
            "ffmpeg",
            "yt-dlp",
        ]
        .iter()
        .any(|word| context.contains(word))
        {
            "Музыка"
        } else {
            "Команда или действие"
        };
        let summary = if context == "Музыка"
            && (message.contains("403") || message.contains("forbidden"))
        {
            "источник аудио отказал в доступе; попробуй другую версию трека"
        } else if [
            "forbidden",
            "permission",
            "missing access",
            "403",
            "нет прав",
            "недостаточно прав",
        ]
        .iter()
        .any(|word| message.contains(word))
        {
            "не хватает прав; проверь права бота в канале"
        } else if ["timeout", "timed out", "время ожидания", "тайм-аут"]
            .iter()
            .any(|word| message.contains(word))
        {
            "истекло время ожидания ответа; попробуй ещё раз"
        } else if ["rate limit", "429", "too many requests"]
            .iter()
            .any(|word| message.contains(word))
        {
            "слишком много запросов; подожди немного"
        } else if ["connection", "network", "dns", "connect", "соединени"]
            .iter()
            .any(|word| message.contains(word))
        {
            "не удалось связаться с сервисом; попробуй позже"
        } else {
            "не удалось выполнить действие; попробуй снова или обратись к администратору"
        };
        let failure = LastFailure {
            at: Utc::now(),
            context,
            summary,
        };
        let mut errors = self
            .errors
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        errors.retain(|(id, _)| *id != guild);
        if errors.len() >= 256 {
            errors.pop_front();
        }
        errors.push_back((guild, failure));
    }

    fn last_error(&self, guild: u64) -> Option<LastFailure> {
        self.errors
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .find(|(id, _)| *id == guild)
            .map(|(_, failure)| failure.clone())
    }
}

pub fn record_error(data: &Data, guild: u64, context: &str, message: &str) {
    data.diagnostics.record(guild, context, message);
}

fn format_last_error(error: Option<&LastFailure>) -> String {
    match error {
        Some(error) => format!(
            "Последняя ошибка на этом сервере ({} МСК): {} — {}.",
            error
                .at
                .with_timezone(&chrono_tz::Europe::Moscow)
                .format("%d.%m %H:%M"),
            error.context,
            error.summary
        ),
        None => "Последняя ошибка на этом сервере: не зафиксирована.".into(),
    }
}

#[derive(Default)]
struct MusicSnapshot {
    enabled: bool,
    connected: Option<bool>,
    title: Option<String>,
    queue: Option<usize>,
    autoplay: Option<bool>,
}

#[derive(Clone, Copy)]
enum BackupSnapshot {
    Restricted,
    Unavailable,
    Missing,
    Valid(DateTime<Utc>),
}

/// Состояние бота, задержка Discord и музыка этого сервера (видно только тебе).
#[poise::command(slash_command, guild_only, ephemeral)]
pub async fn status(ctx: Context<'_>) -> Result<(), Error> {
    let Some(guild) = ctx.guild_id() else {
        return Ok(());
    };
    // Read the actual gateway heartbeat measurement; do not turn None into a
    // misleading zero, or make an outbound HTTP request just to estimate ping.
    let latency = tokio::time::timeout(Duration::from_millis(250), async {
        ctx.framework()
            .shard_manager
            .runners
            .lock()
            .await
            .get(&ctx.serenity_context().shard_id)
            .and_then(|runner| runner.latency)
    })
    .await
    .ok()
    .flatten();
    let system_details = match ctx {
        poise::Context::Application(app) => app
            .interaction
            .member
            .as_ref()
            .and_then(|member| member.permissions)
            .is_some_and(|permissions| permissions.manage_guild() || permissions.administrator()),
        _ => false,
    };
    #[cfg(feature = "voice")]
    let mut music = {
        let connected = ctx.guild().map(|cached| {
            cached
                .voice_states
                .get(&ctx.framework().bot_id())
                .is_some_and(|state| state.channel_id.is_some())
        });
        MusicSnapshot {
            enabled: true,
            connected,
            title: ctx.data().music.with(guild.get(), |state| {
                state
                    .now_playing
                    .as_ref()
                    .map(|(_, title)| safe_title(title))
            }),
            ..Default::default()
        }
    };
    #[cfg(not(feature = "voice"))]
    let music = MusicSnapshot::default();
    #[cfg(not(feature = "voice"))]
    let _ = guild;

    let mut backup = if system_details {
        BackupSnapshot::Unavailable
    } else {
        BackupSnapshot::Restricted
    };
    if let Ok(permit) = WORKER.try_acquire() {
        let data = ctx.data().clone();
        let worker = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            #[cfg(feature = "voice")]
            let queue = (
                data.db.queue_count(guild.get()).ok(),
                data.db.autoplay(guild.get()).ok(),
            );
            #[cfg(not(feature = "voice"))]
            let queue: (Option<usize>, Option<bool>) = (None, None);
            let backup = if system_details {
                last_valid_backup(&data.dir).unwrap_or(BackupSnapshot::Unavailable)
            } else {
                BackupSnapshot::Restricted
            };
            (queue, backup)
        });
        if let Ok(Ok((queue, checked_backup))) =
            tokio::time::timeout(Duration::from_secs(5), worker).await
        {
            backup = checked_backup;
            #[cfg(feature = "voice")]
            {
                music.queue = queue.0;
                music.autoplay = queue.1;
            }
            #[cfg(not(feature = "voice"))]
            let _ = queue;
        }
    }
    let content = format_status(
        latency,
        ctx.data().started_at.elapsed(),
        ctx.data().miniapp_ready,
        &music,
        backup,
        ctx.data().diagnostics.last_error(guild.get()).as_ref(),
    );
    ctx.send(
        poise::CreateReply::default()
            .content(content)
            .ephemeral(true)
            .allowed_mentions(s::CreateAllowedMentions::new()),
    )
    .await?;
    Ok(())
}

fn format_status(
    latency: Option<Duration>,
    uptime: Duration,
    miniapp: bool,
    music: &MusicSnapshot,
    backup: BackupSnapshot,
    last_error: Option<&LastFailure>,
) -> String {
    let ping = latency.map_or_else(
        || "нет замера".into(),
        |value| format!("{} мс", value.as_millis()),
    );
    let seconds = uptime.as_secs();
    let mut text = format!("**Состояние бота**\nЗадержка Discord: {ping}\nВремя работы: {} д {} ч {} мин\nМини-приложение: {} (доступность не проверялась)\n",
        seconds / 86400, seconds / 3600 % 24, seconds / 60 % 60,
        if miniapp { "настроено" } else { "не настроено" });
    if music.enabled {
        let connected = match music.connected {
            Some(true) => "подключён",
            Some(false) => "не подключён",
            None => "нет данных",
        };
        let queue = music
            .queue
            .map_or_else(|| "нет данных".into(), |count| count.to_string());
        let autoplay = match music.autoplay {
            Some(true) => "включён",
            Some(false) => "выключен",
            None => "нет данных",
        };
        text.push_str(&format!("\n**Музыка этого сервера**\nГолосовой канал: {connected}\nСейчас играет: {}\nВ очереди: {queue}\nАвтоплей: {autoplay}\n",
            music.title.as_deref().map(safe_title).unwrap_or_else(|| "ничего".into())));
    } else {
        text.push_str("\nМузыка: выключена в этой сборке\n");
    }
    match backup {
        BackupSnapshot::Restricted => {
            text.push_str("\nРезервная копия: сведения доступны с правом «Управлять сервером»\n")
        }
        BackupSnapshot::Unavailable => {
            text.push_str("\nРезервная копия: проверка недоступна или занята; повтори позже\n")
        }
        BackupSnapshot::Missing => {
            text.push_str("\nРезервная копия: исправных автоматических копий не найдено\n")
        }
        BackupSnapshot::Valid(at) => text.push_str(&format!(
            "\nПоследняя проверенная резервная копия бота: {} МСК\n",
            at.with_timezone(&chrono_tz::Europe::Moscow)
                .format("%Y-%m-%d %H:%M:%S")
        )),
    }
    text.push('\n');
    text.push_str(&format_last_error(last_error));
    text
}

// Titles are untrusted metadata. Never render links, mentions, terminal control
// codes, credential assignments, or token-shaped strings in diagnostics.
fn safe_title(title: &str) -> String {
    let lower = title.to_lowercase();
    if [
        "token",
        "secret",
        "password",
        "authorization",
        "api_key",
        "apikey",
        "bearer",
        "private key",
        "http:",
        "https:",
        "www.",
        ":\\",
        "file:",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        || title
            .split(|c: char| !c.is_ascii_alphanumeric() && !matches!(c, '_' | '-' | '.'))
            .any(|word| word.len() >= 24 && word.bytes().any(|b| b.is_ascii_digit()))
        || title.contains(['/', '\\'])
    {
        return "название скрыто".into();
    }
    let mut result: String = title
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(*c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .map(|c| {
            if matches!(c, '@' | '<' | '>' | '`' | '*' | '_' | '~' | '|' | '[' | ']') {
                ' '
            } else {
                c
            }
        })
        .take(120)
        .collect();
    if result.trim().is_empty() {
        result = "без названия".into();
    }
    result
}

fn ordinary(path: &Path, directory: bool) -> bool {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if !directory && meta.nlink() != 1 {
            return false;
        }
    }
    !meta.file_type().is_symlink()
        && if directory {
            meta.is_dir()
        } else {
            meta.is_file()
        }
}

fn plain_directory(path: &Path) -> bool {
    path.ancestors()
        .filter(|p| !p.as_os_str().is_empty())
        .all(|p| ordinary(p, true))
}

fn allowed_file(relative: &str) -> bool {
    if matches!(relative, "levels.db" | "patchnotes.txt") {
        return true;
    }
    let Some((dir, name)) = relative.split_once('/') else {
        return false;
    };
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\', ':']) {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    if lower.split(['.', '-', '_']).any(|part| {
        matches!(
            part,
            "secret"
                | "secrets"
                | "token"
                | "tokens"
                | "key"
                | "keys"
                | "apikey"
                | "credential"
                | "credentials"
                | "password"
                | "passwords"
                | "env"
                | "log"
                | "logs"
        )
    }) {
        return false;
    }
    let extension = Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    match dir {
        "config" => matches!(extension, "ini" | "txt"),
        "miniapp" => matches!(extension, "html" | "js" | "css"),
        _ => false,
    }
}

/// Read only the private automatic-backup area; return no filenames or errors
/// to Discord. Invalid/newer incomplete copies never hide an older valid one.
fn last_valid_backup(root: &Path) -> Result<BackupSnapshot, Error> {
    let auto = root.join("backups/auto");
    match fs::symlink_metadata(&auto) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BackupSnapshot::Missing)
        }
        Err(error) => return Err(error.into()),
        Ok(_) if !plain_directory(&auto) => return Ok(BackupSnapshot::Unavailable),
        _ => (),
    }
    let deadline = Instant::now() + Duration::from_secs(4);
    let now = Utc::now();
    let mut candidates = Vec::new();
    for (index, entry) in fs::read_dir(auto)?.enumerate() {
        if index >= 4096 || Instant::now() >= deadline {
            return Ok(BackupSnapshot::Unavailable);
        }
        let entry = entry?;
        let path = entry.path();
        if let Some(created) = valid_manifest(&path, now) {
            candidates.push((created, path));
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    for (created, path) in candidates {
        if Instant::now() >= deadline {
            return Ok(BackupSnapshot::Unavailable);
        }
        // Read-only connection: never open the live database or create a file.
        let checked = (|| -> rusqlite::Result<bool> {
            let connection = Connection::open_with_flags(
                path.join("levels.db"),
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            connection.busy_timeout(Duration::from_millis(100))?;
            let results = connection
                .prepare("PRAGMA quick_check(1)")?
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(results == ["ok"])
        })();
        if matches!(checked, Ok(true)) {
            return Ok(BackupSnapshot::Valid(created));
        }
    }
    Ok(BackupSnapshot::Missing)
}

fn valid_manifest(path: &Path, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if !plain_directory(path) {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    let (stamp, nonce) = name.strip_prefix("musicbot-v1-")?.rsplit_once('-')?;
    if nonce.len() != 16 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let created = NaiveDateTime::parse_from_str(stamp, STAMP).ok()?.and_utc();
    if created > now || created.format(STAMP).to_string() != stamp {
        return None;
    }
    let manifest_path = path.join("manifest.json");
    if !ordinary(&manifest_path, false) {
        return None;
    }
    let mut bytes = Vec::new();
    fs::File::open(manifest_path)
        .ok()?
        .take(128 * 1024 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 128 * 1024 {
        return None;
    }
    let manifest: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    if manifest["format"] != "musicbot-auto-v1"
        || manifest["directory"] != name
        || manifest["quick_check"] != "ok"
        || manifest["created_utc"]
            .as_str()?
            .parse::<DateTime<Utc>>()
            .ok()?
            != created
        || manifest["version"].as_str()?.is_empty()
        || manifest["version"].as_str()?.len() > 128
    {
        return None;
    }
    let files = manifest["files"].as_array()?;
    if files.len() > 513 {
        return None;
    }
    let mut expected = BTreeSet::new();
    for file in files {
        let relative = file.as_str()?;
        if !allowed_file(relative)
            || !expected.insert(relative.to_owned())
            || !ordinary(&path.join(relative), false)
        {
            return None;
        }
    }
    if !expected.contains("levels.db") {
        return None;
    }
    let mut actual = BTreeSet::new();
    for (index, entry) in fs::read_dir(path).ok()?.enumerate() {
        if index >= 517 {
            return None;
        }
        let entry = entry.ok()?;
        let name = entry.file_name().into_string().ok()?;
        if matches!(name.as_str(), "config" | "miniapp") && ordinary(&entry.path(), true) {
            if !expected
                .iter()
                .any(|file| file.starts_with(&format!("{name}/")))
            {
                return None;
            }
            for (index, child) in fs::read_dir(entry.path()).ok()?.enumerate() {
                if index >= 512 {
                    return None;
                }
                let child = child.ok()?;
                if !ordinary(&child.path(), false) {
                    return None;
                }
                actual.insert(format!("{name}/{}", child.file_name().to_str()?));
            }
        } else {
            if !ordinary(&entry.path(), false) {
                return None;
            }
            if name != "manifest.json" {
                actual.insert(name);
            }
        }
    }
    (expected == actual).then_some(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_guild_scoped_bounded_and_never_retain_raw_details() {
        let state = DiagnosticsState::default();
        assert!(state.last_error(1).is_none());
        state.record(1, "/play C:\\private\\token", "HTTP 403 token=supersecret");
        assert!(state.last_error(2).is_none());
        let error = state.last_error(1).unwrap();
        let text = format_last_error(Some(&error));
        assert!(text.contains("источник аудио"));
        for raw in ["private", "token", "supersecret", "403", "C:\\"] {
            assert!(!text.contains(raw));
        }
        state.record(1, "command", "request timed out");
        assert!(format_last_error(state.last_error(1).as_ref()).contains("время"));
        for guild in 2..=257 {
            state.record(guild, "command", &"я".repeat(10000));
        }
        assert!(state.last_error(1).is_none());
        assert!(state.last_error(257).is_some());
        assert!(format_last_error(state.last_error(257).as_ref()).len() < 500);

        let other = state.last_error(257).unwrap();
        let text = format_status(
            None,
            Duration::ZERO,
            false,
            &MusicSnapshot::default(),
            BackupSnapshot::Restricted,
            Some(&other),
        );
        assert!(!text.contains("не хватает прав"));
        assert!(!text.contains("UTC\n"));
        assert!(text.chars().count() < 2000);
    }

    #[test]
    fn private_status_bounds_unicode_and_hides_sensitive_metadata() {
        for secret in [
            "https://example.test/?token=secret",
            "C:\\private\\levels.db",
            "/home/bot/.env",
            "Authorization: Bearer xyz",
            "MTAxMjM0NTY3ODkwMTIzNDU2Nzg5.secret",
        ] {
            let music = MusicSnapshot {
                enabled: true,
                title: Some(secret.into()),
                ..Default::default()
            };
            let text = format_status(
                None,
                Duration::ZERO,
                true,
                &music,
                BackupSnapshot::Restricted,
                None,
            );
            assert!(!text.contains(secret));
            assert!(!text.contains("2026-"));
            assert!(text.contains("не зафиксирована"));
            assert!(text.contains("Задержка Discord: нет замера"));
        }
        assert_eq!(safe_title("Песня 🎵"), "Песня 🎵");
        let title = safe_title(&"я🎵".repeat(1000));
        assert_eq!(title.chars().count(), 120);
        assert!(!safe_title("@everyone\n**песня**\u{202e}").contains('@'));
        let text = format_status(
            Some(Duration::from_millis(42)),
            Duration::from_secs(90061),
            false,
            &MusicSnapshot::default(),
            BackupSnapshot::Missing,
            None,
        );
        assert!(text.contains("42 мс") && text.contains("1 д 1 ч 1 мин"));
        assert!(text.contains("выключена в этой сборке") && text.contains("не найдено"));
        assert!(text.chars().count() < 2000);
    }

    #[test]
    fn missing_damaged_future_and_corrupt_backups_are_ignored() {
        let root = std::env::temp_dir().join(format!(
            "musicbot-diagnostics-{:016x}",
            rand::random::<u64>()
        ));
        fs::create_dir(&root).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        assert!(matches!(
            last_valid_backup(&root).unwrap(),
            BackupSnapshot::Missing
        ));
        let at: DateTime<Utc> = "2025-01-01T04:00:00Z".parse().unwrap();
        let name = format!("musicbot-v1-{}-0000000000000001", at.format(STAMP));
        let path = root.join("backups/auto").join(&name);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("manifest.json"), "broken").unwrap();
        assert!(matches!(
            last_valid_backup(&root).unwrap(),
            BackupSnapshot::Missing
        ));
        let manifest = serde_json::json!({"format":"musicbot-auto-v1", "directory":name, "created_utc":at.to_rfc3339(), "version":"test", "quick_check":"ok", "files":["levels.db"]});
        fs::write(path.join("manifest.json"), manifest.to_string()).unwrap();
        fs::write(path.join("levels.db"), "not sqlite").unwrap();
        assert!(matches!(
            last_valid_backup(&root).unwrap(),
            BackupSnapshot::Missing
        ));
        fs::remove_file(path.join("levels.db")).unwrap();
        Connection::open(path.join("levels.db"))
            .unwrap()
            .execute_batch("CREATE TABLE test(value INTEGER); INSERT INTO test VALUES(1);")
            .unwrap();
        assert!(
            matches!(last_valid_backup(&root).unwrap(), BackupSnapshot::Valid(value) if value == at)
        );
        assert!(valid_manifest(&path, at - chrono::Duration::seconds(1)).is_none());
        let mut damaged = manifest.clone();
        damaged["format"] = "foreign".into();
        fs::write(path.join("manifest.json"), damaged.to_string()).unwrap();
        assert!(matches!(
            last_valid_backup(&root).unwrap(),
            BackupSnapshot::Missing
        ));
    }
}
