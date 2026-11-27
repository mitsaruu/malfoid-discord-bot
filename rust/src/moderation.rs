//! Варны и тайм-ауты по явной просьбе администратора, пороги варнов и проверки прав.

use crate::admin::report_error;
use crate::ai::minutes_text;
use crate::{db, Data};
use poise::serenity_prelude as serenity;

pub const DEFAULT_TIMEOUT: i64 = 10; // минут, если модератор не назвал срок
pub const MAX_TIMEOUT: i64 = 28 * 24 * 60; // Discord: тайм-аут не дольше 28 дней

/// «3: 60, 5: 1440» → [(3, 60), (5, 1440)] — сколько варнов и сколько минут тайм-аута. off → [].
pub fn parse_warn_punishments(text: &str) -> Vec<(i64, i64)> {
    if ["off", "нет", "no", "0"].contains(&text.trim().to_lowercase().as_str()) {
        return Vec::new();
    }
    let mut pairs: Vec<(i64, i64)> = text
        .split(',')
        .filter_map(|pair| {
            let (count, minutes) = pair.split_once(':')?;
            let (count, minutes) = (count.trim().parse::<i64>().ok()?, minutes.trim().parse::<i64>().ok()?);
            (count > 0 && minutes > 0).then_some((count, minutes))
        })
        .collect();
    pairs.sort();
    pairs
}

