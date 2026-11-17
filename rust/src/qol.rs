//! Справка, общий экран настроек и подтверждение действий.
use crate::{Context, Error, Shared};
use poise::serenity_prelude as s;

pub async fn confirm(ctx: Context<'_>, text: impl Into<String>) -> Result<bool, Error> {
    let accepted = crate::reply::challenge(
        ctx,
        ctx.author().id,
        format!("⚠️ {}\nПодтверди действие кнопкой «Принять».", text.into()),
    )
    .await?;
    if let Some(press) = accepted {
        crate::reply::finish_challenge(ctx, &press, "✅ Подтверждено.".into()).await?;
        Ok(true)
    } else {
        Ok(false)
    }
}

const SECTIONS: [&str; 5] = [
    "Музыка",
    "Экономика и питомцы",
    "Слова и игры",
    "ИИ",
    "Сервер",
];
fn section(name: &str) -> usize {
    if matches!(
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
            | "playlists"
            | "playlist_delete"
            | "lyrics"
            | "top_chart_month"
            | "nowplaying"
            | "queue_remove"
            | "queue_move"
            | "queue_clear_mine"
            | "favorites"
            | "history"
    ) {
        0
    } else if name.starts_with("pet")
        || name.starts_with("shop")
        || matches!(
            name,
            "work"
                | "balance"
                | "daily"
                | "give_money"
                | "coinflip"
                | "slots"
                | "blackjack"
                | "case"
                | "cases"
                | "money_leaderboard"
        )
    {
        1
    } else if name.starts_with("word")
        || name.starts_with("dick")
        || matches!(
            name,
            "wort" | "slovo" | "dice" | "roll" | "predskazanie" | "shar" | "rank" | "leaderboard"
        )
    {
        2
    } else if matches!(name, "ask" | "summary" | "quiz" | "forget" | "ai_limits") {
        3
    } else {
        4
    }
}
fn help_embed(commands: &[poise::Command<Shared, Error>], page: usize) -> s::CreateEmbed {
    let mut lines: Vec<String> = commands
        .iter()
        .filter(|cmd| section(&cmd.name) == page)
        .map(|cmd| {
            format!(
                "**/{}** — {}",
                cmd.name,
                cmd.description.as_deref().unwrap_or("")
            )
        })
        .collect();
    lines.sort();
    let example = [
        "`/play название` · `/queue` — страницы и кнопки управления",
        "`/daily` · `/pets` · `/shop`",
        "`/words` · `/wordstats`",
        "`/ask question:объясни закон Ома model:Hydrogen thinking:high` · `/ai_limits`",
        "`/music_channel channel:#музыка` · `/status` · `/server_settings`",
    ][page];
    s::CreateEmbed::new()
        .title(format!("Справка · {}", SECTIONS[page]))
        .description(lines.join("\n"))
        .color(0x8B5CF6)
        .field("Примеры", example, false)
}
fn help_buttons(user: s::UserId) -> Vec<s::CreateActionRow> {
    vec![s::CreateActionRow::Buttons(
        SECTIONS
            .iter()
            .enumerate()
            .map(|(i, label)| {
                s::CreateButton::new(format!("help:{}:{i}", user.get())).label(*label)
            })
            .collect(),
    )]
}
/// Все команды бота по разделам с примерами
#[poise::command(slash_command, ephemeral)]
pub async fn help(ctx: Context<'_>) -> Result<(), Error> {
    ctx.send(
        poise::CreateReply::default()
            .embed(help_embed(&ctx.framework().options().commands, 0))
            .components(help_buttons(ctx.author().id))
            .ephemeral(true),
    )
    .await?;
    Ok(())
}
pub async fn help_pressed(
    ctx: &s::Context,
    press: &s::ComponentInteraction,
    commands: &[poise::Command<Shared, Error>],
) -> bool {
    let Some(rest) = press.data.custom_id.strip_prefix("help:") else {
        return false;
    };
    let Some((user, page)) = rest.split_once(':') else {
        return true;
    };
    let page = page.parse::<usize>().unwrap_or(usize::MAX);
    if user.parse::<u64>().ok() != Some(press.user.id.get()) || page >= SECTIONS.len() {
        let _ = press
            .create_response(
                ctx,
                s::CreateInteractionResponse::Message(
                    s::CreateInteractionResponseMessage::new()
                        .content("Открой свою справку: /help")
                        .ephemeral(true),
                ),
            )
            .await;
    } else {
        let _ = press
            .create_response(
                ctx,
                s::CreateInteractionResponse::UpdateMessage(
                    s::CreateInteractionResponseMessage::new().embed(help_embed(commands, page)),
                ),
            )
            .await;
    }
    true
}

