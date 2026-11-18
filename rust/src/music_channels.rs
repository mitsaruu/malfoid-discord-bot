//! Music channel restriction. Run both gates before any defer or handler response.
//!
//! `match:` buttons belong to the choose collector: call `button_check` there,
//! never also in the global component handler (a denial is already acknowledged).

use crate::db::Database;
use crate::{Context, Data, Error};
use poise::serenity_prelude as serenity;
use rusqlite::{params, OptionalExtension};

pub fn prepare(db: &Database) -> rusqlite::Result<()> {
    db.lock().execute_batch(
        "CREATE TABLE IF NOT EXISTS music_channel (
            guild_id INTEGER PRIMARY KEY,
            channel_id INTEGER NOT NULL
        );",
    )
}

pub fn channel(db: &Database, guild: u64) -> rusqlite::Result<Option<u64>> {
    db.lock()
        .query_row(
            "SELECT channel_id FROM music_channel WHERE guild_id=?1",
            [guild as i64],
            |row| row.get::<_, i64>(0).map(|id| id as u64),
        )
        .optional()
}

fn set_channel(db: &Database, guild: u64, channel: Option<u64>) -> rusqlite::Result<()> {
    let db = db.lock();
    if let Some(channel) = channel {
        db.execute(
            "INSERT INTO music_channel(guild_id,channel_id) VALUES (?1,?2)
             ON CONFLICT(guild_id) DO UPDATE SET channel_id=excluded.channel_id",
            params![guild as i64, channel as i64],
        )?;
    } else {
        db.execute(
            "DELETE FROM music_channel WHERE guild_id=?1",
            [guild as i64],
        )?;
    }
    Ok(())
}

/// Allow the configured channel itself, including when it has a parent category,
/// and threads whose parent is that channel.
pub fn allowed(configured: Option<u64>, id: u64, parent: Option<u64>) -> bool {
    configured.is_none_or(|configured| id == configured || parent == Some(configured))
}

/// Skip the legacy games gate for these names, including `top_chart_month`.
pub fn is_music_command(name: &str) -> bool {
    matches!(
        name,
        "play"
            | "skip"
            | "pause"
            | "resume"
            | "queue"
            | "remove_all"
            | "setvolume"
            | "leave"
            | "autoplay"
            | "playlist_save"
            | "playlist_play"
            | "playlist_delete"
            | "playlists"
            | "lyrics"
            | "top_chart_month"
            | "nowplaying"
            | "queue_remove"
            | "queue_move"
            | "queue_clear_mine"
            | "favorites"
            | "history"
    )
}

pub fn is_music_button(custom_id: &str) -> bool {
    ["music:", "library:", "queue:", "version:", "match:"]
        .iter()
        .any(|prefix| custom_id.starts_with(prefix))
}

fn denial(channel: u64) -> String {
    format!("🎵 Музыкальные команды и кнопки работают только в <#{channel}>.")
}

fn ctx_restriction(ctx: Context<'_>) -> rusqlite::Result<Option<u64>> {
    let Some(guild) = ctx.guild_id() else { return Ok(None) };
    let parent = match ctx {
        poise::Context::Application(app) => app.interaction.channel.as_ref().and_then(|c| c.parent_id),
        poise::Context::Prefix(_) => ctx.guild().and_then(|guild| {
            guild.channels.get(&ctx.channel_id()).and_then(|c| c.parent_id)
        }),
    }.map(|id| id.get());
    restriction(ctx.data(), guild.get(), ctx.channel_id().get(), parent)
}

pub(crate) fn restriction(
    data: &Data,
    guild: u64,
    id: u64,
    parent: Option<u64>,
) -> rusqlite::Result<Option<u64>> {
    Ok(channel(&data.db, guild)?.filter(|configured| !allowed(Some(*configured), id, parent)))
}

#[cfg(feature = "voice")]
pub(crate) fn ctx_allowed(ctx: Context<'_>) -> rusqlite::Result<bool> {
    Ok(ctx_restriction(ctx)?.is_none())
}

#[cfg(feature = "voice")]
pub(crate) fn press_allowed(data: &Data, press: &serenity::ComponentInteraction) -> rusqlite::Result<bool> {
    let Some(guild) = press.guild_id else { return Ok(false) };
    let parent = press.channel.as_ref().and_then(|c| c.parent_id).map(|id| id.get());
    Ok(restriction(data, guild.get(), press.channel_id.get(), parent)?.is_none())
}

/// Read only the interaction payload/cache before the initial response; no HTTP lookup.
pub async fn command_check(ctx: Context<'_>) -> Result<bool, Error> {
    if !is_music_command(&ctx.command().name) {
        return Ok(true);
    }
    let Some(configured) = ctx_restriction(ctx)? else { return Ok(true) };
    crate::reply::whisper(ctx, denial(configured)).await?;
    Ok(false)
}

/// True leaves the interaction untouched. False sends a private type-4 response.
/// The caller must stop handling a denied press to avoid a second acknowledgement.
pub async fn button_check(
    ctx: &serenity::Context,
    data: &Data,
    press: &serenity::ComponentInteraction,
) -> Result<bool, Error> {
    if !is_music_button(&press.data.custom_id) {
        return Ok(true);
    }
    let Some(guild) = press.guild_id else {
        return Ok(true);
    };
    let Some(configured) = channel(&data.db, guild.get())? else {
        return Ok(true);
    };
    let parent = press
        .channel
        .as_ref()
        .and_then(|c| c.parent_id)
        .map(|id| id.get());
    if allowed(Some(configured), press.channel_id.get(), parent) {
        return Ok(true);
    }
    press
        .create_response(
            ctx,
            serenity::CreateInteractionResponse::Message(
                serenity::CreateInteractionResponseMessage::new()
                    .content(denial(configured))
                    .ephemeral(true)
                    .allowed_mentions(serenity::CreateAllowedMentions::new()),
            ),
        )
        .await?;
    Ok(false)
}

