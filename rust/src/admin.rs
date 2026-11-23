//! Настройки сервера и модерация: канал для игр (/games_channel), канал логов (/log_channel),
//! просмотр и снятие варнов (/warns, /warns_clear). Варны выдаются по просьбе модератора, за оскорбления
//! бота (moderation.rs) и за спам (chat.rs).

use crate::reply::{say, whisper};
use crate::{Context, Data, Error};
use poise::serenity_prelude as serenity;
use std::collections::BTreeSet;

/// Какие команды работают только в канале для игр, если его выбрали (settings.ini → [channels] games_commands).
pub const DEFAULT_GAMES_COMMANDS: &str = "rank, leaderboard, money_leaderboard, dick_growth, dick_size, \
    dick_leaderboard, work, balance, give_money, coinflip, slots, blackjack, games, rocket, roulette, case, pets, pets_collect, pet_feed, pet_sell, \
    dice, roll, predskazanie, shar, words, daily, shop, shop_buy, pet_battle, pet_trade, wordstats, wordtop, wordduel, quiz";

/// Названия через запятую → набор названий без «/» и в нижнем регистре.
pub fn parse_commands(text: &str) -> BTreeSet<String> {
    let mut commands: BTreeSet<_> = text.split(',').map(|name| {
        let name = name.trim().trim_start_matches('/').to_lowercase();
        if matches!(name.as_str(), "wort" | "slovo") { "words".to_string() } else { name }
    }).filter(|name| !name.is_empty() && !crate::music_channels::is_music_command(name)).collect();
    // Старые настройки игрового канала включают команды, объединённые в /games.
    if commands.contains("words") || commands.contains("rocket") {
        commands.insert("games".into());
    }
    commands
}

/// Что боту можно в этом канале. None — не получилось узнать (нет данных о сервере).
pub fn bot_permissions(ctx: Context<'_>, channel: &serenity::GuildChannel) -> Option<serenity::Permissions> {
    let guild = ctx.guild()?;
    let member = guild.members.get(&ctx.framework().bot_id())?;
    Some(guild.user_permissions_in(channel, member))
}

/// Можно ли выполнить команду в этом канале. channel_parent — канал, внутри которого ветка (если это ветка).
pub fn allowed_here(games: &BTreeSet<String>, command: &str, games_channel: Option<u64>, channel: u64, channel_parent: Option<u64>) -> bool {
    match games_channel {
        Some(games_channel) if games.contains(command) => channel == games_channel || channel_parent == Some(games_channel),
        _ => true, // команда не из списка или канал для игр не выбран — можно везде
    }
}

/// Проверка перед каждой командой: если нельзя — тихо подсказывает, где можно.
pub async fn games_channel_check(ctx: Context<'_>) -> Result<bool, Error> {
    let Some(guild) = ctx.guild_id() else { return Ok(true) }; // в личных сообщениях можно всё
    let data = ctx.data();
    if !data.games_commands.contains(ctx.command().name.as_ref()) {
        return Ok(true);
    }
    let games_channel = data.db.games_channel(guild.get())?;
    if games_channel.is_none() || games_channel == Some(ctx.channel_id().get()) {
        return Ok(true);
    }
    // Discord уже присылает родительский канал ветки во взаимодействии. Не делаем HTTP-запрос
    // до первого ответа: на медленной сети он съедал отведённые Discord три секунды.
    let parent = match ctx {
        poise::Context::Application(app) => app.interaction.channel.as_ref().and_then(|channel| channel.parent_id),
        poise::Context::Prefix(_) => ctx.guild().and_then(|guild| guild.channels.get(&ctx.channel_id()).and_then(|channel| channel.parent_id)),
    }.map(|id| id.get());
    if allowed_here(&data.games_commands, &ctx.command().name, games_channel, ctx.channel_id().get(), parent) {
        return Ok(true);
    }
    whisper(ctx, format!("🎮 Эта команда работает только в <#{}>", games_channel.unwrap_or_default())).await?;
    Ok(false)
}