/// Каналы, функции и права бота; настройка автовыхода из пустого голосового канала
#[poise::command(
    slash_command,
    guild_only,
    ephemeral,
    default_member_permissions = "MANAGE_GUILD"
)]
pub async fn server_settings(
    ctx: Context<'_>,
    #[description = "Через сколько минут выходить из пустого канала (0 — выключить)"]
    #[min = 0]
    #[max = 60]
    empty_minutes: Option<i64>,
) -> Result<(), Error> {
    let guild = ctx.guild_id().unwrap();
    let data = ctx.data();
    if let Some(minutes) = empty_minutes {
        data.db.lock().execute("INSERT INTO music_preferences(guild_id,empty_seconds) VALUES(?1,?2) ON CONFLICT(guild_id) DO UPDATE SET empty_seconds=excluded.empty_seconds",rusqlite::params![guild.get() as i64,minutes*60])?;
    }
    let channel = |id: Option<u64>| id.map_or("выключен".into(), |id| format!("<#{}>", id));
    let daily = data
        .db
        .daily_settings()?
        .into_iter()
        .find(|row| row.guild == guild.get());
    let patch: Option<u64> = data
        .db
        .lock()
        .query_row(
            "SELECT channel_id FROM patchnotes_channel WHERE guild_id=?1",
            [guild.get() as i64],
            |r| r.get::<_, i64>(0).map(|id| id as u64),
        )
        .ok();
    let settings = crate::config::Settings::load(&data.dir.join("config/settings.ini"));
    let mut text=format!("Игры: {}\nМузыка: {}\nЛоги: {}\nПатчноуты: {}\nУтро/вечер: {}\n\nИИ: {}\nМини-приложение: {}\nАнти-спам: {}",
        channel(data.db.games_channel(guild.get())?),channel(crate::music_channels::channel(&data.db,guild.get())?),channel(data.db.log_channel(guild.get())?),channel(patch),
        daily.map_or("выключены".into(),|r|format!("<#{}>, {} / {} ({})",r.channel,r.morning,r.evening,r.timezone)),
        data.ai.ready(),data.miniapp_ready,settings.text("antispam","enabled","on"));
    #[cfg(feature = "voice")]
    {
        text.push_str(&format!(
            "\nАвтоплей: {}\nВыход из пустого канала: {} мин.",
            data.db.autoplay(guild.get())?,
            crate::music_qol::empty_seconds(data, guild.get()) / 60
        ));
    }
    let perms = match ctx {
        poise::Context::Application(app) => app.interaction.app_permissions,
        _ => None,
    };
    let missing: Vec<&str> = [
        (s::Permissions::SEND_MESSAGES, "Отправлять сообщения"),
        (s::Permissions::EMBED_LINKS, "Встраивать ссылки"),
        (s::Permissions::ATTACH_FILES, "Прикреплять файлы"),
        (s::Permissions::READ_MESSAGE_HISTORY, "Читать историю"),
        (s::Permissions::MANAGE_MESSAGES, "Удалять спам"),
        (s::Permissions::MODERATE_MEMBERS, "Тайм-ауты"),
        (s::Permissions::MANAGE_ROLES, "Управлять ролями"),
    ]
    .into_iter()
    .filter(|(flag, _)| !perms.is_some_and(|p| p.contains(*flag)))
    .map(|(_, name)| name)
    .collect();
    text.push_str(&format!(
        "\n\nПрава в этом текстовом канале: {}.",
        if perms.is_none() {
            "не удалось определить".into()
        } else if missing.is_empty() {
            "все необходимые есть".into()
        } else {
            format!("не хватает: {}", missing.join(", "))
        }
    ));
    let voice = ctx.guild().and_then(|g| {
        let bot = ctx.framework().bot_id();
        let channel = g
            .voice_states
            .get(&bot)
            .or_else(|| g.voice_states.get(&ctx.author().id))?
            .channel_id?;
        let member = g.members.get(&bot)?;
        Some((
            channel,
            g.user_permissions_in(g.channels.get(&channel)?, member),
        ))
    });
    if let Some((channel, p)) = voice {
        text.push_str(&format!(
            "\nГолосовой канал <#{channel}>: подключение {}, передача звука {}.",
            p.connect(),
            p.speak()
        ));
    } else {
        text.push_str("\nДля проверки голосовых прав зайди в голосовой канал.");
    }
    text.push_str("\nДля магазина и тайм-аутов роль бота также должна быть выше роли участника.");
    ctx.send(
        poise::CreateReply::default()
            .embed(
                s::CreateEmbed::new()
                    .title("Настройки сервера")
                    .description(text)
                    .color(0x8B5CF6),
            )
            .ephemeral(true),
    )
    .await?;
    Ok(())
}
