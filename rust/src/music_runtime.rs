//! Durable playback handoff and bounded metadata-only speculative work.
use crate::{db::Database, music_qol::Item, Data, Error, Shared};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{watch, Notify};

pub fn prepare(db: &Database) -> rusqlite::Result<()> {
    db.lock().execute_batch("CREATE TABLE IF NOT EXISTS music_current (guild_id INTEGER PRIMARY KEY, url TEXT NOT NULL, title TEXT NOT NULL, details TEXT NOT NULL);")
}

fn row_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    Ok(Item {
        id: row.get(0)?,
        url: row.get(1)?,
        title: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
        details: row
            .get::<_, Option<String>>(3)?
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or(Value::Null),
    })
}

/// Removing a queue row and recording the pending resolver input is one commit.
/// Caller holds the guild playback lock until resolution/playback installation.
pub fn claim(db: &Database, guild: u64) -> rusqlite::Result<Option<Item>> {
    let mut conn = db.lock();
    let tx = conn.transaction()?;
    let occupied: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM music_current WHERE guild_id=?1)",
        [guild as i64],
        |r| r.get(0),
    )?;
    if occupied {
        return Ok(None);
    }
    let item=tx.query_row("SELECT id,url,title,details FROM queue WHERE guild_id=?1 ORDER BY COALESCE(sort_order,id),id LIMIT 1",[guild as i64],row_item).optional()?;
    if let Some(item) = &item {
        tx.execute(
            "INSERT INTO music_current(guild_id,url,title,details) VALUES(?1,?2,?3,?4)",
            params![guild as i64, item.url, item.title, item.details.to_string()],
        )?;
        tx.execute(
            "DELETE FROM queue WHERE guild_id=?1 AND id=?2",
            params![guild as i64, item.id],
        )?;
    }
    tx.commit()?;
    Ok(item)
}