/// Канал для рангов, рейтингов и игр (пусто — работают везде)
#[poise::command(slash_command, guild_only, required_permissions = "MANAGE_GUILD", default_member_permissions = "MANAGE_GUILD", rename = "games_channel")]
pub async fn set_games_channel(
    ctx: Context<'_>,
    #[description = "Канал для игр (если не выбрать — команды снова будут работать в любом канале)"]
    #[channel_types("Text")]
    channel: Option<serenity::GuildChannel>,
) -> Result<(), Error> {
    let (data, guild) = (ctx.data(), ctx.guild_id().unwrap().get());
    let Some(channel) = channel else {
        data.db.set_games_channel(guild, None)?;
        return say(ctx, "✅ Ранги, рейтинги и игры снова работают в любом канале.").await;
    };
    let allowed = bot_permissions(ctx, &channel).is_some_and(|p| p.send_messages() && p.attach_files() && p.embed_links());
    if !allowed {
        return whisper(ctx, format!(
            "Мне не хватает прав в <#{}>: нужны «Отправлять сообщения», «Прикреплять файлы» и «Встраивать ссылки».",
            channel.id)).await;
    }
    data.db.set_games_channel(guild, Some(channel.id.get()))?;
    let commands: Vec<String> = data.games_commands.iter()
        .filter(|name| !matches!(name.as_str(), "words" | "rocket" | "roulette" | "slots"))
        .map(|name| format!("/{name}")).collect();
    say(ctx, format!(
        "✅ Теперь эти команды работают только в <#{}>:\n{}\nЧтобы снова разрешить их везде — вызови /games_channel без канала.",
        channel.id, commands.join(", "))).await
}

/// Канал, куда бот присылает свои ошибки (пусто — не присылать)
#[poise::command(slash_command, guild_only, required_permissions = "MANAGE_GUILD", default_member_permissions = "MANAGE_GUILD", rename = "log_channel")]
pub async fn set_log_channel(
    ctx: Context<'_>,
    #[description = "Канал для логов (лучше закрытый, только для админов). Пусто — выключить"]
    #[channel_types("Text")]
    channel: Option<serenity::GuildChannel>,
) -> Result<(), Error> {
    let (data, guild) = (ctx.data(), ctx.guild_id().unwrap().get());
    let Some(channel) = channel else {
        data.db.set_log_channel(guild, None)?;
        return whisper(ctx, "✅ Больше не присылаю ошибки в Discord (в консоль пишу как раньше).").await;
    };
    let allowed = bot_permissions(ctx, &channel).is_some_and(|p| p.view_channel() && p.send_messages() && p.embed_links());
    if !allowed {
        return whisper(ctx, format!(
            "Мне не хватает прав в <#{}>: нужны «Просматривать канал», «Отправлять сообщения» и «Встраивать ссылки».",
            channel.id)).await;
    }
    data.db.set_log_channel(guild, Some(channel.id.get()))?;
    whisper(ctx, format!("✅ Теперь присылаю ошибки в <#{}>. Выключить — /log_channel без канала.", channel.id)).await
}

/// Присылает ошибку в канал логов сервера (если он выбран). Без пингов.
pub async fn report_error(http: &serenity::Http, data: &Data, guild: Option<serenity::GuildId>, place: &str, details: &str) {
    let details = crate::interactions::sanitize_error(details);
    let Some(guild) = guild else { return };
    crate::diagnostics::record_error(data, guild.get(), place, &details);
    eprintln!("Ошибка ({place}, сервер {guild}): {details}");
    let Ok(Some(channel)) = data.db.log_channel(guild.get()) else { return };
    let embed = serenity::CreateEmbed::new().color(0xEF4444)
        .title(format!("⚠️ Ошибка: {place}"))
        .description("Категория и время последней ошибки доступны в /status. Технические подробности в журнале службы.");
    let message = serenity::CreateMessage::new().embed(embed).allowed_mentions(serenity::CreateAllowedMentions::new());
    if let Err(error) = serenity::ChannelId::new(channel).send_message(http, message).await {
        println!("Не получилось отправить лог в канал: {error}");
    }
}

