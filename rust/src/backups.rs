//! Local daily backups. No restore, upload, or access to environment secrets.
//!
//! `[backups] time = 04:00` is a Moscow wall-clock time; `keep = 7` counts
//! completed, validated backups. Only this module's versioned format is pruned.
//! Unix syncs directory entries before pruning. Windows retains file flushes
//! and atomic rename, with best-effort directory crash durability.

use crate::{config::Settings, db::Database, Error, Shared};
use chrono::{DateTime, Days, NaiveDateTime, NaiveTime, SecondsFormat, TimeZone, Utc};
use chrono_tz::Europe::Moscow;
use rusqlite::{
    backup::{Backup, StepResult},
    Connection, OpenFlags,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const FORMAT: &str = "musicbot-auto-v1";
const PREFIX: &str = "musicbot-v1-";
const STAMP: &str = "%Y%m%dT%H%M%S%.9fZ";
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_FILES: usize = 512;
const MAX_ENTRIES: usize = 4096;
const MAX_MANIFEST_BYTES: u64 = 128 * 1024;
const RETRY: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone)]
pub struct BackupSettings {
    time: NaiveTime,
    keep: usize,
}

impl BackupSettings {
    pub fn load(settings: &Settings) -> Self {
        Self {
            time: NaiveTime::parse_from_str(&settings.text("backups", "time", "04:00"), "%H:%M")
                .unwrap_or_else(|_| NaiveTime::from_hms_opt(4, 0, 0).unwrap()),
            keep: settings.int("backups", "keep", 7).clamp(1, 365) as usize,
        }
    }
}

fn invalid(message: &str) -> Error {
    io::Error::new(io::ErrorKind::InvalidData, message).into()
}

fn preceding_checkpoint(now: DateTime<Utc>, settings: &BackupSettings) -> DateTime<Utc> {
    let local = now.with_timezone(&Moscow);
    let mut day = local.date_naive();
    if local.time() < settings.time {
        day = day
            .checked_sub_days(Days::new(1))
            .expect("supported calendar date");
    }
    Moscow
        .from_local_datetime(&day.and_time(settings.time))
        .single()
        .expect("unambiguous Moscow schedule")
        .with_timezone(&Utc)
}

fn next_deadline(now: DateTime<Utc>, settings: &BackupSettings) -> DateTime<Utc> {
    let day = preceding_checkpoint(now, settings)
        .with_timezone(&Moscow)
        .date_naive()
        .checked_add_days(Days::new(1))
        .expect("supported calendar date");
    Moscow
        .from_local_datetime(&day.and_time(settings.time))
        .single()
        .expect("unambiguous Moscow schedule")
        .with_timezone(&Utc)
}

fn wake_after(
    checked_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    success: bool,
    settings: &BackupSettings,
) -> DateTime<Utc> {
    if success {
        next_deadline(checked_at, settings)
    } else {
        finished_at + chrono::Duration::seconds(RETRY.as_secs() as i64)
    }
}

/// Spawn once during startup, using `data.db` and `data.dir`. Blocking SQLite
/// and file work runs off the Tokio executor. An error never advances freshness.
pub fn start_loop(data: Shared, settings: BackupSettings) {
    tokio::spawn(async move {
        loop {
            let shared = data.clone();
            let options = settings.clone();
            let result = tokio::task::spawn_blocking(move || -> Result<DateTime<Utc>, Error> {
                let now = Utc::now();
                if backup_due(&shared.dir, now, &options)? {
                    snapshot(&shared.db, &shared.dir, now, options.keep)?;
                }
                Ok(now)
            })
            .await;
            let finished_at = Utc::now();
            let (success, checked_at) = match result {
                Ok(Ok(checked_at)) => (true, checked_at),
                Ok(Err(error)) => {
                    eprintln!("Automatic backup failed: {error}");
                    (false, finished_at)
                }
                Err(error) => {
                    eprintln!("Automatic backup worker failed: {error}");
                    (false, finished_at)
                }
            };
            // Base success on the time freshness was checked. If file copying
            // crossed the daily checkpoint, the next iteration is due now.
            let deadline = wake_after(checked_at, finished_at, success, &settings);
            // Recompute against wall time in bounded sleeps, so clock changes
            // do not make a daily interval drift or miss its checkpoint.
            loop {
                let remaining = deadline.signed_duration_since(Utc::now());
                if remaining <= chrono::Duration::zero() {
                    break;
                }
                tokio::time::sleep(remaining.to_std().unwrap().min(Duration::from_secs(60))).await;
            }
        }
    });
}

fn linked(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0 // includes directory junctions
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn plain_directory(path: &Path) -> Result<(), Error> {
    // Check ancestors too: a normal-looking auto/ below a linked backups/
    // must not redirect either copying or retention outside the requested root.
    for ancestor in path.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        let metadata = fs::symlink_metadata(ancestor)?;
        if linked(&metadata) || !metadata.is_dir() {
            return Err(invalid("backup path contains a link or non-directory"));
        }
    }
    Ok(())
}

