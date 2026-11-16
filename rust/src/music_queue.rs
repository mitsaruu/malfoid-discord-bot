//! Public queue views; only their opener may operate their controls.
use crate::{db, music_qol::Item, Context, Error, Shared};
use poise::serenity_prelude as s;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

const PAGE_SIZE: usize = 10;
const VIEW_SECONDS: f64 = 900.;

#[derive(Clone)]
struct View {
    id: String,
    guild: u64,
    user: u64,
    channel: u64,
    message: u64,
    page: usize,
    revision: i64,
    snapshot: String,
    selected: Option<i64>,
}

pub fn prepare(db: &db::Database) -> rusqlite::Result<()> {
    db.lock().execute_batch(
        "CREATE TABLE IF NOT EXISTS music_queue_views (
            id TEXT PRIMARY KEY, guild_id INTEGER NOT NULL, user_id INTEGER NOT NULL,
            channel_id INTEGER NOT NULL, message_id INTEGER NOT NULL DEFAULT 0,
            page INTEGER NOT NULL DEFAULT 0, revision INTEGER NOT NULL DEFAULT 0,
            expires REAL NOT NULL, snapshot TEXT NOT NULL DEFAULT '[]', selected INTEGER,
            claimed INTEGER NOT NULL DEFAULT 0, spent_revision INTEGER NOT NULL DEFAULT -1
         ); CREATE INDEX IF NOT EXISTS music_queue_views_expiry ON music_queue_views(expires);",
    )
}

fn new_view(
    db: &db::Database,
    guild: u64,
    user: u64,
    channel: u64,
    now: f64,
) -> rusqlite::Result<View> {
    let id = format!("{:032x}", rand::random::<u128>());
    let conn = db.lock();
    conn.execute("DELETE FROM music_queue_views WHERE expires<=?1", [now])?;
    conn.execute("INSERT INTO music_queue_views(id,guild_id,user_id,channel_id,expires) VALUES(?1,?2,?3,?4,?5)",
        params![id, guild as i64, user as i64, channel as i64, now + VIEW_SECONDS])?;
    Ok(View {
        id,
        guild,
        user,
        channel,
        message: 0,
        page: 0,
        revision: 0,
        snapshot: "[]".into(),
        selected: None,
    })
}

/// Hold the connection only for the persisted identity check and revision claim.
fn claim_view(db: &db::Database, view: &View, now: f64) -> rusqlite::Result<Option<View>> {
    let conn = db.lock();
    let found = conn
        .query_row(
            "SELECT page,snapshot,selected FROM music_queue_views
         WHERE id=?1 AND guild_id=?2 AND user_id=?3 AND channel_id=?4 AND message_id=?5
         AND message_id<>0 AND revision=?6 AND expires>?7 AND claimed=0",
            params![
                view.id,
                view.guild as i64,
                view.user as i64,
                view.channel as i64,
                view.message as i64,
                view.revision,
                now
            ],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((page, snapshot, selected)) = found else {
        return Ok(None);
    };
    let changed = conn.execute(
        "UPDATE music_queue_views SET revision=revision+1,claimed=1 WHERE id=?1 AND revision=?2 AND claimed=0",
        params![view.id, view.revision],
    )?;
    if changed != 1 {
        return Ok(None);
    }
    Ok(Some(View {
        page: page.max(0) as usize,
        snapshot,
        selected,
        revision: view.revision + 1,
        ..view.clone()
    }))
}

/// Raw SQLite values preserve NULL, metadata bytes, request identity and sort keys.
/// Include every row in order, not just the displayed page or selected track.
fn queue_snapshot(
    conn: &rusqlite::Connection,
    guild: u64,
) -> rusqlite::Result<(String, Vec<Item>)> {
    let mut st = conn.prepare("SELECT id,url,title,details,sort_order FROM queue WHERE guild_id=?1 ORDER BY COALESCE(sort_order,id),id")?;
    let rows = st.query_map([guild as i64], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<i64>>(4)?,
        ))
    })?;
    let mut raw = Vec::new();
    let mut items = Vec::new();
    for row in rows {
        let (id, url, title, details, order) = row?;
        raw.push(json!([id, url, title, details, order]));
        items.push(Item {
            id,
            url,
            title: title.unwrap_or_default(),
            details: details
                .and_then(|d| serde_json::from_str(&d).ok())
                .unwrap_or(Value::Null),
        });
    }
    Ok((json!(raw).to_string(), items))
}