/// Посмотреть варны участника
#[poise::command(slash_command, guild_only, ephemeral, rename = "warns")]
pub async fn show_warns(
    ctx: Context<'_>,
    #[description = "Чьи варны посмотреть (если не выбрать — твои)"] member: Option<serenity::Member>,
) -> Result<(), Error> {
    let (data, guild) = (ctx.data(), ctx.guild_id().unwrap().get());
    if member.as_ref().is_some_and(|member| member.user.id != ctx.author().id) {
        let is_moderator = ctx.author_member().await.is_some_and(|requester| {
            crate::moderation::is_moderator(ctx.serenity_context(), serenity::GuildId::new(guild), &requester)
        });
        if !is_moderator {
            return whisper(ctx, "Чужие варны видны только модераторам.").await;
        }
    }
    let (user, name) = match &member {
        Some(member) => (member.user.id.get(), member.display_name().to_string()),
        None => (ctx.author().id.get(), crate::reply::display_name(ctx).await),
    };
    let (rows, total) = data.db.warns(guild, user)?;
    if rows.is_empty() {
        return whisper(ctx, format!("✅ У <@{user}> нет варнов.")).await;
    }
    let lines: Vec<String> = rows
        .iter()
        .map(|(moderator, reason, created)| format!("• <t:{}:d> — {reason} (выдал <@{moderator}>)", *created as i64))
        .collect();
    let window = lines.join("\n");
    let description: String = window.chars().take(2000).collect();
    let mut embed = serenity::CreateEmbed::new().color(0xF59E0B).title(format!("⚠️ Варны {name}: {total}")).description(description);
    if total > rows.len() as i64 {
        embed = embed.footer(serenity::CreateEmbedFooter::new(format!("Показаны последние {}", rows.len())));
    }
    ctx.send(poise::CreateReply::default().ephemeral(true).embed(embed).allowed_mentions(serenity::CreateAllowedMentions::new())).await?;
    Ok(())
}

/// Снять все варны с участника
#[poise::command(slash_command, guild_only, ephemeral, required_permissions = "MODERATE_MEMBERS", default_member_permissions = "MODERATE_MEMBERS")]
pub async fn warns_clear(ctx: Context<'_>, #[description = "С кого снять варны"] member: serenity::User) -> Result<(), Error> {
    let guild_id = ctx.guild_id().unwrap();
    let guild = guild_id.get();
    let cutoff:Option<i64>=ctx.data().db.lock().query_row("SELECT MAX(id) FROM warns WHERE guild_id=?1 AND user_id=?2",rusqlite::params![guild as i64,member.id.get() as i64],|r|r.get(0))?;
    if cutoff.is_none(){return say(ctx,"У участника нет варнов.").await}
    if !crate::qol::confirm(ctx,format!("Снять все текущие варны с <@{}>?",member.id)).await?{return Ok(())}
    let requester = ctx.http().get_member(guild_id, ctx.author().id).await?;
    let target = ctx.http().get_member(guild_id, member.id).await?;
    let serenity = ctx.serenity_context();
    if !crate::moderation::permissions(serenity, guild_id, &requester).moderate_members()
        || !crate::moderation::outranks(serenity, guild_id, &requester, &target)
    {
        return whisper(ctx, "Чтобы снять варны, нужно право на модерацию и роль выше участника.").await;
    }
    let deleted=ctx.data().db.lock().execute("DELETE FROM warns WHERE guild_id=?1 AND user_id=?2 AND id<=?3",rusqlite::params![guild as i64,member.id.get() as i64,cutoff])?;
    say(ctx, if deleted > 0 {
        format!("🧹 С <@{}> сняты варны: {deleted}.", member.id)
    } else {
        format!("У <@{}> и так нет варнов.", member.id)
    }).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn games_channel_rules() {
        let games = parse_commands(" rank, /Work ,slots,, ");
        assert_eq!(games.iter().cloned().collect::<Vec<_>>(), ["rank", "slots", "work"]);
        assert!(allowed_here(&games, "rank", None, 5, None)); // канал для игр не выбран
        assert!(allowed_here(&games, "play", Some(9), 5, None)); // команда не из списка
        assert!(!allowed_here(&games, "rank", Some(9), 5, None));
        assert!(allowed_here(&games, "rank", Some(9), 9, None));
        assert!(allowed_here(&games, "rank", Some(9), 5, Some(9))); // ветка внутри канала для игр
        assert!(parse_commands(DEFAULT_GAMES_COMMANDS).contains("wordduel"));
        let legacy = parse_commands("wort, /slovo, wordstats");
        assert!(legacy.contains("words"));
        assert!(!allowed_here(&legacy, "games", Some(9), 5, None));
        assert!(allowed_here(&legacy, "games", Some(9), 5, Some(9)));
        assert!(parse_commands("rocket").contains("games"));
        assert!(!legacy.contains("wort") && !legacy.contains("slovo"));
        assert!(!allowed_here(&legacy, "words", Some(9), 5, None));
    }
}