fn private_directory(path: &Path) -> Result<(), Error> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => (),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    plain_directory(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn private_file(path: &Path) -> Result<File, Error> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

fn read_plain(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, Error> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if linked(&metadata) || !metadata.is_file() {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Ok(None);
        }
    }
    if metadata.len() > limit {
        return Err(invalid("backup text file exceeds size limit"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Linux O_NOFOLLOW | O_NONBLOCK: refuse swapped symlinks/FIFOs.
        options.custom_flags(0o400000 | 0o4000);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path)?;
    let opened = file.metadata()?;
    if linked(&opened) || !opened.is_file() {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.nlink() != 1 || metadata.dev() != opened.dev() || metadata.ino() != opened.ino() {
            return Err(invalid("backup source changed while opening"));
        }
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("backup text file grew beyond size limit"));
    }
    Ok(Some(bytes))
}

fn allowed_file(relative: &str) -> bool {
    if relative == "levels.db" || relative == "patchnotes.txt" {
        return true;
    }
    let Some((dir, name)) = relative.split_once('/') else {
        return false;
    };
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\', ':']) {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    if lower.split(['.', '-', '_']).any(|s| {
        matches!(
            s,
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
        "config" => matches!(extension, "ini" | "txt") && lower != "env.ini" && lower != "env.txt",
        "miniapp" => matches!(extension, "html" | "js" | "css"),
        _ => false,
    }
}

fn credential_literal(value: &str) -> bool {
    let value = value.trim();
    let Some(quote @ ('\'' | '"' | '`')) = value.chars().next() else {
        return false;
    };
    let mut escaped = false;
    for (end, character) in value.char_indices().skip(1) {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == quote {
            let literal = &value[1..end];
            if literal.trim().is_empty() || (quote == '`' && literal.contains("${")) {
                return false;
            }
            // A closing quote must end the value, rather than merely its
            // first operand: "Bearer " + session is runtime source code.
            let tail = value[end + 1..]
                .trim()
                .trim_start_matches([';', ',', '}', ')', ']'])
                .trim_start();
            return tail.is_empty() || tail.starts_with("//") || tail.starts_with("/*");
        }
    }
    false
}

fn ordinary_text(bytes: &[u8], configuration: bool) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return false;
    }
    // Skip whole credential-bearing files; never redact into a misleading copy.
    !text.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        if lower.contains("-----begin ") && lower.contains("private key") {
            return true;
        }
        let Some((name, value)) = lower.split_once('=').or_else(|| lower.split_once(':')) else {
            return false;
        };
        let name = name.trim().trim_matches(['\'', '"']);
        // OAuth source code referring to runtime tokens is ordinary JS. A
        // configuration assignment or a literal credential is excluded.
        if !configuration && !credential_literal(value) {
            return false;
        }
        let compact: String = name.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        [
            "token",
            "secret",
            "password",
            "credential",
            "credentials",
            "apikey",
            "privatekey",
            "authorization",
        ]
        .iter()
        .any(|suffix| compact.ends_with(suffix))
    })
}

fn copy_text(root: &Path, staging: &Path) -> Result<Vec<String>, Error> {
    let mut candidates = vec!["patchnotes.txt".to_owned()];
    for dir in ["config", "miniapp"] {
        let source = root.join(dir);
        let metadata = match fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if linked(&metadata) || !metadata.is_dir() {
            continue;
        }
        for (index, entry) in fs::read_dir(&source)?.enumerate() {
            if index >= MAX_ENTRIES {
                return Err(invalid("too many backup source entries"));
            }
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let relative = format!("{dir}/{name}");
            if allowed_file(&relative) {
                if candidates.len() >= MAX_FILES {
                    return Err(invalid("too many backup source files"));
                }
                candidates.push(relative);
            }
        }
    }
    candidates.sort();
    let mut files = vec!["levels.db".to_owned()];
    let mut total = 0;
    for relative in candidates {
        let Some(bytes) = read_plain(&root.join(&relative), MAX_FILE_BYTES)? else {
            continue;
        };
        if !ordinary_text(&bytes, relative.starts_with("config/")) {
            continue;
        }
        total += bytes.len();
        if total > MAX_TOTAL_BYTES {
            return Err(invalid("backup text exceeds total size limit"));
        }
        let destination = staging.join(&relative);
        private_directory(destination.parent().unwrap())?;
        let mut file = private_file(&destination)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        files.push(relative);
    }
    Ok(files)
}

struct Staging(PathBuf, bool);

impl Drop for Staging {
    fn drop(&mut self) {
        // Only our freshly-created staging directory, at a fixed depth. Never
        // use recursive deletion, follow links, or sweep another worker's temp.
        if !self.1 {
            if let Err(error) = remove_staging(&self.0) {
                eprintln!("Incomplete automatic backup cleanup failed: {error}");
            }
        }
    }
}