fn short(text: &str, limit: usize) -> String {
    let mut remaining = limit;
    text.chars()
        .map(|c| {
            if c.is_control() || "*_`~[]\\<>".contains(c) {
                ' '
            } else {
                c
            }
        })
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
    current: Option<&str>,
) -> rusqlite::Result<(String, Vec<s::CreateActionRow>)> {
    let conn = db.lock();
    let (snapshot, items) = queue_snapshot(&conn, view.guild)?;
    if view.snapshot != snapshot {
        view.selected = None;
    }
    view.snapshot = snapshot;
    view.page = view.page.min(items.len().saturating_sub(1) / PAGE_SIZE);
    view.selected = view.selected.filter(|id| items.iter().any(|i| i.id == *id));
    conn.execute("UPDATE music_queue_views SET page=?2,snapshot=?3,selected=?4,claimed=0
        WHERE id=?1 AND revision=?5 AND guild_id=?6 AND user_id=?7 AND channel_id=?8 AND message_id=?9",
        params![view.id, view.page as i64, view.snapshot, view.selected, view.revision,
            view.guild as i64, view.user as i64, view.channel as i64, view.message as i64])?;
    let mut text = format!(
        "🎶 Очередь · {} треков · страница {}/{}\n",
        items.len(),
        view.page + 1,
        items.len().saturating_sub(1) / PAGE_SIZE + 1
    );
    if let Some(current) = current {
        text.push_str(&format!("Сейчас: {}\n", short(current, 150)));
    }
    let start = view.page * PAGE_SIZE;
    let page = &items[start..items.len().min(start + PAGE_SIZE)];
    for (offset, item) in page.iter().enumerate() {
        let requester = item.details["requester"]
            .as_u64()
            .map_or("автоплей / старая заявка".into(), |id| {
                format!("<@{id}>")
            });
        text.push_str(&format!(
            "\n{}{}. {} · {} · {}",
            if view.selected == Some(item.id) {
                "▶ "
            } else {
                ""
            },
            start + offset + 1,
            short(&item.title, 75),
            crate::music_qol::duration(item.details["duration"].as_f64()),
            requester
        ));
    }
    if items.is_empty() {
        text.push_str("\nОчередь пуста.");
    }
    let prefix = format!("queue:{}:{}", view.id, view.revision);
    let mut rows = Vec::new();
    if !page.is_empty() {
        let options = page
            .iter()
            .map(|item| {
                let title = short(&item.title, 100);
                s::CreateSelectMenuOption::new(
                    if title.trim().is_empty() {
                        "Без названия".into()
                    } else {
                        title
                    },
                    item.id.to_string(),
                )
                .default_selection(view.selected == Some(item.id))
            })
            .collect();
        rows.push(s::CreateActionRow::SelectMenu(
            s::CreateSelectMenu::new(
                format!("{prefix}:select"),
                s::CreateSelectMenuKind::String { options },
            )
            .placeholder("Выбери трек для удаления или перемещения")
            .min_values(1)
            .max_values(1),
        ));
    }
    rows.push(s::CreateActionRow::Buttons(vec![
        s::CreateButton::new(format!("{prefix}:prev"))
            .label("←")
            .disabled(view.page == 0),
        s::CreateButton::new(format!("{prefix}:next"))
            .label("→")
            .disabled(start + PAGE_SIZE >= items.len()),
        s::CreateButton::new(format!("{prefix}:refresh")).label("Обновить"),
    ]));
    if !items.is_empty() {
        rows.push(s::CreateActionRow::Buttons(
            [
                ("remove", "Удалить"),
                ("up", "Выше"),
                ("down", "Ниже"),
                ("first", "В начало"),
                ("last", "В конец"),
            ]
            .into_iter()
            .map(|(action, label)| {
                s::CreateButton::new(format!("{prefix}:{action}"))
                    .label(label)
                    .disabled(view.selected.is_none())
                    .style(if action == "remove" {
                        s::ButtonStyle::Danger
                    } else {
                        s::ButtonStyle::Secondary
                    })
            })
            .collect(),
        ));
    }
    // Bound the whole message in Discord's UTF-16 units, including all metadata.
    let text = text
        .chars()
        .scan(1900usize, |remaining, c| {
            if c.len_utf16() > *remaining {
                return None;
            }
            *remaining -= c.len_utf16();
            Some(c)
        })
        .collect();
    Ok((text, rows))
}