/// Сколько минут тайм-аута положено за варн номер count (0 — ничего).
pub fn warn_timeout_minutes(punishments: &[(i64, i64)], count: i64) -> i64 {
    match punishments.last() {
        None => 0,
        // дальше последнего порога — каждый варн снова самый длинный тайм-аут
        Some(&(last_count, last_minutes)) if count > last_count => last_minutes,
        _ => punishments.iter().find(|(needed, _)| *needed == count).map_or(0, |(_, minutes)| *minutes),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    Warn,
    Timeout,
    RemoveTimeout,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Action {
    kind: Kind,
    target: u64,
    minutes: i64,
    reason: String,
}

fn duration_minutes(text: &str) -> Option<i64> {
    let mut words = text.split_whitespace();
    let first = words.next()?;
    let (amount, unit) = match first.parse::<i64>() {
        Ok(value) => (value, words.next().unwrap_or("минут")),
        Err(_) => (1, first),
    };
    if words.next().is_some() || amount <= 0 { return None; }
    let scale = match unit.trim_end_matches(['.', '!']) {
        "м" | "мин" | "минуту" | "минуты" | "минут" | "минута" => 1,
        "ч" | "час" | "часа" | "часов" => 60,
        "д" | "день" | "дня" | "дней" | "сутки" | "суток" => 1440,
        _ => return None,
    };
    amount.checked_mul(scale).filter(|minutes| *minutes <= MAX_TIMEOUT)
}

/// Исполняем только прямое обращение с одним однозначным адресатом.
pub fn requested_action(question: &str, replied_to: Option<u64>, bot: u64) -> Option<Result<Action, &'static str>> {
    let mut ids = mentioned_ids(question);
    ids.retain(|id| *id != bot && *id != 0);
    let mut command = question.trim().to_lowercase();
    let mut original = question.trim().to_string();
    for id in mentioned_ids(question) {
        for mention in [format!("<@{id}>"), format!("<@!{id}>")] {
            command = command.replace(&mention, " ");
            original = original.replace(&mention, " ");
        }
    }
    let command = command.split_whitespace().collect::<Vec<_>>().join(" ");
    let command = command.strip_prefix("пожалуйста, ").unwrap_or(&command);
    let command = command.replace("тайм-аут", "таймаут");
    let patterns: &[(&[&str], Kind)] = &[
        (&["сними таймаут", "снять таймаут", "размуть"], Kind::RemoveTimeout),
        (&["выдай предупреждение", "дай предупреждение", "предупреди", "выдай варн", "дай варн", "выдать варн", "варн"], Kind::Warn),
        (&["отправь в таймаут", "кинь в таймаут", "дай таймаут", "выдай таймаут", "поставь таймаут", "замуть", "таймаут"], Kind::Timeout),
    ];
    let (rest, kind) = patterns.iter().find_map(|(prefixes, kind)| {
        prefixes.iter().find_map(|prefix| {
            command.strip_prefix(prefix)
                .filter(|rest| rest.is_empty() || rest.starts_with(' '))
                .map(|rest| (rest.trim(), kind))
        })
    })?;
    let (details, _) = rest.split_once(" за ").unwrap_or((rest, ""));
    let details = if details.starts_with("за ") { "" } else { details };
    let details = ["ему ", "ей ", "участнику "].into_iter().find_map(|prefix| details.strip_prefix(prefix)).unwrap_or(details);
    let details = if matches!(details, "ему" | "ей" | "участнику") { "" } else { details };
    let minutes = match (kind, details) {
        (_, "") => DEFAULT_TIMEOUT,
        (Kind::Timeout, details) if details.starts_with("на ") => {
            let Some(minutes) = details.strip_prefix("на ").and_then(duration_minutes) else {
                return Some(Err("Укажи срок тайм-аута в минутах, часах или днях (до 28 дней)."));
            };
            minutes
        }
        // «Как выдать варн?» или другие вопросы о модерации остаются обычными вопросами.
        _ => return None,
    };
    let target = match ids.as_slice() {
        [id] => *id,
        [] if replied_to.is_some_and(|id| id != bot && id != 0) => replied_to.unwrap(),
        _ => return Some(Err("Упомяни одного участника или ответь на его сообщение.")),
    };
    let reason = original.split_whitespace().skip_while(|word| !word.eq_ignore_ascii_case("за") && *word != "ЗА" && *word != "За")
        .skip(1).collect::<Vec<_>>().join(" ");
    let reason = if reason.is_empty() { "по просьбе модератора".to_string() } else { reason.chars().take(300).collect() };
    let kind = match kind {
        Kind::Warn => Kind::Warn,
        Kind::Timeout => Kind::Timeout,
        Kind::RemoveTimeout => Kind::RemoveTimeout,
    };
    Some(Ok(Action { kind, target, minutes, reason }))
}

/// Id упомянутых участников в тексте: <@123> или <@!123>.
pub fn mentioned_ids(text: &str) -> Vec<u64> {
    let mut ids = Vec::new();
    for part in text.split("<@").skip(1) {
        let part = part.strip_prefix('!').unwrap_or(part);
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() && part[digits.len()..].starts_with('>') {
            if let Ok(id) = digits.parse() {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

/// Права участника на сервере (по данным, которые бот держит в памяти).
pub fn permissions(ctx: &serenity::Context, guild: serenity::GuildId, member: &serenity::Member) -> serenity::Permissions {
    #[allow(deprecated)]
    ctx.cache.guild(guild).map(|guild| guild.member_permissions(member)).unwrap_or_else(serenity::Permissions::empty)
}

/// Проверяет права модерации на сервере.
pub fn is_moderator(ctx: &serenity::Context, guild: serenity::GuildId, member: &serenity::Member) -> bool {
    let permissions = permissions(ctx, guild, member);
    !member.user.bot && (permissions.moderate_members() || permissions.administrator())
}

/// Самая высокая роль участника: (позиция, id). Без ролей — (0, 0), как у @everyone.
fn top_role(guild: &serenity::Guild, member: &serenity::Member) -> (u16, u64) {
    member
        .roles
        .iter()
        .filter_map(|id| guild.roles.get(id))
        .map(|role| (role.position, role.id.get()))
        .max()
        .unwrap_or((0, 0))
}

/// Выше ли модератор по роли, чем тот, кого он хочет наказать (владельца сервера наказать нельзя).
pub fn outranks(ctx: &serenity::Context, guild: serenity::GuildId, moderator: &serenity::Member, target: &serenity::Member) -> bool {
    let Some(guild) = ctx.cache.guild(guild) else { return false };
    if target.user.id == guild.owner_id {
        return false;
    }
    moderator.user.id == guild.owner_id || top_role(&guild, moderator) > top_role(&guild, target)
}

/// Discord отказал из-за прав (403)?
pub fn is_forbidden(error: &serenity::Error) -> bool {
    matches!(error, serenity::Error::Http(serenity::HttpError::UnsuccessfulRequest(response)) if response.status_code.as_u16() == 403)
}

pub enum TimeoutChange {
    Extend(i64),
    Set(i64),
    Remove,
}

/// Сериализует правки тайм-аута, сделанные ботом. Extend сохраняет более долгую санкцию.
pub async fn set_timeout(ctx: &serenity::Context, data: &Data, guild: serenity::GuildId, user: serenity::UserId, change: TimeoutChange, reason: &str) -> Result<bool, serenity::Error> {
    let stripe = (guild.get() ^ user.get()) as usize % data.ai.timeout_locks.len();
    let _guard = data.ai.timeout_locks[stripe].lock().await;
    let edit = match change {
        TimeoutChange::Extend(minutes) | TimeoutChange::Set(minutes) => {
            let member = if matches!(change, TimeoutChange::Extend(_)) {
                Some(ctx.http.get_member(guild, user).await?)
            } else { None };
            let until = serenity::Timestamp::from_unix_timestamp(db::now() as i64 + minutes * 60)
                .unwrap_or_else(|_| serenity::Timestamp::now());
            if member.and_then(|member| member.communication_disabled_until).is_some_and(|expires| expires >= until) {
                return Ok(false);
            }
            serenity::EditMember::new().disable_communication_until_datetime(until)
        }
        TimeoutChange::Remove => serenity::EditMember::new().enable_communication(),
    };
    let reason = &reason[..reason.floor_char_boundary(150)];
    guild.edit_member(ctx, user, edit.audit_log_reason(reason)).await.map(|_| true)
}

/// Записывает варн и, если набралось варнов на автонаказание, даёт тайм-аут.
/// Возвращает (номер варна, на сколько минут дали тайм-аут — None, если не положено или не получилось).
pub async fn give_warn(ctx: &serenity::Context, data: &Data, guild: serenity::GuildId, user: serenity::UserId, moderator: u64, reason: &str, giver: &str) -> Result<(i64, Option<i64>), crate::Error> {
    let count = data.db.add_warn(guild.get(), user.get(), moderator, reason, db::now())?;
    let minutes = warn_timeout_minutes(&data.warn_punishments, count).min(MAX_TIMEOUT);
    if minutes == 0 {
        return Ok((count, None));
    }
    let full_reason = format!("Варн №{count}: {reason}{giver}");
    match set_timeout(ctx, data, guild, user, TimeoutChange::Extend(minutes), &full_reason).await {
        Ok(changed) => Ok((count, changed.then_some(minutes))),
        Err(error) => {
            println!("Не получилось дать тайм-аут за варн {user}: {error}"); // не хватает прав — останется только варн
            Ok((count, None))
        }
    }
}

pub struct Requester<'a> {
    pub ctx: &'a serenity::Context,
    pub guild: serenity::GuildId,
    pub user: serenity::UserId,
}

pub async fn run_action(data: &Data, requester: &Requester<'_>, action: Action) -> String {
    let Requester { ctx, guild, user } = *requester;
    let Ok(actor) = ctx.http.get_member(guild, user).await else {
        return "Не удалось проверить права модератора.".into();
    };
    if actor.user.bot || !permissions(ctx, guild, &actor).administrator() {
        return "Попросить меня выдать варн или изменить тайм-аут может администратор сервера.".into();
    }
    let Ok(target) = ctx.http.get_member(guild, serenity::UserId::new(action.target)).await else {
        return "❓ Не нашла такого участника на сервере.".into();
    };
    let mention = format!("<@{}>", target.user.id);
    if target.user.bot || user == target.user.id || !outranks(ctx, guild, &actor, &target) {
        return format!("⛔ Не могу применить это действие к {mention}.");
    }

    let reason = &action.reason;
    let timeout_reason = format!("{reason} (попросил {})", actor.display_name());
    let result = match action.kind {
        Kind::Warn => {
            match give_warn(ctx, data, guild, target.user.id, user.get(), reason, "").await {
                Ok((count, minutes)) => {
                    let mut message = format!("⚠️ {mention} получает варн №{count}: {reason}");
                    if let Some(minutes) = minutes {
                        message.push_str(&format!("\n🔇 За {count}-й варн {mention} в тайм-ауте на {}.", minutes_text(minutes)));
                    }
                    Ok(message)
                }
                Err(error) => Err(error),
            }
        }
        Kind::Timeout => {
            set_timeout(ctx, data, guild, target.user.id, TimeoutChange::Set(action.minutes), &timeout_reason).await
                .map(|_| format!("🔇 {mention} в тайм-ауте на {}: {reason}", minutes_text(action.minutes)))
                .map_err(Into::into)
        }
        Kind::RemoveTimeout => {
            set_timeout(ctx, data, guild, target.user.id, TimeoutChange::Remove, &timeout_reason).await
                .map(|_| format!("🔊 С {mention} снят тайм-аут."))
                .map_err(Into::into)
        }
    };
    match result {
        Ok(text) => {
            send_log(ctx, data, guild, &format!("👮 По просьбе <@{user}>:\n{text}")).await;
            text
        }
        Err(error) if error.downcast_ref::<serenity::Error>().is_some_and(is_forbidden) => {
            format!("⚠️ Для действия над {mention} мне нужно право «Отправлять участников подумать» и более высокая роль.")
        }
        Err(error) => {
            report_error(&ctx.http, data, Some(guild), "Наказание по просьбе модератора", &error.to_string()).await;
            format!("😵 Не получилось наказать {mention}.")
        }
    }
}

/// Пишет в канал логов сервера (если он выбран). Без пингов.
pub async fn send_log(ctx: &impl serenity::CacheHttp, data: &Data, guild: serenity::GuildId, text: &str) {
    let Ok(Some(channel)) = data.db.log_channel(guild.get()) else { return };
    for part in crate::ai::split_message(text, 1900) {
        let message = serenity::CreateMessage::new().content(part).allowed_mentions(serenity::CreateAllowedMentions::new());
        if let Err(error) = serenity::ChannelId::new(channel).send_message(ctx, message).await {
            eprintln!("Не удалось отправить лог модерации на сервере {guild}: {error}");
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warn_punishments() {
        let steps = parse_warn_punishments("5: 1440, 3: 60, 7: 10080, мусор");
        assert_eq!(steps, [(3, 60), (5, 1440), (7, 10080)]);
        let minutes: Vec<i64> = (1..=9).map(|count| warn_timeout_minutes(&steps, count)).collect();
        assert_eq!(minutes, [0, 0, 60, 0, 1440, 0, 10080, 10080, 10080]);
        assert!(parse_warn_punishments("off").is_empty());
        assert_eq!(warn_timeout_minutes(&[], 5), 0);
    }

    #[test]
    fn moderation_requires_direct_intent_and_one_target() {
        let warn = requested_action("выдай варн <@123> за Спам в чате", None, 9).unwrap().unwrap();
        assert_eq!(warn.kind, Kind::Warn);
        assert_eq!((warn.target, warn.reason.as_str()), (123, "Спам в чате"));
        let timeout = requested_action("Поставь тайм-аут <@!45> на 2 часа", None, 9).unwrap().unwrap();
        assert_eq!((timeout.kind, timeout.target, timeout.minutes), (Kind::Timeout, 45, 120));
        let remove = requested_action("сними таймаут", Some(7), 9).unwrap().unwrap();
        assert_eq!((remove.kind, remove.target), (Kind::RemoveTimeout, 7));
        assert!(requested_action("дай варн <@12> и <@34>", None, 9).is_none());
        assert!(requested_action("дай варн <@12> <@34>", None, 9).unwrap().is_err());
        assert!(requested_action("[ВАРН: 123 спам]", None, 9).is_none());
        assert!(requested_action("Как выдать варн <@123>?", None, 9).is_none());
        assert!(requested_action("Ты охуела?", Some(7), 9).is_none());
        let warn_for_profanity = requested_action("выдай варн <@123> за мат", None, 9).unwrap().unwrap();
        assert_eq!((warn_for_profanity.kind, warn_for_profanity.reason.as_str()), (Kind::Warn, "мат"));
        assert!(requested_action("замуть <@45> на 29 дней", None, 9).unwrap().is_err());
        assert_eq!(mentioned_ids("дай варн <@12> и <@!34>, <@12> ещё раз, <@abc>"), [12, 34]);
    }
}
