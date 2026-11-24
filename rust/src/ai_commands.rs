//! Команды ИИ: /forget, /ai_limits, /summary, /quiz.

use crate::ai::{self, Turn};
use crate::reply::whisper;
use crate::text::money_text;
use crate::{db, Context, Error};
use poise::serenity_prelude as serenity;
use std::collections::HashSet;
use std::time::Duration;

const NOT_READY: &str = "ИИ не настроен: нет ключа GEMINI_API_KEY.";

/// Стереть память разговора с ботом в этом канале
#[poise::command(slash_command)]
pub async fn forget(ctx: Context<'_>) -> Result<(), Error> {
    let removed = ai::forget_history(ctx.data(), ctx.channel_id().get())?;
    ctx.say(format!("💬 Память разговора в этом канале стёрта (сообщений: {removed}).")).await?;
    Ok(())
}

/// Сколько запросов к ИИ осталось
#[poise::command(slash_command)]
pub async fn ai_limits(ctx: Context<'_>) -> Result<(), Error> {
    let data = ctx.data();
    if !data.ai.ready() {
        return whisper(ctx, NOT_READY).await;
    }
    let settings = &data.ai.settings;
    // Личный лимит (его ведёт сам бот); обнуляется в полночь по времени компьютера, на котором запущен бот
    let (count, last) = data.ai.usage_today(ctx.author().id.get());
    let left = (settings.daily_limit - count).max(0);
    let tomorrow = (chrono::Local::now().date_naive() + chrono::Duration::days(1)).and_hms_opt(0, 0, 0).unwrap();
    let personal_reset = tomorrow.and_local_timezone(chrono::Local).earliest().map_or(0, |time| time.timestamp());
    let wait = settings.cooldown as f64 - (db::now() - last);
    let (hydrogen_used, hydrogen_reserved) = ai::hydrogen_usage(data, ctx.author().id.get(), &ai::moscow_day())?;
    let provider_usage = ai::provider_usage(data, &ai::google_day())?;
    let google_reset = ai::google_reset_time();
    // <t:ЧИСЛО:t> и <t:ЧИСЛО:R> — формат Discord: каждый увидит время в своём часовом поясе
    let mut lines = vec![
        "📊 **Лимиты ИИ**".to_string(),
        String::new(),
        "**Твой личный лимит** (ограничение бота):".to_string(),
        format!("• Осталось вопросов сегодня: **{left}** из {}", settings.daily_limit),
        format!("• Обновится: <t:{personal_reset}:t> (<t:{personal_reset}:R>)"),
        format!("• Hydrogen: **{}** из {} ответов доступно, зарезервировано {hydrogen_reserved}. Сброс в 00:00 МСК (<t:{}:R>)",
                (ai::HYDROGEN_DAILY_LIMIT - hydrogen_used - hydrogen_reserved).max(0), ai::HYDROGEN_DAILY_LIMIT, ai::moscow_reset_time()),
    ];
    if wait > 0.0 {
        lines.push(format!("• Следующий вопрос можно задать через {} сек", wait as i64 + 1));
    }
    lines.extend([
        String::new(),
        "**Запросы к Gemini**".to_string(),
        format!("• Попыток за последнюю минуту: {}", data.ai.requests_last_minute()),
        format!("• Ориентиры из настроек: до {} в сутки и {} в минуту на модель", settings.google_daily_limit, settings.google_minute_limit),
        format!("• Обновится: <t:{google_reset}:t> (<t:{google_reset}:R>)"),
    ]);
    for row in provider_usage.into_iter().take(8) {
        let model: String = row.model.chars().take(80).collect();
        lines.push(format!("• `{model}`: {} попыток, {} ответов Gemini", row.attempts, row.succeeded));
    }
    let quota_error = data.ai.last_quota_error();
    if db::now() - quota_error < 24.0 * 3600.0 {
        lines.push(format!("⚠️ Google последний раз отказал из-за лимита <t:{}:R>", quota_error as i64));
    }
    // "-# " — в Discord это мелкий серый текст
    lines.extend([String::new(), "-# Лимиты Google примерные, бот считает только свои запросы. Точно: aistudio.google.com/rate-limit".to_string()]);
    whisper(ctx, lines.join("\n")).await
}

const SUMMARY_MAX_MESSAGES: usize = 300;