fn edit(
    db: &db::Database,
    view: &View,
    action: &str,
    moderator: bool,
    same_voice: bool,
    now: f64,
) -> rusqlite::Result<&'static str> {
    let mut conn = db.lock();
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let live = tx
        .query_row(
            "SELECT snapshot,selected FROM music_queue_views WHERE id=?1 AND guild_id=?2
        AND user_id=?3 AND channel_id=?4 AND message_id=?5 AND revision=?6 AND expires>?7
        AND claimed=1 AND spent_revision<>revision",
            params![
                view.id,
                view.guild as i64,
                view.user as i64,
                view.channel as i64,
                view.message as i64,
                view.revision,
                now
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)),
        )
        .optional()?;
    let Some((saved, selected)) = live else {
        return Ok("stale");
    };
    let (snapshot, mut items) = queue_snapshot(&tx, view.guild)?;
    if snapshot != saved || saved != view.snapshot || selected != view.selected {
        return Ok("stale");
    }
    let Some(index) = items.iter().position(|i| Some(i.id) == selected) else {
        return Ok("stale");
    };
    if !same_voice {
        return Ok("voice");
    }
    if !moderator && items[index].details["requester"].as_u64() != Some(view.user) {
        return Ok("permission");
    }
    if !matches!(action, "remove" | "up" | "down" | "first" | "last") {
        return Ok("stale");
    }
    // Spending the revision and changing the exact snapshot commit together.
    if tx.execute("UPDATE music_queue_views SET spent_revision=revision WHERE id=?1 AND revision=?2 AND claimed=1 AND spent_revision<>revision",
        params![view.id, view.revision])? != 1 { return Ok("stale") }
    let item = items.remove(index);
    if action == "remove" {
        tx.execute(
            "DELETE FROM queue WHERE guild_id=?1 AND id=?2",
            params![view.guild as i64, item.id],
        )?;
    } else {
        let target = match action {
            "up" => index.saturating_sub(1),
            "down" => (index + 1).min(items.len()),
            "first" => 0,
            _ => items.len(),
        };
        items.insert(target, item);
        for (position, item) in items.iter().enumerate() {
            tx.execute(
                "UPDATE queue SET sort_order=?1 WHERE guild_id=?2 AND id=?3",
                params![position as i64 + 1, view.guild as i64, item.id],
            )?;
        }
    }
    tx.commit()?;
    Ok("changed")
}

pub async fn show(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer().await?;
    let guild = ctx
        .guild_id()
        .ok_or("Очередь доступна только на сервере.")?
        .get();
    let mut view = new_view(
        &ctx.data().db,
        guild,
        ctx.author().id.get(),
        ctx.channel_id().get(),
        db::now(),
    )?;
    let current = ctx.data().music.with(guild, |st| {
        st.now_playing.as_ref().map(|(_, title)| title.clone())
    });
    let (text, rows) = render(&ctx.data().db, &mut view, current.as_deref())?;
    let reply = ctx
        .send(
            poise::CreateReply::default()
                .content(text)
                .components(rows)
                .allowed_mentions(s::CreateAllowedMentions::new()),
        )
        .await?;
    let message = reply.message().await?.id.get();
    ctx.data().db.lock().execute(
        "UPDATE music_queue_views SET message_id=?2 WHERE id=?1 AND message_id=0",
        params![view.id, message as i64],
    )?;
    Ok(())
}