/// The selected page and original request details survive restart; never store audio URLs.
pub fn selected(
    db: &Database,
    guild: u64,
    url: &str,
    title: &str,
    details: &Value,
) -> rusqlite::Result<()> {
    let changed = db.lock().execute(
        "UPDATE music_current SET url=?2,title=?3,details=?4 WHERE guild_id=?1",
        params![guild as i64, url, title, details.to_string()],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    Ok(())
}
fn return_in(tx: &rusqlite::Transaction<'_>, guild: i64) -> rusqlite::Result<usize> {
    let n=tx.execute("INSERT INTO queue(guild_id,url,title,sort_order,details) SELECT guild_id,url,title,(SELECT COALESCE(MIN(COALESCE(sort_order,id)),0)-1 FROM queue WHERE guild_id=?1),details FROM music_current WHERE guild_id=?1",[guild])?;
    tx.execute("DELETE FROM music_current WHERE guild_id=?1", [guild])?;
    Ok(n)
}
/// Used for leave, voice loss, and spawn failure: return and clear atomically.
pub fn return_current(db: &Database, guild: u64) -> rusqlite::Result<usize> {
    let mut conn = db.lock();
    let tx = conn.transaction()?;
    let n = return_in(&tx, guild as i64)?;
    tx.commit()?;
    Ok(n)
}
/// Retire a finished/skipped/failed request, or atomically put a repeat first.
pub fn finish(db: &Database, guild: u64, repeat: bool) -> rusqlite::Result<()> {
    if repeat {
        return_current(db, guild)?;
    } else {
        db.lock().execute(
            "DELETE FROM music_current WHERE guild_id=?1",
            [guild as i64],
        )?;
    }
    Ok(())
}
/// Transfer the claimed input into a durable version choice in one commit.
/// The caller serializes this with playback using the guild playback lock.
pub fn handoff_choice(
    db: &Database,
    guild: u64,
    nonce: &str,
    user: u64,
    payload: &Value,
    expires: f64,
) -> rusqlite::Result<()> {
    let mut conn = db.lock();
    let tx = conn.transaction()?;
    let (url, title, details): (String, String, String) = tx.query_row(
        "SELECT url,title,details FROM music_current WHERE guild_id=?1",
        [guild as i64],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let mut payload = payload.clone();
    let object = payload
        .as_object_mut()
        .ok_or(rusqlite::Error::InvalidQuery)?;
    let details: Value = serde_json::from_str(&details).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;
    object.insert(
        "restore".into(),
        serde_json::json!({"url":url,"title":title,"details":details}),
    );
    tx.execute(
        "INSERT INTO music_choices(id,guild_id,user_id,expires,payload) VALUES(?1,?2,?3,?4,?5)",
        params![
            nonce,
            guild as i64,
            user as i64,
            expires,
            payload.to_string()
        ],
    )?;
    tx.execute(
        "DELETE FROM music_current WHERE guild_id=?1",
        [guild as i64],
    )?;
    tx.commit()
}

fn return_choice_in(tx: &rusqlite::Transaction<'_>, nonce: &str) -> rusqlite::Result<usize> {
    let row: Option<(i64, String)> = tx
        .query_row(
            "SELECT guild_id,payload FROM music_choices WHERE id=?1",
            [nonce],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((guild, payload)) = row else {
        return Ok(0);
    };
    let payload: Value = serde_json::from_str(&payload).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let Some(restore) = payload.get("restore") else {
        return Ok(0);
    };
    let url = restore["url"]
        .as_str()
        .ok_or(rusqlite::Error::InvalidQuery)?;
    let title = restore["title"]
        .as_str()
        .ok_or(rusqlite::Error::InvalidQuery)?;
    let details = restore
        .get("details")
        .ok_or(rusqlite::Error::InvalidQuery)?;
    let n = tx.execute(
        "INSERT INTO queue(guild_id,url,title,sort_order,details) VALUES(?1,?2,?3,(SELECT COALESCE(MIN(COALESCE(sort_order,id)),0)-1 FROM queue WHERE guild_id=?1),?4)",
        params![guild, url, title, details.to_string()],
    )?;
    tx.execute("DELETE FROM music_choices WHERE id=?1", [nonce])?;
    Ok(n)
}

/// Restore an unconsumed durable choice after publication failure. Legacy choices
/// have no restore input and are left alone; consumed choices return zero.
pub fn return_choice(db: &Database, nonce: &str) -> rusqlite::Result<usize> {
    let mut conn = db.lock();
    let tx = conn.transaction()?;
    let n = return_choice_in(&tx, nonce)?;
    tx.commit()?;
    Ok(n)
}

/// Startup only, before commands/event loops. Does not interact with voice.
pub fn recover(db: &Database) -> rusqlite::Result<usize> {
    let mut conn = db.lock();
    let tx = conn.transaction()?;
    let guilds: Vec<i64> = {
        let mut st = tx.prepare("SELECT guild_id FROM music_current ORDER BY guild_id")?;
        let rows = st.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    // Recover only live choices. Reverse insertion order makes front insertion
    // preserve their original order; legacy payloads remain untouched.
    let choices: Vec<String> = {
        let mut st = tx
            .prepare("SELECT id,payload FROM music_choices WHERE expires>?1 ORDER BY rowid DESC")?;
        let rows = st.query_map([crate::db::now()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut choices = Vec::new();
        for row in rows {
            let (id, payload) = row?;
            if serde_json::from_str::<Value>(&payload)
                .ok()
                .is_some_and(|p| p.get("restore").is_some())
            {
                choices.push(id);
            }
        }
        choices
    };
    let mut n = 0;
    for nonce in choices {
        n += return_choice_in(&tx, &nonce)?;
    }
    for guild in guilds {
        n += return_in(&tx, guild)?;
    }
    tx.commit()?;
    Ok(n)
}

const TTL: Duration = Duration::from_secs(600);
const CAPACITY: usize = 128;
type SearchResult = Result<Vec<Value>, String>;
struct Entry {
    created: Instant,
    result: watch::Receiver<Option<SearchResult>>,
}
#[derive(Default)]
pub struct MetadataCache {
    entries: Mutex<HashMap<(String, usize), Arc<Entry>>>,
    changed: Notify,
}
fn safe_candidates(list: Vec<Value>) -> Vec<Value> {
    list.into_iter()
        .map(|v| {
            let mut m = serde_json::Map::new();
            for field in ["title", "webpage_url", "duration", "thumbnail", "uploader"] {
                if let Some(value) = v.get(field) {
                    m.insert(field.into(), value.clone());
                }
            }
            Value::Object(m)
        })
        .collect()
}
// Dropping/cancelling a leader wakes waiters; the next request retries.
struct Flight<'a>(watch::Sender<Option<SearchResult>>, &'a Notify);
impl Drop for Flight<'_> {
    fn drop(&mut self) {
        if self.0.borrow().is_none() {
            self.0
                .send_replace(Some(Err("metadata lookup cancelled".into())));
            self.1.notify_waiters();
        }
    }
}
impl MetadataCache {
    async fn get<F, Fut>(&self, query: &str, count: usize, load: F) -> Result<Vec<Value>, Error>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Vec<Value>, Error>>,
    {
        let key = (query.trim().to_owned(), count.clamp(1, 5));
        let (entry, leader) = loop {
            // Register before checking capacity to avoid a lost wakeup.
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let found = {
                let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
                entries.retain(|_, e| {
                    let r = e.result.borrow();
                    r.is_none() || (e.created.elapsed() < TTL && matches!(&*r, Some(Ok(_))))
                });
                if let Some(e) = entries.get(&key) {
                    Some((e.clone(), None))
                } else {
                    if entries.len() >= CAPACITY {
                        let oldest = entries
                            .iter()
                            .filter(|(_, e)| e.result.borrow().is_some())
                            .min_by_key(|(_, e)| e.created)
                            .map(|(k, _)| k.clone());
                        if let Some(oldest) = oldest {
                            entries.remove(&oldest);
                        }
                    }
                    if entries.len() < CAPACITY {
                        let (tx, rx) = watch::channel(None);
                        let e = Arc::new(Entry {
                            created: Instant::now(),
                            result: rx,
                        });
                        entries.insert(key.clone(), e.clone());
                        Some((e, Some(Flight(tx, &self.changed))))
                    } else {
                        None
                    }
                }
            };
            if let Some(found) = found {
                break found;
            }
            notified.await;
        };
        if let Some(flight) = leader {
            let result = load().await.map(safe_candidates).map_err(|e| e.to_string());
            flight.0.send_replace(Some(result));
            self.changed.notify_waiters();
        }
        let mut rx = entry.result.clone();
        loop {
            let result = rx.borrow_and_update().clone();
            if let Some(result) = result {
                return result.map_err(Into::into);
            }
            if rx.changed().await.is_err() {
                return Err("metadata lookup cancelled".into());
            }
        }
    }
    #[cfg(test)]
    fn expire_for_test(&self) {
        let mut entries = self.entries.lock().unwrap();
        for e in entries.values_mut() {
            if let Some(e) = Arc::get_mut(e) {
                e.created = Instant::now() - TTL;
            }
        }
    }
}

pub type Resolved = ((String, String), String, String, Value);
struct Prefetch {
    item: Item,
    task: tokio::task::AbortHandle,
    result: Option<Resolved>,
}
#[derive(Default)]
pub struct RuntimeState {
    pub cache: MetadataCache,
    prefetch: Mutex<HashMap<u64, Prefetch>>,
}
pub async fn cached_candidates(
    data: &Data,
    query: &str,
    count: usize,
) -> Result<Vec<Value>, Error> {
    let count = count.clamp(1, 5);
    data.music_runtime
        .cache
        .get(query, count, || {
            crate::music_qol::uncached_candidates(data, query, count)
        })
        .await
}
fn same_item(a: &Item, b: &Item) -> bool {
    a.id == b.id && a.url == b.url && a.title == b.title && a.details == b.details
}
fn next_unresolved(data: &Data, guild: u64) -> Option<Item> {
    crate::music_qol::items(data, guild)
        .ok()?
        .into_iter()
        .find(|i| i.details["resolved"] != true)
}
/// Call after queue edits/enqueues. A single metadata-only task per guild; no global lock across await.
pub fn queue_changed(data: &Shared, guild: u64) {
    let next = next_unresolved(data, guild);
    let mut tasks = data
        .music_runtime
        .prefetch
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if let (Some(old), Some(next)) = (tasks.get(&guild), &next) {
        if same_item(&old.item, next) {
            return;
        }
    }
    if let Some(old) = tasks.remove(&guild) {
        old.task.abort();
    }
    let Some(item) = next else { return };
    let shared = data.clone();
    let original = item.clone();
    let task = tokio::spawn(async move {
        let result = crate::music_qol::resolve(&shared, original.clone())
            .await
            .ok();
        let valid = next_unresolved(&shared, guild).is_some_and(|i| same_item(&i, &original));
        let mut tasks = shared
            .music_runtime
            .prefetch
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(task) = tasks.get_mut(&guild) {
            if same_item(&task.item, &original) && valid {
                task.result = result;
            }
        }
    });
    tasks.insert(
        guild,
        Prefetch {
            item,
            task: task.abort_handle(),
            result: None,
        },
    );
}
/// Claimed queue identity must match exactly, including request_id/details.
pub fn take_prefetched(data: &Data, guild: u64, item: &Item) -> Option<Resolved> {
    let mut tasks = data
        .music_runtime
        .prefetch
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let old = tasks.remove(&guild)?;
    old.task.abort();
    if same_item(&old.item, item) {
        old.result
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn database() -> Database {
        let db = Database::in_memory().unwrap();
        prepare(&db).unwrap();
        db
    }
    #[test]
    fn choice_handoff_rolls_back_on_insert_or_current_delete_failure() {
        for trigger in [
            "CREATE TRIGGER reject_handoff BEFORE INSERT ON music_choices BEGIN SELECT RAISE(ABORT,'reject'); END;",
            "CREATE TRIGGER reject_handoff BEFORE DELETE ON music_current BEGIN SELECT RAISE(ABORT,'reject'); END;",
        ] {
            let db = database();
            db.queue_add(1, "original", "Original").unwrap();
            claim(&db, 1).unwrap().unwrap();
            db.lock().execute_batch(trigger).unwrap();
            assert!(handoff_choice(&db, 1, "choice", 7, &json!({"list":[]}), 123.).is_err());
            assert_eq!(db.lock().query_row("SELECT COUNT(*) FROM music_choices", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
            db.lock().execute_batch("DROP TRIGGER reject_handoff").unwrap();
            assert_eq!(return_current(&db, 1).unwrap(), 1);
        }
        assert!(handoff_choice(&database(), 1, "missing", 7, &json!({}), 123.).is_err());
    }
    #[test]
    fn choice_return_restores_original_metadata_once_and_rolls_back_failure() {
        let db = database();
        db.queue_add(1, "original", "Original").unwrap();
        db.queue_add(1, "later", "Later").unwrap();
        let details = json!({"request_id":42,"requester":7,"spotify_query":"original search"});
        db.lock()
            .execute(
                "UPDATE queue SET details=?1 WHERE url='original'",
                [details.to_string()],
            )
            .unwrap();
        claim(&db, 1).unwrap().unwrap();
        handoff_choice(
            &db,
            1,
            "choice",
            7,
            &json!({"title":"Ambiguous","list":[1]}),
            123.,
        )
        .unwrap();
        assert_eq!(return_current(&db, 1).unwrap(), 0);
        let payload: String = db
            .lock()
            .query_row(
                "SELECT payload FROM music_choices WHERE id='choice'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["list"], json!([1]));
        assert_eq!(payload["restore"]["details"], details);
        db.lock().execute_batch("CREATE TRIGGER reject_restore BEFORE INSERT ON queue BEGIN SELECT RAISE(ABORT,'reject'); END;").unwrap();
        assert!(return_choice(&db, "choice").is_err());
        db.lock()
            .execute_batch("DROP TRIGGER reject_restore")
            .unwrap();
        assert_eq!(return_choice(&db, "choice").unwrap(), 1);
        assert_eq!(return_choice(&db, "choice").unwrap(), 0);
        let restored = claim(&db, 1).unwrap().unwrap();
        assert_eq!(restored.url, "original");
        assert_eq!(restored.title, "Original");
        assert_eq!(restored.details, details);
    }
    #[test]
    fn startup_recovers_durable_choices_once_but_not_consumed_or_legacy_choices() {
        let db = database();
        db.queue_add(1, "pending", "Pending").unwrap();
        claim(&db, 1).unwrap().unwrap();
        handoff_choice(&db, 1, "pending", 7, &json!({}), crate::db::now() + 600.).unwrap();
        db.queue_add(2, "consumed", "Consumed").unwrap();
        claim(&db, 2).unwrap().unwrap();
        handoff_choice(&db, 2, "consumed", 7, &json!({}), crate::db::now() + 600.).unwrap();
        db.lock()
            .execute("DELETE FROM music_choices WHERE id='consumed'", [])
            .unwrap();
        db.lock()
            .execute(
                "INSERT INTO music_choices(id,guild_id,payload) VALUES('legacy',3,'{}')",
                [],
            )
            .unwrap();
        assert_eq!(recover(&db).unwrap(), 1);
        assert_eq!(recover(&db).unwrap(), 0);
        assert_eq!(
            db.queue_tracks(1, 10).unwrap(),
            vec![("pending".into(), "Pending".into())]
        );
        assert_eq!(db.queue_count(2).unwrap(), 0);
        assert_eq!(return_choice(&db, "legacy").unwrap(), 0);
        assert_eq!(
            db.lock()
                .query_row(
                    "SELECT COUNT(*) FROM music_choices WHERE id='legacy'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
    }
    #[test]
    fn startup_does_not_restore_expired_choices() {
        let db = database();
        db.queue_add(1, "expired", "Expired").unwrap();
        claim(&db, 1).unwrap().unwrap();
        handoff_choice(&db, 1, "expired", 7, &json!({}), crate::db::now() - 1.).unwrap();
        assert_eq!(recover(&db).unwrap(), 0);
        assert_eq!(recover(&db).unwrap(), 0);
        assert_eq!(db.queue_count(1).unwrap(), 0);
    }
    #[test]
    fn startup_restores_choices_in_original_insertion_order() {
        let db = database();
        for (nonce, url) in [("z-first", "first"), ("a-second", "second")] {
            db.queue_add(1, url, url).unwrap();
            claim(&db, 1).unwrap().unwrap();
            handoff_choice(&db, 1, nonce, 7, &json!({}), crate::db::now() + 600.).unwrap();
        }
        db.queue_add(1, "later", "later").unwrap();
        assert_eq!(recover(&db).unwrap(), 2);
        assert_eq!(recover(&db).unwrap(), 0);
        let urls: Vec<String> = db
            .queue_tracks(1, 10)
            .unwrap()
            .into_iter()
            .map(|(url, _)| url)
            .collect();
        assert_eq!(urls, ["first", "second", "later"]);
    }
    #[test]
    fn popped_pending_and_resolved_recover_once_at_front() {
        let db = database();
        db.queue_add(1, "original", "Original").unwrap();
        db.queue_add(1, "later", "Later").unwrap();
        let item = claim(&db, 1).unwrap().unwrap();
        assert_eq!(db.queue_count(1).unwrap(), 1);
        assert_eq!(recover(&db).unwrap(), 1);
        assert_eq!(recover(&db).unwrap(), 0);
        assert_eq!(db.queue_tracks(1, 10).unwrap()[0].0, "original");
        let _ = claim(&db, 1).unwrap().unwrap();
        selected(
            &db,
            1,
            "selected",
            "Selected",
            &json!({"resolved":true,"request_id":item.details["request_id"]}),
        )
        .unwrap();
        assert_eq!(recover(&db).unwrap(), 1);
        assert_eq!(
            db.queue_tracks(1, 10).unwrap()[0],
            ("selected".into(), "Selected".into())
        );
    }
    #[test]
    fn occupied_claim_keeps_queue_and_repeat_preserves_request_metadata() {
        let db = database();
        db.queue_add(1, "first", "First").unwrap();
        db.queue_add(1, "second", "Second").unwrap();
        db.queue_add(2, "other", "Other").unwrap();
        let details = json!({"request_id":123,"requester":456,"original":"search"});
        db.lock()
            .execute(
                "UPDATE queue SET details=?1 WHERE guild_id=1 AND url='first'",
                [details.to_string()],
            )
            .unwrap();
        let first = claim(&db, 1).unwrap().unwrap();
        assert_eq!(first.details, details);
        assert!(claim(&db, 1).unwrap().is_none());
        assert_eq!(db.queue_count(1).unwrap(), 1);
        assert!(claim(&db, 2).unwrap().is_some());
        finish(&db, 1, true).unwrap();
        let repeated = claim(&db, 1).unwrap().unwrap();
        assert_eq!(repeated.url, "first");
        assert_eq!(repeated.details, details);
        assert_eq!(db.queue_count(1).unwrap(), 1);
    }
    #[test]
    fn failed_claim_rolls_back_queue_delete() {
        let db = database();
        db.queue_add(1, "a", "A").unwrap();
        db.lock().execute_batch("CREATE TRIGGER reject_current BEFORE INSERT ON music_current BEGIN SELECT RAISE(ABORT,'reject'); END;").unwrap();
        assert!(claim(&db, 1).is_err());
        assert_eq!(db.queue_count(1).unwrap(), 1);
    }
    #[test]
    fn failed_recovery_rolls_back_and_leave_does_not_duplicate() {
        let db = database();
        db.queue_add(1, "a", "A").unwrap();
        claim(&db, 1).unwrap();
        db.lock().execute_batch("CREATE TRIGGER reject_queue BEFORE INSERT ON queue BEGIN SELECT RAISE(ABORT,'reject'); END;").unwrap();
        assert!(return_current(&db, 1).is_err());
        db.lock()
            .execute_batch("DROP TRIGGER reject_queue")
            .unwrap();
        return_current(&db, 1).unwrap();
        assert_eq!(recover(&db).unwrap(), 0);
        assert_eq!(db.queue_count(1).unwrap(), 1);
    }
    #[tokio::test]
    async fn cache_singleflight_safe_fields_ttl_and_failure_eviction() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cache = MetadataCache::default();
        let calls = AtomicUsize::new(0);
        let load = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            Ok(vec![
                json!({"title":"A","webpage_url":"https://example.com/a","url":"signed-secret","formats":[1]}),
            ])
        };
        let (a, b) = tokio::join!(cache.get("q", 5, load), cache.get("q", 5, load));
        assert_eq!(a.unwrap(), b.unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let result = cache.get("q", 5, load).await.unwrap();
        assert!(result[0].get("url").is_none());
        assert!(result[0].get("formats").is_none());
        cache.expire_for_test();
        cache.get("q", 5, load).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(cache
            .get("bad", 1, || async { Err("failed".into()) })
            .await
            .is_err());
        assert_eq!(
            cache
                .get("bad", 1, || async { Ok(vec![json!({"title":"retry"})]) })
                .await
                .unwrap()[0]["title"],
            "retry"
        );
        for n in 0..140 {
            cache
                .get(&format!("q{n}"), 1, || async { Ok(vec![]) })
                .await
                .unwrap();
        }
        assert!(cache.entries.lock().unwrap().len() <= 128);
    }
    #[tokio::test]
    async fn cancelled_full_cache_leader_wakes_capacity_waiter() {
        let cache = Arc::new(MetadataCache::default());
        let mut leaders = Vec::new();
        for n in 0..CAPACITY {
            let cache = cache.clone();
            leaders.push(tokio::spawn(async move {
                cache
                    .get(&format!("pending{n}"), 1, || async {
                        std::future::pending::<Result<Vec<Value>, Error>>().await
                    })
                    .await
            }));
        }
        while cache.entries.lock().unwrap().len() != CAPACITY {
            tokio::task::yield_now().await;
        }
        let waiting = cache.get("extra", 1, || async { Ok(vec![json!({"title":"awake"})]) });
        tokio::pin!(waiting);
        // Poll explicitly to ensure the capacity waiter is registered before abort.
        std::future::poll_fn(|cx| {
            assert!(waiting.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        leaders[0].abort();
        let result = tokio::time::timeout(Duration::from_secs(1), waiting).await;
        for leader in leaders {
            leader.abort();
        }
        assert_eq!(result.unwrap().unwrap()[0]["title"], "awake");
    }
    #[test]
    fn finished_and_repeated_requests_retire_exactly_once() {
        let db = database();
        db.queue_add(1, "first", "First").unwrap();
        claim(&db, 1).unwrap().unwrap();
        finish(&db, 1, true).unwrap();
        finish(&db, 1, true).unwrap();
        assert_eq!(db.queue_count(1).unwrap(), 1);
        claim(&db, 1).unwrap().unwrap();
        finish(&db, 1, false).unwrap();
        assert_eq!(recover(&db).unwrap(), 0);
        assert_eq!(db.queue_count(1).unwrap(), 0);
    }
    #[test]
    fn prefetched_identity_rejects_changed_request_or_details() {
        let a = crate::music_qol::Item {
            id: 1,
            url: "same".into(),
            title: "A".into(),
            details: json!({"request_id":1}),
        };
        let mut b = a.clone();
        b.details["request_id"] = 2.into();
        assert!(!same_item(&a, &b));
        b = a.clone();
        b.details["resolved"] = true.into();
        assert!(!same_item(&a, &b));
        b = a.clone();
        b.url = "changed".into();
        assert!(!same_item(&a, &b));
        b = a.clone();
        b.id += 1;
        assert!(!same_item(&a, &b));
    }
}