/// Канал для музыкальных команд и кнопок (пусто — работают везде)
#[poise::command(
    slash_command,
    guild_only,
    default_member_permissions = "MANAGE_GUILD",
    required_permissions = "MANAGE_GUILD",
    rename = "music_channel"
)]
pub async fn set_music_channel(
    ctx: Context<'_>,
    #[description = "Текстовый канал для музыки (не выбирай, чтобы разрешить музыку везде)"]
    #[channel_types("Text")]
    channel: Option<serenity::GuildChannel>,
) -> Result<(), Error> {
    let guild = ctx.guild_id().unwrap().get();
    let Some(channel) = channel else {
        set_channel(&ctx.data().db, guild, None)?;
        return crate::reply::whisper(
            ctx,
            "✅ Музыкальные команды и кнопки снова работают в любом канале.",
        )
        .await;
    };
    if channel.guild_id.get() != guild || channel.kind != serenity::ChannelType::Text {
        return crate::reply::whisper(ctx, "Выбери текстовый канал этого сервера.").await;
    }
    let permitted = crate::admin::bot_permissions(ctx, &channel)
        .is_some_and(|p| p.view_channel() && p.send_messages() && p.embed_links());
    if !permitted {
        return crate::reply::whisper(ctx, format!(
            "Мне не хватает прав в <#{}>: нужны «Просматривать канал», «Отправлять сообщения» и «Встраивать ссылки».",
            channel.id,
        )).await;
    }
    set_channel(&ctx.data().db, guild, Some(channel.id.get()))?;
    crate::reply::whisper(ctx, format!(
        "✅ Музыкальные команды и кнопки теперь работают только в <#{}> и его ветках. Чтобы разрешить их везде — вызови /music_channel без канала.",
        channel.id,
    )).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_settings_require_manage_guild_even_if_command_is_exposed() {
        let command = set_music_channel();
        assert!(command.guild_only);
        assert_eq!(
            command.default_member_permissions,
            serenity::Permissions::MANAGE_GUILD
        );
        assert_eq!(
            command.required_permissions,
            serenity::Permissions::MANAGE_GUILD
        );
    }

    #[test]
    fn unrestricted_and_configured_channels() {
        assert!(allowed(None, 5, None));
        assert!(allowed(None, 5, Some(7)));
        assert!(allowed(Some(9), 9, None));
        assert!(!allowed(Some(9), 5, None));
    }

    #[test]
    fn configured_channel_or_its_threads_are_allowed() {
        assert!(allowed(Some(9), 5, Some(9)));
        assert!(!allowed(Some(9), 5, Some(7)));
        // A text channel can itself have a parent category.
        assert!(allowed(Some(9), 9, Some(7)));
    }

    #[test]
    fn classifies_all_music_commands_including_legacy_games_overlap() {
        let names = [
            "play",
            "skip",
            "pause",
            "resume",
            "queue",
            "remove_all",
            "setvolume",
            "leave",
            "autoplay",
            "playlist_save",
            "playlist_play",
            "playlist_delete",
            "playlists",
            "lyrics",
            "top_chart_month",
            "nowplaying",
            "queue_remove",
            "queue_move",
            "queue_clear_mine",
            "favorites",
            "history",
        ];
        for name in names {
            assert!(is_music_command(name), "missing music command: {name}");
        }
        for name in [
            "music_channel",
            "games_channel",
            "rank",
            "play_extra",
            "",
            "Play",
        ] {
            assert!(!is_music_command(name), "unexpected music command: {name}");
        }
        // The coordinator skips the games gate for this command, even without a restriction.
        assert!(is_music_command("top_chart_month"));
        assert!(allowed(None, 5, None));
        assert!(allowed(Some(9), 9, None));
    }

    #[test]
    fn classifies_music_buttons_including_collector_owned_matches() {
        for id in [
            "music:skip",
            "library:view:1:0:next",
            "queue:next",
            "version:1:0",
            "match:cancel",
        ] {
            assert!(is_music_button(id), "missing music button: {id}");
        }
        for id in ["music", "other:music:skip", "challenge:accept", ""] {
            assert!(!is_music_button(id));
        }
    }

    #[test]
    fn settings_are_isolated_by_guild_and_can_be_cleared() {
        let db = Database::in_memory().unwrap();
        prepare(&db).unwrap();
        prepare(&db).unwrap();
        assert_eq!(channel(&db, 1).unwrap(), None);
        set_channel(&db, 1, Some(9)).unwrap();
        set_channel(&db, 2, Some(7)).unwrap();
        assert_eq!(channel(&db, 1).unwrap(), Some(9));
        assert_eq!(channel(&db, 2).unwrap(), Some(7));
        assert!(allowed(channel(&db, 1).unwrap(), 9, None));
        assert!(!allowed(channel(&db, 2).unwrap(), 9, None));
        set_channel(&db, 1, Some(11)).unwrap();
        assert_eq!(channel(&db, 1).unwrap(), Some(11));
        set_channel(&db, 1, None).unwrap();
        assert_eq!(channel(&db, 1).unwrap(), None);
        assert_eq!(channel(&db, 2).unwrap(), Some(7));
    }
}
