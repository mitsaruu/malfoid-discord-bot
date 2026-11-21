//! Подтверждаем команды до длительной работы; запуск Activity требует собственного первого ответа.

use crate::{Context, Error};

pub fn is_expired(error: &Error) -> bool {
    matches!(error.downcast_ref::<poise::serenity_prelude::Error>(),
        Some(poise::serenity_prelude::Error::Http(
            poise::serenity_prelude::HttpError::UnsuccessfulRequest(response))) if response.error.code == 10062)
}

pub fn error_context(ctx: Context<'_>) -> String {
    let age = match ctx {
        // Старшие биты snowflake — миллисекунды с начала эпохи Discord (2015-01-01).
        poise::Context::Application(app) => chrono::Utc::now().timestamp_millis()
            - ((app.interaction.id.get() >> 22) + 1_420_070_400_000) as i64,
        _ => 0,
    };
    format!("/{}; guild={:?}; channel={}; user={}; elapsed={}ms",
        ctx.command().name, ctx.guild_id().map(|id| id.get()), ctx.channel_id(), ctx.author().id, age)
}

/// None — команда сама выбирает тип или видимость своего первого ответа.
fn defer_privately(command: &str) -> Option<bool> {
    if matches!(command, "games" | "roulette"
        | "coinflip" | "slots" | "give_money" | "blackjack" | "shop_buy"
        | "case" | "pet_feed" | "pet_sell" | "pet_battle" | "pet_trade" | "wordduel" | "dick_growth")
    {
        return None;
    }
    Some(matches!(command,
        "help" | "status" | "music_channel" | "server_settings" | "favorites" | "history" | "ai_limits" | "summary" | "cases" | "remind" | "reminders" | "lyrics" |
        "patchnotes" | "patchnotes_channel" | "log_channel" | "shop_add" | "shop_remove" | "daily_test"
        | "warns" | "warns_clear"
    ))
}

pub async fn before_command(ctx: Context<'_>) -> Result<bool, Error> {
    // Autocomplete не принимает обычный Defer.
    if let poise::Context::Application(app) = ctx {
        if app.interaction_type == poise::CommandInteractionType::Autocomplete {
            return Ok(true);
        }
    }
    if !crate::music_channels::command_check(ctx).await? {
        return Ok(false);
    }
    if !matches!(ctx.command().name.as_ref(), "status" | "music_channel")
        && !crate::music_channels::is_music_command(&ctx.command().name) && !crate::admin::games_channel_check(ctx).await? {
        return Ok(false);
    }
    if let Some(private) = defer_privately(&ctx.command().name) {
        match ctx {
            poise::Context::Application(app) => app.defer_response(private).await?,
            poise::Context::Prefix(_) => {}
        }
    }
    Ok(true)
}

/// Ошибки serenity могут включать временный токен interaction в URL. В логах он не нужен.
pub fn sanitize_error(details: &str) -> String {
    let mut text = details.to_string();
    for prefix in ["/interactions/", "/webhooks/"] {
        let mut offset = 0;
        while let Some(found) = text[offset..].find(prefix) {
            let id_start = offset + found + prefix.len();
            let Some(slash) = text[id_start..].find('/') else { break };
            let token_start = id_start + slash + 1;
            let token_len = text[token_start..].chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                .map(char::len_utf8).sum::<usize>();
            if token_len > 0 {
                text.replace_range(token_start..token_start + token_len, "<token>");
            }
            offset = token_start + if token_len > 0 { "<token>".len() } else { 0 };
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_and_private_commands_keep_their_response_type() {
        assert_eq!(defer_privately("games"), None);
        assert_eq!(defer_privately("roulette"), None);
        assert_eq!(defer_privately("warns"), Some(true));
        assert_eq!(defer_privately("favorites"), Some(true));
        assert_eq!(defer_privately("history"), Some(true));
        assert_eq!(defer_privately("remind"), Some(true));
        assert_eq!(defer_privately("summary"), Some(true));
        assert_eq!(defer_privately("play"), Some(false));
        assert_eq!(defer_privately("coinflip"), None);
    }

    #[test]
    fn hides_tokens_but_keeps_error_codes_and_endpoints() {
        let error = "404 /interactions/123/abc_X.12/callback and /webhooks/42/other-secret?wait=true 10062 Unknown interaction";
        let clean = sanitize_error(error);
        assert_eq!(clean, "404 /interactions/123/<token>/callback and /webhooks/42/<token>?wait=true 10062 Unknown interaction");
        assert_eq!(sanitize_error(&clean), clean);
        assert_eq!(sanitize_error("Gemini 503"), "Gemini 503");
    }
}