/// Задание для пересказа чата.
pub fn summary_task(hours: i64, chat: &str) -> String {
    format!(
        "Ниже сообщения из чата Discord за последние {hours} ч. Перескажи кратко, о чём говорили:\n\
         3–7 пунктов списком, самое важное — первым. Называй участников по именам, без упоминаний через @.\n\
         Не выдумывай того, чего в сообщениях нет. Если сообщений почти нет — так и скажи одной фразой.\n\n{chat}")
}

fn may_read_summary(ctx: Context<'_>) -> bool {
    let poise::Context::Application(app) = ctx else { return false; };
    app.interaction.member.as_ref().and_then(|member| member.permissions).is_some_and(|permissions| {
        permissions.administrator() || (permissions.view_channel() && permissions.read_message_history())
    })
}

/// ИИ кратко перескажет, о чём говорили в этом канале
#[poise::command(slash_command, guild_only)]
pub async fn summary(
    ctx: Context<'_>,
    #[description = "За сколько последних часов (по умолчанию 1)"]
    #[min = 1]
    #[max = 24]
    hours: Option<i64>,
) -> Result<(), Error> {
    let (data, hours) = (ctx.data(), hours.unwrap_or(1));
    if !data.ai.ready() {
        return whisper(ctx, "ИИ не настроен: нет ключа Gemini.").await;
    }
    if !may_read_summary(ctx) {
        return whisper(ctx, "Для пересказа нужно право «Читать историю сообщений» в этом канале.").await;
    }
    if let Some(refusal) = data.ai.limit_check(ctx.author().id.get()) {
        return whisper(ctx, refusal).await;
    }
    ctx.defer_ephemeral().await?;
    let after = db::now() as i64 - hours * 3600;
    // Discord отдаёт историю порциями по 100 сообщений, от новых к старым
    let (mut lines, mut before, mut done) = (Vec::new(), None::<serenity::MessageId>, false);
    for _ in 0..SUMMARY_MAX_MESSAGES.div_ceil(100) {
        if done || lines.len() >= SUMMARY_MAX_MESSAGES { break; }
        let mut request = serenity::GetMessages::new().limit(100);
        if let Some(before) = before {
            request = request.before(before);
        }
        let Ok(messages) = ctx.channel_id().messages(ctx, request).await else {
            ctx.say("Не могу прочитать историю этого канала — не хватает прав.").await?;
            return Ok(());
        };
        done = messages.len() < 100;
        for message in &messages {
            before = Some(message.id);
            if message.timestamp.unix_timestamp() < after {
                done = true;
                break;
            }
            let text: String = message.content_safe(ctx).chars().take(300).collect();
            if !text.is_empty() && !message.author.bot {
                let name = message.member.as_ref().and_then(|member| member.nick.clone()).unwrap_or_else(|| crate::reply::user_name(&message.author));
                lines.push(format!("{name}: {text}"));
            }
        }
    }
    if lines.len() < 3 {
        ctx.say(format!("За последние {hours} ч здесь почти ничего не писали.")).await?;
        return Ok(());
    }
    lines.reverse(); // история шла от новых к старым
    let task = summary_task(hours, &lines.join("\n"));
    let system = "Ты аккуратно и нейтрально пересказываешь переписку. Отвечай по-русски.";
    let delivery = ai::DeliveryReceipt::default();
    let answer = match ai::generate_selected(data, system, &[Turn::user(task)], ai::RequestOptions { user: Some(ctx.author().id.get()), delivery: Some(&delivery), ..Default::default() }).await {
        Ok(answer) => answer,
        Err(error) => {
            ctx.say(ai::error_text(&error)).await?;
            return Ok(());
        }
    };
    let text = ai::strip_tags(&answer.text, &ai::MOD_TAGS);
    let text = if text.is_empty() { "🤔 ИИ не дал ответа.".to_string() } else { text };
    for part in ai::split_message(&format!("📝 **О чём говорили за {hours} ч** ({} сообщений):\n{text}", lines.len()), 1900) {
        ctx.send(poise::CreateReply::default().content(part).ephemeral(true).allowed_mentions(serenity::CreateAllowedMentions::new())).await?;
        if !text.is_empty() { delivery.delivered(); }
    }
    Ok(())
}

/// Настройки викторины из settings.ini, раздел [quiz].
pub struct QuizSettings {
    pub reward: i64,  // монет за правильный ответ
    pub seconds: u64, // сколько секунд на ответ
}

impl QuizSettings {
    pub fn load(settings: &crate::config::Settings) -> Self {
        Self { reward: settings.int("quiz", "reward", 100).max(0), seconds: settings.int("quiz", "seconds", 30).max(10) as u64 }
    }
}