fn remove_staging(path: &Path) -> Result<(), Error> {
    plain_directory(path)?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() && !linked(&metadata) {
            if !matches!(entry.file_name().to_str(), Some("config" | "miniapp")) {
                continue;
            }
            for child in fs::read_dir(entry.path())? {
                let child = child?;
                if !fs::symlink_metadata(child.path())?.is_dir() {
                    fs::remove_file(child.path())?;
                }
            }
            fs::remove_dir(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    fs::remove_dir(path)?;
    Ok(())
}

fn with_source<T>(
    db: &Database,
    operation: impl FnOnce(&Connection) -> Result<T, Error>,
) -> Result<T, Error> {
    let path = {
        let source = db.lock();
        source
            .path()
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
    };
    if let Some(path) = path {
        // Release the shared mutex before opening or stepping the backup.
        // SQLite coordinates this independent reader with the live WAL writer.
        let source = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        source.busy_timeout(Duration::from_millis(100))?;
        operation(&source)
    } else {
        // Private in-memory databases cannot be reopened from a filename.
        operation(&db.lock())
    }
}

fn source_revision(root: &Path) -> Option<String> {
    use std::process::{Command, Stdio};
    // Best effort, local only. No shell, remote, working-tree scan, or token
    // access. Bound runtime and output; deployed directories may lack Git.
    let mut child = Command::new("git")
        .args(["--no-optional-locks", "-C"])
        .arg(root)
        .args(["rev-parse", "--verify", "HEAD"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut output = String::new();
                child
                    .stdout
                    .take()?
                    .take(129)
                    .read_to_string(&mut output)
                    .ok()?;
                let revision = output.trim();
                return ((revision.len() == 40 || revision.len() == 64)
                    && revision.bytes().all(|b| b.is_ascii_hexdigit()))
                .then(|| revision.to_owned());
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Consistent online SQLite snapshot, checked before atomic publication.
/// `root` is the bot's source directory; output is `root/backups/auto/...`.
/// Copy failures leave previous completed backups intact. Retention failures
/// retain extra copies and are logged, since publication already succeeded.
/// Unix directory-sync failures abort publication and skip retention; Windows
/// has file flushes and atomic rename but no portable directory-fsync guarantee.
pub fn snapshot(
    db: &Database,
    root: &Path,
    at: DateTime<Utc>,
    keep: usize,
) -> Result<PathBuf, Error> {
    snapshot_with_sync(db, root, at, keep, sync_directory)
}

// Unix directory fsync makes publication crash-durable. Windows keeps atomic
// rename and file flushing, but std cannot provide a portable directory flush;
// directory crash durability is therefore best effort on that platform.
fn sync_directory(path: &Path) -> Result<(), Error> {
    plain_directory(path)?;
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    Ok(())
}

fn snapshot_with_sync(
    db: &Database,
    root: &Path,
    at: DateTime<Utc>,
    keep: usize,
    mut sync_dir: impl FnMut(&Path) -> Result<(), Error>,
) -> Result<PathBuf, Error> {
    plain_directory(root)?;
    let parent = root.join("backups");
    private_directory(&parent)?;
    sync_dir(root)?; // make a newly created backups/ entry durable
    let auto = parent.join("auto");
    private_directory(&auto)?;
    sync_dir(&parent)?; // make a newly created auto/ entry durable
    let name = format!(
        "{PREFIX}{}-{:016x}",
        at.format(STAMP),
        rand::random::<u64>()
    );
    let temporary = auto.join(format!(".tmp-{name}"));
    // Unlike reusable parent directories, staging must be exclusively ours.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&temporary)?;
    let mut staging = Staging(temporary, false);
    let database_path = staging.0.join("levels.db");
    drop(private_file(&database_path)?);
    with_source(db, |source| {
        let mut destination = Connection::open(&database_path)?;
        {
            let backup = Backup::new(source, &mut destination)?;
            let timeout = Instant::now() + Duration::from_secs(60);
            loop {
                if matches!(backup.step(256)?, StepResult::Done) {
                    break;
                }
                if Instant::now() >= timeout {
                    return Err(invalid("SQLite backup timed out"));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        // A WAL source can transfer its journal mode. Finalize the destination
        // as a standalone database, without relying on any sidecar file.
        destination.execute_batch("PRAGMA journal_mode=DELETE;")?;
        let checks: Vec<String> = destination
            .prepare("PRAGMA quick_check")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        if checks != ["ok"] {
            return Err(invalid("SQLite backup quick_check failed"));
        }
        destination.close().map_err(|(_, error)| error)?;
        Ok(())
    })?;
    OpenOptions::new()
        .write(true)
        .open(&database_path)?
        .sync_all()?;
    let files = copy_text(root, &staging.0)?;
    for dir in ["config", "miniapp"] {
        if files
            .iter()
            .any(|file| file.starts_with(&format!("{dir}/")))
        {
            sync_dir(&staging.0.join(dir))?;
        }
    }
    let manifest = json!({
        "format": FORMAT, "directory": name,
        "created_utc": at.to_rfc3339_opts(SecondsFormat::Nanos, true),
        "version": env!("CARGO_PKG_VERSION"), "source_revision": source_revision(root),
        "quick_check": "ok", "files": files,
    });
    let mut file = private_file(&staging.0.join("manifest.json"))?;
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(invalid("backup manifest exceeds size limit"));
    }
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    sync_dir(&staging.0)?;
    let completed = auto.join(&name);
    if fs::symlink_metadata(&completed).is_ok() {
        return Err(invalid("backup name collision"));
    }
    fs::rename(&staging.0, &completed)?;
    // Until the parent directory is synced this is still our incomplete copy.
    // On failure the guard removes it so it cannot satisfy freshness checks.
    staging.0 = completed.clone();
    sync_dir(&auto)?;
    // Disarm cleanup after successful publication (without leaking the path).
    staging.1 = true;
    drop(staging);
    if let Err(error) = prune(&auto, keep.max(1), &completed) {
        eprintln!("Automatic backup published; retention postponed: {error}");
    }
    Ok(completed)
}

fn owned_manifest(path: &Path) -> Option<(DateTime<Utc>, Vec<String>)> {
    (|| -> Result<_, Error> {
        plain_directory(path)?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| invalid("invalid backup name"))?;
        let (stamp, nonce) = name
            .strip_prefix(PREFIX)
            .and_then(|s| s.rsplit_once('-'))
            .ok_or_else(|| invalid("foreign backup"))?;
        if nonce.len() != 16 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid("invalid backup nonce"));
        }
        let created = NaiveDateTime::parse_from_str(stamp, STAMP)?.and_utc();
        if created.format(STAMP).to_string() != stamp {
            return Err(invalid("invalid backup timestamp"));
        }
        let bytes = read_plain(&path.join("manifest.json"), MAX_MANIFEST_BYTES)?
            .ok_or_else(|| invalid("missing manifest"))?;
        let manifest: Value = serde_json::from_slice(&bytes)?;
        if manifest["format"] != FORMAT
            || manifest["directory"] != name
            || manifest["quick_check"] != "ok"
            || manifest["created_utc"]
                .as_str()
                .and_then(|s| s.parse::<DateTime<Utc>>().ok())
                != Some(created)
            || manifest["version"]
                .as_str()
                .is_none_or(|s| s.is_empty() || s.len() > 128)
        {
            return Err(invalid("invalid backup manifest"));
        }
        let list = manifest["files"]
            .as_array()
            .ok_or_else(|| invalid("invalid backup file list"))?;
        if list.len() > MAX_FILES + 1 {
            return Err(invalid("oversized backup manifest"));
        }
        let mut expected = BTreeSet::new();
        for value in list {
            let relative = value
                .as_str()
                .ok_or_else(|| invalid("invalid manifest path"))?;
            if !allowed_file(relative) || !expected.insert(relative.to_owned()) {
                return Err(invalid("unsafe manifest path"));
            }
        }
        if !expected.contains("levels.db") {
            return Err(invalid("missing backup database"));
        }
        let mut actual = BTreeSet::new();
        for (index, entry) in fs::read_dir(path)?.enumerate() {
            if index > MAX_FILES + 3 {
                return Err(invalid("unexpected backup entries"));
            }
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid("invalid backup filename"))?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if linked(&metadata) {
                return Err(invalid("linked backup entry"));
            }
            if metadata.is_dir() && matches!(name.as_str(), "config" | "miniapp") {
                if !expected
                    .iter()
                    .any(|file| file.starts_with(&format!("{name}/")))
                {
                    return Err(invalid("unlisted backup directory"));
                }
                for (index, child) in fs::read_dir(entry.path())?.enumerate() {
                    if index >= MAX_FILES {
                        return Err(invalid("unexpected backup entries"));
                    }
                    let child = child?;
                    let metadata = fs::symlink_metadata(child.path())?;
                    if linked(&metadata) || !metadata.is_file() {
                        return Err(invalid("non-file backup entry"));
                    }
                    let child_name = child
                        .file_name()
                        .into_string()
                        .map_err(|_| invalid("invalid backup filename"))?;
                    actual.insert(format!("{name}/{child_name}"));
                }
            } else if metadata.is_file() {
                if name != "manifest.json" {
                    actual.insert(name);
                }
            } else {
                return Err(invalid("unexpected backup directory"));
            }
        }
        if actual != expected {
            return Err(invalid("backup contents disagree with manifest"));
        }
        Ok((created, expected.into_iter().collect()))
    })()
    .ok()
}

pub(crate) fn check_database(directory: &Path) -> rusqlite::Result<bool> {
    let connection = Connection::open_with_flags(
        directory.join("levels.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_millis(100))?;
    let mut statement = connection.prepare("PRAGMA quick_check(1)")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()? == ["ok"])
}

fn completed_backups(auto: &Path) -> Result<Vec<(DateTime<Utc>, PathBuf)>, Error> {
    match fs::symlink_metadata(auto) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
        Ok(_) => plain_directory(auto)?,
    }
    let mut completed = Vec::new();
    for entry in fs::read_dir(auto)? {
        let entry = entry?;
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|s| s.starts_with(PREFIX))
        {
            continue;
        }
        if let Some((created, _)) = owned_manifest(&entry.path()) {
            completed.push((created, entry.path()));
        }
    }
    completed.sort();
    Ok(completed)
}

fn backup_due(root: &Path, now: DateTime<Utc>, settings: &BackupSettings) -> Result<bool, Error> {
    let checkpoint = preceding_checkpoint(now, settings);
    Ok(!completed_backups(&root.join("backups/auto"))?
        .iter()
        .any(|(created, path)| *created >= checkpoint && *created <= now
            && matches!(check_database(path), Ok(true))))
}

fn prune(auto: &Path, keep: usize, published: &Path) -> Result<(), Error> {
    if owned_manifest(published).is_none() || !matches!(check_database(published), Ok(true)) {
        return Ok(());
    }
    let completed: Vec<_> = completed_backups(auto)?.into_iter()
        .filter(|(_, path)| matches!(check_database(path), Ok(true))).collect();
    let excess = completed.len().saturating_sub(keep);
    for (_, path) in completed
        .into_iter()
        .filter(|(_, path)| path != published)
        .take(excess)
    {
        let Some((_, files)) = owned_manifest(&path) else {
            continue;
        };
        // Delete only listed ordinary files and the two known, now-empty dirs.
        // Never recursively delete an arbitrary tree, even in a versioned dir.
        for relative in files {
            fs::remove_file(path.join(relative))?;
        }
        for dir in ["config", "miniapp"] {
            match fs::remove_dir(path.join(dir)) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
        fs::remove_file(path.join("manifest.json"))?;
        fs::remove_dir(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "musicbot-backup-test-{:016x}",
                rand::random::<u64>()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn database(&self) -> Database {
            Database::open(&self.0.join("levels.db")).unwrap()
        }

        fn put(&self, name: &str, bytes: impl AsRef<[u8]>) {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        fn auto(&self) -> PathBuf {
            self.0.join("backups/auto")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn at(value: &str) -> DateTime<Utc> {
        value.parse().unwrap()
    }

    fn defaults() -> BackupSettings {
        BackupSettings::load(&Settings::default())
    }


    #[test]
    fn damaged_copy_does_not_satisfy_freshness_or_displace_restore_point() {
        let fixture = Fixture::new();
        let db = fixture.database();
        let before = snapshot(&db, &fixture.0, at("2026-11-15T01:01:00Z"), 7).unwrap();
        let damaged = snapshot(&db, &fixture.0, at("2026-11-16T01:02:00Z"), 7).unwrap();
        fs::write(damaged.join("levels.db"), "damaged after publication").unwrap();
        assert!(backup_due(&fixture.0, at("2026-11-16T01:03:00Z"), &defaults()).unwrap());
        prune(&fixture.auto(), 1, &damaged).unwrap();
        assert!(before.exists());
    }

    // A raw copy of levels.db loses the committed WAL row; copying an open
    // transaction also incorrectly includes the second, uncommitted row.
    #[test]
    fn snapshot_contains_committed_wal_and_is_standalone() {
        let fixture = Fixture::new();
        let db = fixture.database();
        db.lock().execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE probe(value TEXT);").unwrap();
        let writer = Connection::open(fixture.0.join("levels.db")).unwrap();
        writer.execute_batch("INSERT INTO probe VALUES('committed'); BEGIN IMMEDIATE; INSERT INTO probe VALUES('uncommitted');").unwrap();
        assert!(fs::metadata(fixture.0.join("levels.db-wal")).unwrap().len() > 0);

        let completed = snapshot(&db, &fixture.0, at("2026-11-16T01:00:00Z"), 7).unwrap();
        writer.execute_batch("ROLLBACK").unwrap();
        let copy = Connection::open(completed.join("levels.db")).unwrap();
        let rows: Vec<String> = copy
            .prepare("SELECT value FROM probe")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows, ["committed"]);
        assert_eq!(
            copy.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        assert!(!completed.join("levels.db-wal").exists());
        let manifest: Value =
            serde_json::from_slice(&fs::read(completed.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["quick_check"], "ok");
        assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
    }

    // Removing the extension, secret-name, or text checks would copy one of
    // these disallowed files into a completed backup.
    #[test]
    fn copies_only_bounded_allowlisted_text() {
        let fixture = Fixture::new();
        let db = fixture.database();
        for name in [
            "config/settings.ini",
            "config/words.txt",
            "miniapp/index.html",
            "miniapp/app.js",
            "miniapp/style.css",
            "patchnotes.txt",
        ] {
            fixture.put(name, "ordinary text\n");
        }
        for name in [
            ".env",
            "config/.env",
            "config/.hidden.txt",
            "config/keys.ini",
            "config/token.txt",
            "config/credentials.txt",
            "config/log.txt",
            "config/run.log",
            "config/program.exe",
            "miniapp/private-key.js",
            "miniapp/app.js.map",
            "config/nested/words.txt",
        ] {
            fixture.put(name, "excluded\n");
        }
        fixture.put("config/binary.txt", [0, 1, 2, 3]);
        fixture.put("config/auth.ini", "DISCORD_TOKEN = synthetic-test-value\n");
        fixture.put("config/settings.ini", "[ai]\nmax_output_tokens=500\n");
        fixture.put("miniapp/app.js", "const token = response.access_token;\nconst headers = {token: response.access_token};\n");
        let completed = snapshot(&db, &fixture.0, at("2026-11-16T01:00:00Z"), 7).unwrap();
        assert_eq!(
            fs::read_to_string(completed.join("config/settings.ini")).unwrap(),
            "[ai]\nmax_output_tokens=500\n"
        );
        assert!(completed.join("miniapp/app.js").is_file());
        assert!(completed.join("miniapp/style.css").is_file());
        assert!(completed.join("miniapp/index.html").is_file());
        assert!(completed.join("config/words.txt").is_file());
        assert!(completed.join("patchnotes.txt").is_file());
        for name in [
            ".env",
            "config/.env",
            "config/.hidden.txt",
            "config/keys.ini",
            "config/token.txt",
            "config/credentials.txt",
            "config/log.txt",
            "config/run.log",
            "config/program.exe",
            "miniapp/private-key.js",
            "miniapp/app.js.map",
            "config/nested",
            "config/binary.txt",
            "config/auth.ini",
        ] {
            assert!(!completed.join(name).exists(), "copied {name}");
        }
    }

    #[test]
    fn copies_shipped_javascript_with_runtime_authorization() {
        let fixture = Fixture::new();
        let db = fixture.database();
        let shipped = include_str!("../../miniapp/app.js");
        fixture.put("miniapp/app.js", shipped);
        fixture.put(
            "miniapp/hardcoded.js",
            "options.headers.Authorization = \"Bearer synthetic-test-secret\";\n",
        );
        fixture.put(
            "miniapp/templates.js",
            "options.headers.Authorization = `Bearer ${session}`;\n",
        );
        let completed = snapshot(&db, &fixture.0, at("2026-11-16T01:00:00Z"), 7).unwrap();
        assert_eq!(
            fs::read_to_string(completed.join("miniapp/app.js")).unwrap(),
            shipped
        );
        assert!(completed.join("miniapp/templates.js").is_file());
        assert!(!completed.join("miniapp/hardcoded.js").exists());
    }

    #[test]
    fn directory_sync_failure_preserves_previous_copies_and_freshness() {
        for fail_at in ["root", "backups", "config", "miniapp", "staging", "auto"] {
            let fixture = Fixture::new();
            let db = fixture.database();
            let first = snapshot(&db, &fixture.0, at("2026-11-14T01:00:00Z"), 7).unwrap();
            let second = snapshot(&db, &fixture.0, at("2026-11-15T01:00:00Z"), 7).unwrap();
            fixture.put("config/settings.ini", "ordinary\n");
            fixture.put("miniapp/app.js", "ordinary\n");
            let mut injected = false;
            let result =
                snapshot_with_sync(&db, &fixture.0, at("2026-11-16T01:00:00Z"), 1, |path| {
                    assert!(first.join("levels.db").is_file(), "pruned before sync");
                    assert!(second.join("levels.db").is_file(), "pruned before sync");
                    let name = path.file_name().unwrap().to_str().unwrap();
                    if (fail_at == "root" && path == fixture.0)
                        || name == fail_at
                        || (fail_at == "staging" && name.starts_with(".tmp-"))
                    {
                        injected = true;
                        return Err(io::Error::new(
                            io::ErrorKind::Other,
                            "injected directory sync failure",
                        )
                        .into());
                    }
                    sync_directory(path)
                });
            assert!(injected, "did not sync {fail_at}");
            assert!(result.is_err());
            assert!(first.join("levels.db").is_file());
            assert!(second.join("levels.db").is_file());
            assert_eq!(fs::read_dir(fixture.auto()).unwrap().count(), 2);
            assert!(backup_due(&fixture.0, at("2026-11-16T01:00:01Z"), &defaults()).unwrap());
        }
    }

    #[test]
    fn syncs_child_directories_then_staging_then_parent_before_pruning() {
        let fixture = Fixture::new();
        let db = fixture.database();
        let previous = snapshot(&db, &fixture.0, at("2026-11-15T01:00:00Z"), 7).unwrap();
        fixture.put("config/settings.ini", "ordinary\n");
        fixture.put("miniapp/app.js", "ordinary\n");
        let mut synced = Vec::new();
        let completed =
            snapshot_with_sync(&db, &fixture.0, at("2026-11-16T01:00:00Z"), 1, |path| {
                assert!(previous.join("levels.db").is_file(), "pruned before sync");
                let name = path.file_name().unwrap().to_str().unwrap();
                synced.push(if path == fixture.0 {
                    "root".to_owned()
                } else if name.starts_with(".tmp-") {
                    "staging".to_owned()
                } else {
                    name.to_owned()
                });
                sync_directory(path)
            })
            .unwrap();
        assert_eq!(
            synced,
            ["root", "backups", "config", "miniapp", "staging", "auto"]
        );
        assert!(completed.join("levels.db").is_file());
        assert!(!previous.exists());
        assert!(!backup_due(&fixture.0, at("2026-11-16T01:00:01Z"), &defaults()).unwrap());
    }

    // Pruning by name alone would delete the forged directory or its contents.
    #[test]
    fn retention_keeps_seven_and_leaves_unmanaged_or_tampered_trees() {
        let fixture = Fixture::new();
        let db = fixture.database();
        let first = snapshot(&db, &fixture.0, at("2026-11-01T01:00:00Z"), 7).unwrap();
        let forged = fixture
            .auto()
            .join("musicbot-v1-20261001T010000.000000000Z-0000000000000000");
        fs::create_dir(&forged).unwrap();
        fs::copy(first.join("manifest.json"), forged.join("manifest.json")).unwrap();
        fs::copy(first.join("levels.db"), forged.join("levels.db")).unwrap();
        fixture.put("backups/auto/manual/keep.txt", "unmanaged");
        let mut copies = vec![first];
        for day in 2..=9 {
            copies.push(
                snapshot(
                    &db,
                    &fixture.0,
                    at(&format!("2026-11-{day:02}T01:00:00Z")),
                    7,
                )
                .unwrap(),
            );
        }
        assert!(!copies[0].exists());
        assert!(!copies[1].exists());
        assert!(copies[2..].iter().all(|p| p.is_dir()));
        assert_eq!(completed_backups(&fixture.auto()).unwrap().len(), 7);
        assert!(forged.join("levels.db").is_file());
        assert_eq!(
            fs::read_to_string(fixture.auto().join("manual/keep.txt")).unwrap(),
            "unmanaged"
        );
    }

    // A malformed manifest must neither grant deletion ownership nor suppress
    // catch-up. In particular, its file list must not escape the backup.
    #[test]
    fn malicious_manifest_and_extra_directory_are_not_owned() {
        let fixture = Fixture::new();
        let db = fixture.database();
        let bad = snapshot(&db, &fixture.0, at("2026-11-01T01:00:00Z"), 7).unwrap();
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(bad.join("manifest.json")).unwrap()).unwrap();
        manifest["files"] = serde_json::json!(["levels.db", "../../outside.txt"]);
        fs::write(
            bad.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let extra = snapshot(&db, &fixture.0, at("2026-11-02T01:00:00Z"), 7).unwrap();
        fs::create_dir(extra.join("unmanaged")).unwrap();
        fs::write(extra.join("unmanaged/keep.txt"), "keep").unwrap();
        snapshot(&db, &fixture.0, at("2026-11-03T01:00:00Z"), 1).unwrap();
        assert!(bad.is_dir());
        assert!(extra.join("unmanaged/keep.txt").is_file());
        assert_eq!(completed_backups(&fixture.auto()).unwrap().len(), 1);
    }

    // A copy error after SQLite succeeds must clean only this invocation's
    // staging directory, without running retention or deleting stale staging.
    #[test]
    fn failed_new_copy_preserves_previous_backups_and_cleans_own_temp() {
        let fixture = Fixture::new();
        let db = fixture.database();
        let first = snapshot(&db, &fixture.0, at("2026-11-14T01:00:00Z"), 7).unwrap();
        let second = snapshot(&db, &fixture.0, at("2026-11-15T01:00:00Z"), 7).unwrap();
        let stale = fixture.auto().join(".tmp-unmanaged");
        fs::create_dir(&stale).unwrap();
        fixture.put(
            "config/too-large.txt",
            vec![b'a'; MAX_FILE_BYTES as usize + 1],
        );
        assert!(snapshot(&db, &fixture.0, at("2026-11-16T01:00:00Z"), 1).is_err());
        assert!(first.join("levels.db").is_file());
        assert!(second.join("levels.db").is_file());
        assert!(stale.is_dir());
        assert_eq!(fs::read_dir(fixture.auto()).unwrap().count(), 3);
    }

    #[test]
    fn invalid_sqlite_source_preserves_last_good_copy() {
        let fixture = Fixture::new();
        let db = fixture.database();
        let previous = snapshot(&db, &fixture.0, at("2026-11-15T01:00:00Z"), 7).unwrap();
        let mut source = OpenOptions::new()
            .write(true)
            .open(fixture.0.join("levels.db"))
            .unwrap();
        source.write_all(b"not-a-sqlite-db!!").unwrap();
        source.sync_all().unwrap();
        drop(source);
        assert!(snapshot(&db, &fixture.0, at("2026-11-16T01:00:00Z"), 1).is_err());
        assert_eq!(fs::read_dir(fixture.auto()).unwrap().count(), 1);
        let copy = Connection::open(previous.join("levels.db")).unwrap();
        assert_eq!(
            copy.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
    }

    #[test]
    fn memory_database_can_be_snapshotted() {
        let fixture = Fixture::new();
        let db = Database::in_memory().unwrap();
        db.add_money(1, 2, "test", 0, 123).unwrap();
        let completed = snapshot(&db, &fixture.0, at("2026-11-16T01:00:00Z"), 7).unwrap();
        let copy = Connection::open(completed.join("levels.db")).unwrap();
        assert_eq!(
            copy.query_row(
                "SELECT balance FROM economy WHERE guild_id=1 AND user_id=2",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            123
        );
    }

    // A source handle retaining Database::lock blocks this real write until
    // after the backup ends, and fails the bounded receive below.
    #[test]
    fn online_backup_keeps_original_connection_available_for_writes() {
        use std::sync::{mpsc, Arc};
        let fixture = Fixture::new();
        let db = Arc::new(fixture.database());
        db.lock().execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE probe(value BLOB); INSERT INTO probe VALUES(zeroblob(131072));").unwrap();
        let (tx, rx) = mpsc::channel();
        let mut worker = None;
        let result = with_source(&db, |source| {
            let mut destination = Connection::open_in_memory()?;
            let backup = Backup::new(source, &mut destination)?;
            assert!(matches!(backup.step(1)?, StepResult::More));
            let original = db.clone();
            worker = Some(std::thread::spawn(move || {
                let result = original.add_money(1, 2, "concurrent", 0, 42);
                tx.send(result).unwrap();
            }));
            let write = rx.recv_timeout(Duration::from_secs(2));
            // Finish/release the backup even if the receive times out, so a
            // regression reports a failure instead of hanging the test suite.
            backup.run_to_completion(256, Duration::from_millis(1), None)?;
            Ok(write)
        })
        .unwrap();
        worker.unwrap().join().unwrap();
        assert_eq!(
            result
                .expect("original database mutex was blocked")
                .unwrap(),
            42
        );
    }

    #[test]
    fn keeps_just_published_snapshot_when_clock_moves_backward() {
        let fixture = Fixture::new();
        let db = fixture.database();
        let previous = snapshot(&db, &fixture.0, at("2026-11-17T01:00:00Z"), 7).unwrap();
        let current = snapshot(&db, &fixture.0, at("2026-11-16T01:00:00Z"), 1).unwrap();
        assert!(current.join("levels.db").is_file());
        assert!(!previous.exists());
    }

    // Treating startup as "today's date" incorrectly skips yesterday's due
    // checkpoint before 04:00; using > instead of >= misses the exact boundary.
    #[test]
    fn schedule_uses_preceding_checkpoint_and_precise_next_deadline() {
        let settings = defaults();
        for (now, previous, next) in [
            (
                "2026-11-16T00:59:59Z",
                "2026-11-15T01:00:00Z",
                "2026-11-16T01:00:00Z",
            ),
            (
                "2026-11-16T01:00:00Z",
                "2026-11-16T01:00:00Z",
                "2026-11-17T01:00:00Z",
            ),
            (
                "2026-11-16T01:00:01Z",
                "2026-11-16T01:00:00Z",
                "2026-11-17T01:00:00Z",
            ),
            (
                "2026-12-31T23:59:59Z",
                "2026-12-31T01:00:00Z",
                "2027-01-01T01:00:00Z",
            ),
        ] {
            assert_eq!(preceding_checkpoint(at(now), &settings), at(previous));
            assert_eq!(next_deadline(at(now), &settings), at(next));
        }
        let custom = BackupSettings::load(&Settings::parse("[backups]\ntime=23:45\nkeep=0\n"));
        assert_eq!(
            next_deadline(at("2026-11-16T20:44:59Z"), &custom),
            at("2026-11-16T20:45:00Z")
        );
        assert_eq!(custom.keep, 1);
        let invalid = BackupSettings::load(&Settings::parse("[backups]\ntime=25:00\nkeep=bad\n"));
        assert_eq!(
            next_deadline(at("2026-11-16T00:00:00Z"), &invalid),
            at("2026-11-16T01:00:00Z")
        );
        assert_eq!(invalid.keep, 7);
        assert_eq!(
            wake_after(
                at("2026-11-16T00:59:59Z"),
                at("2026-11-16T01:00:01Z"),
                false,
                &settings
            ),
            at("2026-11-16T01:10:01Z")
        );
        assert_eq!(
            wake_after(
                at("2026-11-16T00:59:59Z"),
                at("2026-11-16T01:00:01Z"),
                true,
                &settings
            ),
            at("2026-11-16T01:00:00Z")
        );
    }

    #[test]
    fn catchup_ignores_stale_invalid_and_future_copies() {
        let fixture = Fixture::new();
        let db = fixture.database();
        let settings = defaults();
        let now = at("2026-11-16T00:00:00Z");
        assert!(backup_due(&fixture.0, now, &settings).unwrap());
        snapshot(&db, &fixture.0, at("2026-11-15T00:59:59Z"), 7).unwrap();
        assert!(backup_due(&fixture.0, now, &settings).unwrap());
        snapshot(&db, &fixture.0, at("2026-11-17T01:00:00Z"), 7).unwrap();
        assert!(backup_due(&fixture.0, now, &settings).unwrap());
        snapshot(&db, &fixture.0, at("2026-11-15T01:00:00Z"), 7).unwrap();
        assert!(!backup_due(&fixture.0, now, &settings).unwrap());
        assert!(backup_due(&fixture.0, at("2026-11-16T01:00:00Z"), &settings).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn excludes_source_symlinks_and_never_prunes_linked_trees() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let outside = Fixture::new();
        let db = fixture.database();
        outside.put("private.txt", "outside");
        fs::create_dir(fixture.0.join("config")).unwrap();
        symlink(
            outside.0.join("private.txt"),
            fixture.0.join("config/ordinary.txt"),
        )
        .unwrap();
        symlink(&outside.0, fixture.0.join("miniapp")).unwrap();
        symlink(
            outside.0.join("private.txt"),
            fixture.0.join("patchnotes.txt"),
        )
        .unwrap();
        let first = snapshot(&db, &fixture.0, at("2026-11-14T01:00:00Z"), 7).unwrap();
        assert!(!first.join("config/ordinary.txt").exists());
        assert!(!first.join("miniapp").exists());
        assert!(!first.join("patchnotes.txt").exists());
        symlink(&outside.0, first.join("extra")).unwrap();
        let second = snapshot(&db, &fixture.0, at("2026-11-15T01:00:00Z"), 7).unwrap();
        fs::remove_file(second.join("levels.db")).unwrap();
        symlink(outside.0.join("private.txt"), second.join("levels.db")).unwrap();
        let linked = fixture
            .auto()
            .join("musicbot-v1-20261101T010000.000000000Z-1111111111111111");
        symlink(&outside.0, &linked).unwrap();
        snapshot(&db, &fixture.0, at("2026-11-16T01:00:00Z"), 1).unwrap();
        assert!(first.join("extra").is_symlink());
        assert!(second.join("levels.db").is_symlink());
        assert!(linked.is_symlink());
        assert_eq!(
            fs::read_to_string(outside.0.join("private.txt")).unwrap(),
            "outside"
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_linked_destination_and_restricts_linux_permissions() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let fixture = Fixture::new();
        let outside = Fixture::new();
        let db = fixture.database();
        fs::create_dir(fixture.0.join("backups")).unwrap();
        symlink(&outside.0, fixture.auto()).unwrap();
        assert!(snapshot(&db, &fixture.0, at("2026-11-16T01:00:00Z"), 7).is_err());
        assert_eq!(fs::read_dir(&outside.0).unwrap().count(), 0);
        fs::remove_file(fixture.auto()).unwrap();
        fixture.put("config/settings.ini", "ordinary\n");
        let completed = snapshot(&db, &fixture.0, at("2026-11-16T01:00:00Z"), 7).unwrap();
        for dir in [
            fixture.0.join("backups"),
            fixture.auto(),
            completed.clone(),
            completed.join("config"),
        ] {
            assert_eq!(
                fs::metadata(dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        for file in ["levels.db", "manifest.json", "config/settings.ini"] {
            assert_eq!(
                fs::metadata(completed.join(file))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