/// Coordinator has already applied music_channels::button_check.
pub async fn pressed(ctx: &s::Context, data: &Shared, press: &s::ComponentInteraction) -> bool {
    if !press.data.custom_id.starts_with("queue:") {
        return false;
    }
    let parts: Vec<_> = press.data.custom_id.split(':').collect();
    let parsed = match parts.as_slice() {
        ["queue", id, revision, action]
            if id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            revision
                .parse::<i64>()
                .ok()
                .filter(|r| *r >= 0)
                .and_then(|revision| {
                    let valid_kind = match (&press.data.kind, *action) {
                        (s::ComponentInteractionDataKind::StringSelect { values }, "select") => {
                            values.len() == 1 && values[0].parse::<i64>().is_ok()
                        }
                        (
                            s::ComponentInteractionDataKind::Button,
                            "prev" | "next" | "refresh" | "remove" | "up" | "down" | "first"
                            | "last",
                        ) => true,
                        _ => false,
                    };
                    if valid_kind {
                        Some((*id, revision, *action))
                    } else {
                        None
                    }
                })
        }
        _ => None,
    };
    let claimed = parsed
        .and_then(|(id, revision, _)| press.guild_id.map(|guild| (id, revision, guild)))
        .and_then(|(id, revision, guild)| {
            let candidate = View {
                id: id.into(),
                revision,
                guild: guild.get(),
                user: press.user.id.get(),
                channel: press.channel_id.get(),
                message: press.message.id.get(),
                page: 0,
                snapshot: String::new(),
                selected: None,
            };
            match claim_view(&data.db, &candidate, db::now()) {
                Ok(view) => view,
                Err(error) => {
                    eprintln!("Очередь: {error}");
                    None
                }
            }
        });
    let Some(mut view) = claimed else {
        let _ = press
            .create_response(
                ctx,
                s::CreateInteractionResponse::Message(
                    s::CreateInteractionResponseMessage::new()
                        .content("Чужая или устаревшая кнопка. Открой /queue заново.")
                        .ephemeral(true)
                        .allowed_mentions(s::CreateAllowedMentions::new()),
                ),
            )
            .await;
        return true;
    };
    // Type 6 only for the opener's live message, before queue reads or edits.
    if press
        .create_response(ctx, s::CreateInteractionResponse::Acknowledge)
        .await
        .is_err()
    {
        return true;
    }
    let action = parsed.unwrap().2;
    if let Err(error) = handle_press(ctx, data, press, &mut view, action).await {
        eprintln!(
            "Очередь: {}",
            crate::interactions::sanitize_error(&error.to_string())
        );
        let _ = press
            .create_followup(
                ctx,
                s::CreateInteractionResponseFollowup::new()
                    .content("Не удалось обновить очередь. Открой /queue заново.")
                    .ephemeral(true)
                    .allowed_mentions(s::CreateAllowedMentions::new()),
            )
            .await;
    }
    true
}