/// Задание для ИИ: придумать вопрос. used — недавние вопросы, чтобы не повторяться.
pub fn quiz_task(topic: Option<&str>, used: &[String]) -> String {
    let topic = topic.map_or(" на любую тему".to_string(), |topic| format!(" на тему «{topic}»"));
    let used = if used.is_empty() { "пока нет".to_string() } else { used.join("; ") };
    format!(
        "Придумай один вопрос для викторины{topic}. Вопрос должен быть интересным, с одним точно правильным\n\
         ответом, не слишком простым и не слишком сложным. Ответь ТОЛЬКО в формате JSON, без пояснений:\n\
         {{\"question\": \"текст вопроса\", \"options\": [\"вариант 1\", \"вариант 2\", \"вариант 3\", \"вариант 4\"], \"answer\": 0}}\n\
         answer — номер правильного варианта, считая с нуля. Все варианты разные, каждый не длиннее 70 символов.\n\
         Не повторяй эти вопросы: {used}")
}

/// Ответ ИИ → (вопрос, 4 варианта, номер правильного) или None, если ИИ ответил не по формату.
pub fn parse_quiz(text: &str) -> Option<(String, Vec<String>, usize)> {
    let json: serde_json::Value = serde_json::from_str(text.get(text.find('{')?..=text.rfind('}')?)?).ok()?;
    let question: String = json.get("question")?.as_str()?.trim().chars().take(300).collect();
    let options: Vec<String> =
        json.get("options")?.as_array()?.iter().map(|option| option.as_str().map(|text| text.trim().chars().take(80).collect())).collect::<Option<_>>()?;
    let answer = json.get("answer")?.as_u64()? as usize;
    let different = options.iter().collect::<HashSet<_>>().len() == 4;
    (!question.is_empty() && options.len() == 4 && different && answer < 4 && options.iter().all(|option| !option.is_empty()))
        .then_some((question, options, answer))
}

fn quiz_buttons(options: &[String], reveal: Option<usize>) -> Vec<serenity::CreateActionRow> {
    let buttons = options
        .iter()
        .enumerate()
        .map(|(index, option)| {
            // когда ответ раскрыт — кнопки выключены, а правильная зелёная
            let style = if reveal == Some(index) { serenity::ButtonStyle::Success } else { serenity::ButtonStyle::Secondary };
            serenity::CreateButton::new(format!("quiz:{index}")).label(option).style(style).disabled(reveal.is_some())
        })
        .collect();
    vec![serenity::CreateActionRow::Buttons(buttons)]
}

/// Канал свободен для новой викторины, пока жив этот объект.
struct Running<'a> {
    data: &'a crate::Data,
    channel: u64,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.data.quiz_running.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&self.channel);
    }
}