async fn handle_press(
    ctx: &s::Context,
    data: &Shared,
    press: &s::ComponentInteraction,
    view: &mut View,
    action: &str,
) -> Result<(), Error> {
    let mut notice = "";
    match action {
        "prev" => view.page = view.page.saturating_sub(1),
        "next" => view.page = view.page.saturating_add(1),
        "refresh" => {}
        "select" => {
            if let s::ComponentInteractionDataKind::StringSelect { values } = &press.data.kind {
                let id = values[0].parse::<i64>()?;
                let rows: Vec<Value> = serde_json::from_str(&view.snapshot)?;
                let valid = rows
                    .iter()
                    .skip(view.page.saturating_mul(PAGE_SIZE))
                    .take(PAGE_SIZE)
                    .any(|row| row[0].as_i64() == Some(id));
                if valid {
                    view.selected = Some(id);
                } else {
                    notice = "Выбор устарел. Выбери трек снова.\n";
                }
            }
        }
        _ => {
            let guild = s::GuildId::new(view.guild);
            let same_voice = ctx.cache.guild(guild).is_some_and(|g| {
                let user = g
                    .voice_states
                    .get(&press.user.id)
                    .and_then(|v| v.channel_id);
                let bot = g
                    .voice_states
                    .get(&ctx.cache.current_user().id)
                    .and_then(|v| v.channel_id);
                user.is_some() && user == bot
            });
            let moderator = press.member.as_ref().is_some_and(|member| {
                crate::moderation::permissions(ctx, guild, member).manage_guild()
            });
            notice = match edit(&data.db, view, action, moderator, same_voice, db::now())? {
                "changed" => {
                    crate::music_runtime::queue_changed(data, view.guild);
                    "✅ Очередь обновлена.\n"
                }
                "voice" => "Зайди в голосовой канал бота.\n",
                "permission" => {
                    "Можно менять свои заявки; чужие — с правом «Управлять сервером».\n"
                }
                _ => "Очередь изменилась. Выбери трек снова.\n",
            };
        }
    }
    let current = data.music.with(view.guild, |st| {
        st.now_playing.as_ref().map(|(_, title)| title.clone())
    });
    let (text, rows) = render(&data.db, view, current.as_deref())?;
    let content: String = format!("{notice}{text}")
        .chars()
        .scan(1900usize, |remaining, c| {
            if c.len_utf16() > *remaining {
                return None;
            }
            *remaining -= c.len_utf16();
            Some(c)
        })
        .collect();
    press
        .edit_response(
            ctx,
            s::EditInteractionResponse::new()
                .content(content)
                .components(rows)
                .allowed_mentions(s::CreateAllowedMentions::new()),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> crate::Data {
        let db = db::Database::in_memory().unwrap();
        prepare(&db).unwrap();
        prepare(&db).unwrap();
        crate::Data::new(
            db,
            &crate::config::Settings::default(),
            Default::default(),
            false,
        )
    }
    fn add(data: &crate::Data, guild: u64, title: &str, user: u64, front: bool) {
        crate::music_qol::enqueue(
            data,
            guild,
            title,
            title,
            &json!({"requester": user, "duration": 61}),
            front,
        )
        .unwrap();
    }
    fn view(data: &crate::Data, guild: u64, user: u64) -> View {
        let mut v = new_view(&data.db, guild, user, 7, 100.).unwrap();
        v.message = 8;
        data.db
            .lock()
            .execute(
                "UPDATE music_queue_views SET message_id=8 WHERE id=?1",
                [&v.id],
            )
            .unwrap();
        render(&data.db, &mut v, None).unwrap();
        v
    }
    fn selected(data: &crate::Data, guild: u64, user: u64, index: usize) -> View {
        let mut v = view(data, guild, user);
        v.selected = Some(crate::music_qol::items(data, guild).unwrap()[index].id);
        render(&data.db, &mut v, None).unwrap();
        v
    }
    fn titles(data: &crate::Data, guild: u64) -> Vec<String> {
        crate::music_qol::items(data, guild)
            .unwrap()
            .into_iter()
            .map(|i| i.title)
            .collect()
    }

    #[test]
    fn failed_move_rolls_back_order_and_revision_spend() {
        let d = data();
        for title in ["a", "b", "c"] {
            add(&d, 1, title, 10, false);
        }
        let v = selected(&d, 1, 10, 0);
        let v = claim_view(&d.db, &v, 101.).unwrap().unwrap();
        d.db.lock().execute_batch("CREATE TRIGGER reject_move BEFORE UPDATE OF sort_order ON queue WHEN NEW.title='c' BEGIN SELECT RAISE(ABORT,'reject'); END;").unwrap();
        assert!(edit(&d.db, &v, "last", false, true, 101.).is_err());
        assert_eq!(titles(&d, 1), ["a", "b", "c"]);
        d.db.lock()
            .execute_batch("DROP TRIGGER reject_move")
            .unwrap();
        assert_eq!(
            edit(&d.db, &v, "last", false, true, 101.).unwrap(),
            "changed"
        );
        assert_eq!(titles(&d, 1), ["b", "c", "a"]);
    }

    #[test]
    fn duplicate_selection_removes_exact_request_and_boundary_move_is_spent() {
        let d = data();
        add(&d, 1, "same", 10, false);
        add(&d, 1, "same", 10, false);
        let requests = crate::music_qol::items(&d, 1).unwrap();
        assert_ne!(
            requests[0].details["request_id"],
            requests[1].details["request_id"]
        );
        let v = selected(&d, 1, 10, 1);
        let v = claim_view(&d.db, &v, 101.).unwrap().unwrap();
        assert_eq!(
            edit(&d.db, &v, "remove", false, true, 101.).unwrap(),
            "changed"
        );
        let remaining = crate::music_qol::items(&d, 1).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, requests[0].id);
        assert_eq!(remaining[0].details, requests[0].details);
        let v = selected(&d, 1, 10, 0);
        let v = claim_view(&d.db, &v, 101.).unwrap().unwrap();
        assert_eq!(
            edit(&d.db, &v, "first", false, true, 101.).unwrap(),
            "changed"
        );
        assert_eq!(
            edit(&d.db, &v, "first", false, true, 101.).unwrap(),
            "stale"
        );
    }

    #[test]
    fn unclaimed_or_substituted_selection_cannot_mutate() {
        let d = data();
        add(&d, 1, "a", 10, false);
        add(&d, 1, "b", 10, false);
        let v = selected(&d, 1, 10, 0);
        assert_eq!(
            edit(&d.db, &v, "remove", false, true, 101.).unwrap(),
            "stale"
        );
        let v = claim_view(&d.db, &v, 101.).unwrap().unwrap();
        let mut substituted = v.clone();
        substituted.selected = Some(crate::music_qol::items(&d, 1).unwrap()[1].id);
        assert_eq!(
            edit(&d.db, &substituted, "remove", true, true, 101.).unwrap(),
            "stale"
        );
        assert_eq!(
            edit(&d.db, &v, "remove", true, true, 1000.).unwrap(),
            "stale"
        );
        assert_eq!(titles(&d, 1), ["a", "b"]);
        assert_eq!(
            edit(&d.db, &v, "remove", false, true, 101.).unwrap(),
            "changed"
        );
        assert_eq!(titles(&d, 1), ["b"]);
    }

    // Without owner/guild/message predicates a foreign callback could update another UI.
    #[test]
    fn views_isolate_owner_guild_channel_message_expiry_and_replays() {
        let d = data();
        let v = view(&d, 1, 10);
        for change in 0..4 {
            let mut foreign = v.clone();
            match change {
                0 => foreign.user = 11,
                1 => foreign.guild = 2,
                2 => foreign.channel = 9,
                _ => foreign.message = 9,
            }
            assert!(claim_view(&d.db, &foreign, 101.).unwrap().is_none());
        }
        assert!(claim_view(&d.db, &v, 1000.).unwrap().is_none());
        assert!(claim_view(&d.db, &v, 101.).unwrap().is_some());
        assert!(claim_view(&d.db, &v, 101.).unwrap().is_none());
        let independent = data();
        assert!(claim_view(&independent.db, &v, 101.).unwrap().is_none());
    }

    #[test]
    fn removal_enforces_voice_requester_and_manage_guild() {
        let d = data();
        add(&d, 1, "own", 10, false);
        add(&d, 1, "other", 11, false);
        add(&d, 2, "isolated", 10, false);
        let v = selected(&d, 1, 10, 1);
        let v = claim_view(&d.db, &v, 101.).unwrap().unwrap();
        assert_eq!(
            edit(&d.db, &v, "remove", true, false, 101.).unwrap(),
            "voice"
        );
        assert_eq!(
            edit(&d.db, &v, "remove", false, true, 101.).unwrap(),
            "permission"
        );
        assert_eq!(
            edit(&d.db, &v, "remove", true, true, 101.).unwrap(),
            "changed"
        );
        assert_eq!(
            edit(&d.db, &v, "remove", true, true, 101.).unwrap(),
            "stale"
        );
        assert_eq!(titles(&d, 1), ["own"]);
        assert_eq!(titles(&d, 2), ["isolated"]);
        let v = selected(&d, 1, 10, 0);
        let v = claim_view(&d.db, &v, 101.).unwrap().unwrap();
        assert_eq!(
            edit(&d.db, &v, "remove", false, true, 101.).unwrap(),
            "changed"
        );
        assert!(titles(&d, 1).is_empty());
    }

    #[test]
    fn stale_membership_order_and_reinserted_id_never_edit() {
        for mutation in ["insert", "delete", "reorder", "replace", "metadata"] {
            let d = data();
            add(&d, 1, "a", 10, false);
            add(&d, 1, "b", 10, false);
            let v = selected(&d, 1, 10, 0);
            let id = v.selected.unwrap();
            match mutation {
                "insert" => add(&d, 1, "new", 10, false),
                "delete" => {
                    d.db.lock()
                        .execute("DELETE FROM queue WHERE title='b'", [])
                        .unwrap();
                }
                "reorder" => {
                    d.db.lock()
                        .execute("UPDATE queue SET sort_order=-1 WHERE title='b'", [])
                        .unwrap();
                }
                "metadata" => {
                    d.db.lock()
                        .execute("UPDATE queue SET title='changed' WHERE id=?1", [id])
                        .unwrap();
                }
                _ => {
                    let conn = d.db.lock();
                    conn.execute("DELETE FROM queue WHERE id=?1", [id]).unwrap();
                    conn.execute("INSERT INTO queue(id,guild_id,url,title,details,sort_order) VALUES(?1,1,'a','a',?2,1)", params![id, json!({"requester":10,"duration":61,"request_id":999}).to_string()]).unwrap();
                }
            }
            let before = titles(&d, 1);
            let v = claim_view(&d.db, &v, 101.).unwrap().unwrap();
            assert_eq!(
                edit(&d.db, &v, "last", false, true, 101.).unwrap(),
                "stale",
                "{mutation}"
            );
            assert_eq!(titles(&d, 1), before);
        }
    }

    #[test]
    fn moves_persist_order_and_new_front_back_insertions() {
        let d = data();
        for title in ["a", "b", "c"] {
            add(&d, 1, title, 10, false);
        }
        for (index, action, want) in [
            (0, "last", vec!["b", "c", "a"]),
            (2, "up", vec!["b", "a", "c"]),
            (1, "first", vec!["a", "b", "c"]),
            (0, "down", vec!["b", "a", "c"]),
        ] {
            let v = selected(&d, 1, 10, index);
            let v = claim_view(&d.db, &v, 101.).unwrap().unwrap();
            assert_eq!(
                edit(&d.db, &v, action, false, true, 101.).unwrap(),
                "changed"
            );
            assert_eq!(titles(&d, 1), want);
            assert_eq!(edit(&d.db, &v, action, false, true, 101.).unwrap(), "stale");
        }
        add(&d, 1, "front", 10, true);
        add(&d, 1, "back", 10, false);
        assert_eq!(titles(&d, 1), ["front", "b", "a", "c", "back"]);
        let v = selected(&d, 1, 11, 0);
        let v = claim_view(&d.db, &v, 101.).unwrap().unwrap();
        assert_eq!(
            edit(&d.db, &v, "last", false, true, 101.).unwrap(),
            "permission"
        );
        assert_eq!(
            edit(&d.db, &v, "last", true, true, 101.).unwrap(),
            "changed"
        );
    }

    #[test]
    fn concurrent_claims_and_edits_have_exactly_one_winner() {
        let d = std::sync::Arc::new(data());
        add(&d, 1, "a", 10, false);
        add(&d, 1, "b", 10, false);
        let v = selected(&d, 1, 10, 0);
        let winners = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let d = &d;
                    let v = &v;
                    scope.spawn(move || claim_view(&d.db, v, 101.).unwrap().is_some() as usize)
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .sum::<usize>()
        });
        assert_eq!(winners, 1);
        // Independent views can claim the same snapshot, but only one edit can commit.
        let a = selected(&d, 1, 10, 0);
        let b = selected(&d, 1, 10, 0);
        let a = claim_view(&d.db, &a, 101.).unwrap().unwrap();
        let b = claim_view(&d.db, &b, 101.).unwrap().unwrap();
        let results = std::thread::scope(|scope| {
            let x = scope.spawn(|| edit(&d.db, &a, "last", false, true, 101.).unwrap());
            let y = scope.spawn(|| edit(&d.db, &b, "last", false, true, 101.).unwrap());
            [x.join().unwrap(), y.join().unwrap()]
        });
        assert_eq!(results.iter().filter(|r| **r == "changed").count(), 1);
        assert_eq!(titles(&d, 1), ["b", "a"]);
    }

    #[test]
    fn unicode_limits_full_pagination_and_empty_page_clamping() {
        let d = data();
        for n in 0..23 {
            add(
                &d,
                1,
                &format!("{n}{}", "🎵*\n".repeat(1000)),
                u64::MAX,
                false,
            );
        }
        let mut v = view(&d, 1, 10);
        let mut seen = vec![];
        for page in 0..3 {
            v.page = page;
            let (text, rows) = render(&d.db, &mut v, Some(&"😀".repeat(3000))).unwrap();
            assert!(text.encode_utf16().count() <= 1900);
            let payload = serde_json::to_value(rows).unwrap();
            let options = payload[0]["components"][0]["options"].as_array().unwrap();
            assert_eq!(options.len(), if page == 2 { 3 } else { 10 });
            for o in options {
                assert!((1..=100).contains(&o["label"].as_str().unwrap().encode_utf16().count()));
                seen.push(o["value"].as_str().unwrap().to_owned());
            }
            for row in payload.as_array().unwrap() {
                assert!(row["components"].as_array().unwrap().len() <= 5);
            }
        }
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 23);
        v.page = usize::MAX;
        render(&d.db, &mut v, None).unwrap();
        assert_eq!(v.page, 2);
        d.db.lock().execute("DELETE FROM queue", []).unwrap();
        let (text, _) = render(&d.db, &mut v, None).unwrap();
        assert_eq!(v.page, 0);
        assert!(text.contains("пуста"));
        assert!(v.selected.is_none());
    }
}