/// Викторина от ИИ: кто первым ответит правильно — получит монеты
#[poise::command(slash_command, guild_only)]
pub async fn quiz(
    ctx: Context<'_>,
    #[description = "Тема вопроса (если не указать — любая)"]
    #[max_length = 60]
    topic: Option<String>,
) -> Result<(), Error> {
    let (data, guild, channel) = (ctx.data(), ctx.guild_id().unwrap().get(), ctx.channel_id().get());
    if !data.ai.ready() {
        return whisper(ctx, "ИИ не настроен: нет ключа Gemini.").await;
    }
    if !data.quiz_running.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(channel) {
        return whisper(ctx, "В этом канале уже идёт викторина — сначала ответьте на неё.").await;
    }
    let _running = Running { data, channel };
    if let Some(refusal) = data.ai.limit_check(ctx.author().id.get()) {
        return whisper(ctx, refusal).await;
    }
    ctx.defer().await?;
    let used: Vec<String> = data.quiz_recent.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(&guild).cloned().unwrap_or_default();
    let task = quiz_task(topic.as_deref(), &used[used.len().saturating_sub(15)..]);
    let system = "Ты ведущий викторины. Отвечай по-русски и строго в формате JSON.";
    let delivery = ai::DeliveryReceipt::default();
    let answer = match ai::generate_selected(data, system, &[Turn::user(task)], ai::RequestOptions { user: Some(ctx.author().id.get()), delivery: Some(&delivery), ..Default::default() }).await {
        Ok(answer) => answer,
        Err(error) => {
            ctx.say(ai::error_text(&error)).await?;
            return Ok(());
        }
    };
    let Some((question, options, correct)) = parse_quiz(&answer.text) else {
        ctx.say("🤔 ИИ придумал вопрос не по правилам — попробуй ещё раз.").await?;
        return Ok(());
    };
    {
        let mut recent = data.quiz_recent.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let list = recent.entry(guild).or_default();
        list.push(question.clone());
        let extra = list.len().saturating_sub(30);
        list.drain(..extra);
    }
    let (reward, seconds) = (data.quiz.reward, data.quiz.seconds);
    let reply = ctx
        .send(poise::CreateReply::default()
            .content(format!(
                "🧠 **Викторина:** {question}\nНа ответ {seconds} сек., у каждого одна попытка. Приз — {}.", money_text(reward)))
            .components(quiz_buttons(&options, None)))
        .await?;
    delivery.delivered();
    let message_id = reply.message().await?.id;
    let result = |ending: String| format!("🧠 **Викторина:** {question}\n✅ Правильный ответ: **{}**\n{ending}", options[correct]);
    let (deadline, mut tried) = (tokio::time::Instant::now() + Duration::from_secs(seconds), HashSet::new());
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let press = serenity::ComponentInteractionCollector::new(ctx.serenity_context()).message_id(message_id).timeout(left).await;
        let Some(press) = press else {
            let ended = poise::CreateReply::default()
                .content(result("⌛ Время вышло — никто не угадал.".to_string()))
                .components(quiz_buttons(&options, Some(correct)));
            let _ = reply.edit(ctx, ended).await;
            return Ok(());
        };
        let note = |text: &str| {
            serenity::CreateInteractionResponse::Message(serenity::CreateInteractionResponseMessage::new().content(text).ephemeral(true))
        };
        if !tried.insert(press.user.id) {
            press.create_response(ctx, note("У тебя была одна попытка 🙂")).await?;
            continue;
        }
        if press.data.custom_id != format!("quiz:{correct}") {
            press.create_response(ctx, note("❌ Неверно.")).await?;
            continue;
        }
        press.create_response(ctx, serenity::CreateInteractionResponse::Acknowledge).await?;
        let name = guild_name(&press);
        let balance = data.db.add_money(guild, press.user.id.get(), &name, data.economy.start_balance, reward)?;
        let won = serenity::EditInteractionResponse::new()
            .content(result(format!(
                "🏆 Первым правильно ответил <@{}>: **+{}** (баланс {}).", press.user.id, money_text(reward), money_text(balance))))
            .components(quiz_buttons(&options, Some(correct)))
            .allowed_mentions(serenity::CreateAllowedMentions::new());
        press.edit_response(ctx, won).await?;
        return Ok(());
    }
}

/// Имя нажавшего кнопку на сервере (ник), а если ника нет — обычное имя.
fn guild_name(press: &serenity::ComponentInteraction) -> String {
    press.member.as_ref().map(|member| member.display_name().to_string()).unwrap_or_else(|| crate::reply::user_name(&press.user))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiz_parsing() {
        let good = r#"Вот: {"question": " Столица Австралии? ", "options": ["Сидней", "Канберра", "Мельбурн", "Перт"], "answer": 1} готово"#;
        let (question, options, answer) = parse_quiz(good).unwrap();
        assert_eq!((question.as_str(), options.len(), answer), ("Столица Австралии?", 4, 1));
        assert_eq!(parse_quiz("чушь"), None);
        assert_eq!(parse_quiz(r#"{"question":"q","options":["a","a","b","c"],"answer":0}"#), None); // одинаковые варианты
        assert_eq!(parse_quiz(r#"{"question":"q","options":["a","b","c"],"answer":0}"#), None);
        assert_eq!(parse_quiz(r#"{"question":"q","options":["a","b","c","d"],"answer":4}"#), None);
        assert_eq!(parse_quiz(r#"{"question":"","options":["a","b","c","d"],"answer":0}"#), None);
        assert_eq!(parse_quiz(r#"{"question":"q","options":["a","b","c",5],"answer":0}"#), None);
    }

    #[test]
    fn tasks() {
        let task = quiz_task(Some("космос"), &["Первый вопрос".to_string(), "Второй".to_string()]);
        assert!(task.contains("на тему «космос»") && task.contains("Не повторяй эти вопросы: Первый вопрос; Второй"));
        assert!(task.contains(r#"{"question": "текст вопроса""#));
        assert!(quiz_task(None, &[]).contains("на любую тему") && quiz_task(None, &[]).ends_with("пока нет"));
        assert!(summary_task(3, "A: привет").starts_with("Ниже сообщения из чата Discord за последние 3 ч."));
        let quiz = QuizSettings::load(&crate::config::Settings::default());
        assert_eq!((quiz.reward, quiz.seconds), (100, 30));
    }
}
