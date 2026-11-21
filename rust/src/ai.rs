//! Ядро ИИ: запросы к Gemini, характер и правила, лимиты, память разговора, грубость.
//! Команды (/ask, /summary, /quiz…) — в ai_commands.rs.

use crate::moderation::Requester;
use crate::text::plural;
use crate::{db, Data};
use base64::Engine;
use poise::serenity_prelude as serenity;
use serde_json::{json, Value};
use rusqlite::{params, OptionalExtension};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

const MAX_TOKENS: i64 = 8192; // предел длины ответа; с запасом — «обдумывание» модели тоже входит в предел
const HYDROGEN_MAX_TOKENS: i64 = 32768; // длинные ответы + рассуждение high
pub const MAX_IMAGES: usize = 3; // сколько картинок из одного сообщения отправлять ИИ
pub const MAX_IMAGE_MB: u64 = 5; // картинки тяжелее не отправляем (весь запрос к Gemini — не больше 20 МБ)
const IMAGE_TYPES: [&str; 5] = ["image/png", "image/jpeg", "image/webp", "image/heic", "image/heif"];

/// Характер по умолчанию (если config/prompt.txt пуст) и технические правила — те же тексты, что в bot.py.
const DEFAULT_CHARACTER: &str = include_str!("ai_character.txt");
const TECH_RULES: &str = include_str!("ai_rules.txt");

/// Настройки из settings.ini, раздел [ai].
#[derive(Clone)]
pub struct AiSettings {
    pub model: String,
    pub fallback_model: Option<String>, // запасная модель, если основная не отвечает
    pub thinking_level: Option<String>, // сколько ИИ «обдумывает» ответ: minimal, low, medium, high
    pub retries: i64,                   // сколько раз повторить запрос при ошибке «перегружен»
    pub cooldown: i64,                  // один человек спрашивает не чаще раза в столько секунд
    pub daily_limit: i64,               // и не больше стольких вопросов в день
    pub history_limit: i64,             // память разговора: сколько последних сообщений помнить
    pub google_daily_limit: i64,        // примерные лимиты бесплатного тарифа Google (для /ai_limits)
    pub google_minute_limit: i64,
    pub one_line: bool,                 // склеивать ответ в одну строку
}

pub const HYDROGEN_MODEL: &str = "gemini-4-hydrogen";
pub const FLASH_LITE_MODEL: &str = "gemini-3.5-flash-lite";
pub const HYDROGEN_DAILY_LIMIT: i64 = 4;

#[derive(Debug, Clone, Copy, Default, poise::ChoiceParameter)]
pub enum ModelChoice {
    #[default]
    #[name = "Авто"]
    Auto,
    #[name = "Flash-Lite"]
    FlashLite,
    #[name = "Hydrogen"]
    Hydrogen,
}

#[derive(Debug, Clone, Copy, poise::ChoiceParameter)]
pub enum ThinkingLevel {
    #[name = "low"]
    Low,
    #[name = "medium"]
    Medium,
    #[name = "high"]
    High,
}

impl ThinkingLevel {
    fn as_str(self) -> &'static str {
        match self { Self::Low => "low", Self::Medium => "medium", Self::High => "high" }
    }
}

#[derive(Clone, Copy, Default)]
pub struct RequestOptions<'a> {
    pub model: ModelChoice,
    pub thinking: Option<ThinkingLevel>,
    pub user: Option<u64>,
    pub delivery: Option<&'a DeliveryReceipt>,
}

fn request_settings(defaults: &AiSettings, options: RequestOptions<'_>) -> AiSettings {
    let mut settings = defaults.clone();
    match options.model {
        ModelChoice::Auto => {},
        ModelChoice::FlashLite => { settings.model = FLASH_LITE_MODEL.into(); settings.fallback_model = None; },
        ModelChoice::Hydrogen => { settings.model = HYDROGEN_MODEL.into(); settings.fallback_model = None; },
    }
    if let Some(level) = options.thinking { settings.thinking_level = Some(level.as_str().into()); }
    settings
}

pub fn moscow_day_at(now: chrono::DateTime<chrono::Utc>) -> String {
    now.with_timezone(&chrono_tz::Europe::Moscow).date_naive().to_string()
}

pub fn moscow_day() -> String { moscow_day_at(chrono::Utc::now()) }

pub fn moscow_reset_time() -> i64 {
    use chrono::TimeZone;
    let zone = chrono_tz::Europe::Moscow;
    let next = chrono::Utc::now().with_timezone(&zone).date_naive() + chrono::Duration::days(1);
    zone.from_local_datetime(&next.and_hms_opt(0, 0, 0).unwrap()).earliest().map_or(0, |time| time.timestamp())
}

/// Резерв снимается и при ошибке, и при отмене future по тайм-ауту.
struct HydrogenPermit {
    db: std::sync::Arc<db::Database>,
    user: u64,
    day: String,
    success: bool,
}

/// Живёт до отправки ответа в Discord. Отмена/ошибка доставки возвращают резерв.
#[derive(Default)]
pub struct DeliveryReceipt {
    permit: Mutex<Option<HydrogenPermit>>,
}

impl DeliveryReceipt {
    pub fn delivered(&self) {
        if let Some(mut permit) = locked(&self.permit).take() {
            permit.success = true;
        }
    }
}

impl Drop for HydrogenPermit {
    fn drop(&mut self) {
        if let Err(error) = self.db.finish_ai_model(self.user, &self.day, HYDROGEN_MODEL, self.success) {
            eprintln!("Не удалось сохранить расход Hydrogen: {error}");
        }
    }
}

impl AiSettings {
    pub fn load(settings: &crate::config::Settings) -> Self {
        let off = |text: &str| ["none", "нет", "off"].contains(&text.to_lowercase().as_str());
        let model = settings.text("ai", "model", "gemini-3.5-flash-lite");
        let level = settings.text("ai", "thinking_level", "high").to_lowercase();
        let fallback = settings.text("ai", "fallback_model", "gemini-4-hydrogen");
        Self {
            thinking_level: if off(&level) {
                None
            } else if ["minimal", "low", "medium", "high"].contains(&level.as_str()) {
                Some(level)
            } else {
                println!("settings.ini: thinking_level = {level} — такого уровня нет, беру high");
                Some("high".to_string())
            },
            fallback_model: (!off(&fallback) && fallback != model).then_some(fallback),
            model,
            retries: settings.int("ai", "retries", 2).max(0),
            cooldown: settings.int("ai", "cooldown_seconds", 5).clamp(0, 86_400),
            daily_limit: settings.int("ai", "daily_limit_per_user", 50),
            history_limit: settings.int("ai", "history_limit", 20).clamp(0, 100),
            google_daily_limit: settings.int("ai", "google_daily_limit", 500),
            google_minute_limit: settings.int("ai", "google_minute_limit", 15),
            one_line: !["off", "нет", "no", "0"].contains(&settings.text("ai", "one_line_answers", "on").to_lowercase().as_str()),
        }
    }
}

/// Настройки варнов за грубость из settings.ini, раздел [moderation].
pub struct RudeSettings {
    pub strike_reset: i64,       // через сколько минут без грубостей счётчик обнуляется
    pub announce: bool,          // писать ли в чат, что человек получил варн
}

impl RudeSettings {
    pub fn load(settings: &crate::config::Settings) -> Self {
        let legacy_announce = settings.text("moderation", "announce_timeouts", "on");
        let announce = settings.text("moderation", "announce_warnings", &legacy_announce);
        Self {
            strike_reset: settings.int("moderation", "strike_reset_minutes", 60),
            announce: !["off", "нет", "no", "0"].contains(&announce.to_lowercase().as_str()),
        }
    }
}

/// Всё, что ИИ хранит между запросами.
pub struct AiState {
    pub settings: AiSettings,
    pub rude: RudeSettings,
    pub key: Option<String>, // ключ Gemini; None — ИИ не настроен
    /// Сколько раз кто спрашивал: участник → (дата, вопросов за эту дату, время последнего вопроса). После перезапуска — с нуля.
    usage: Mutex<HashMap<u64, (String, i64, f64)>>,
    recent_requests: Mutex<VecDeque<Instant>>,
    last_quota_error: Mutex<f64>,
    generation_slots: Semaphore,
    classifier_slots: Semaphore,
    classifier_attempts: Mutex<VecDeque<Instant>>,
    pub(crate) ask_slots: Semaphore,
    image_slots: Semaphore,
    prompt: ConfigCache<String>,
    active_history: Mutex<HashMap<u64, Weak<()>>>,
    accounting_initialized: Mutex<bool>,
    pub(crate) timeout_locks: [tokio::sync::Mutex<()>; 64],
}

impl AiState {
    pub fn new(settings: &crate::config::Settings, key: Option<String>) -> Self {
        Self {
            settings: AiSettings::load(settings),
            rude: RudeSettings::load(settings),
            key: key.filter(|key| !key.is_empty()),
            usage: Default::default(),
            recent_requests: Default::default(),
            last_quota_error: Default::default(),
            generation_slots: Semaphore::new(8),
            classifier_slots: Semaphore::new(2),
            classifier_attempts: Default::default(),
            ask_slots: Semaphore::new(8),
            image_slots: Semaphore::new(4),
            prompt: Default::default(),
            active_history: Default::default(),
            accounting_initialized: Default::default(),
            timeout_locks: std::array::from_fn(|_| tokio::sync::Mutex::new(())),
        }
    }

    pub fn ready(&self) -> bool {
        self.key.is_some()
    }

    /// Личный счётчик человека на сегодня: (сколько вопросов задал, время последнего).
    pub fn usage_today(&self, user: u64) -> (i64, f64) {
        usage_for(&locked(&self.usage), user, &local_day())
    }

    /// Проверяет лимиты. Возвращает текст отказа или None, если спрашивать можно (и засчитывает вопрос).
    pub fn limit_check(&self, user: u64) -> Option<String> {
        let mut usage = locked(&self.usage);
        let now = db::now();
        let day = local_day();
        let (count, last) = usage_for(&usage, user, &day);
        if usage.len() >= 1024 && !usage.contains_key(&user) {
            usage.retain(|_, (date, _, last)| *date == day || now - *last < self.settings.cooldown as f64);
        }
        if usage.len() >= 20_000 && !usage.contains_key(&user) {
            return Some(error_text(&AiError::Busy).to_string());
        }
        if now - last < self.settings.cooldown as f64 {
            return Some(format!("Не так быстро 🙂 Подожди {} сек.", (self.settings.cooldown as f64 - (now - last)) as i64 + 1));
        }
        if count >= self.settings.daily_limit {
            return Some(format!("На сегодня лимит вопросов ({}) закончился. Завтра продолжим!", self.settings.daily_limit));
        }
        usage.insert(user, (day, count + 1, now));
        None
    }

    fn prune_recent(recent: &mut VecDeque<Instant>) {
        while recent.front().is_some_and(|time| time.elapsed() >= Duration::from_secs(60)) {
            recent.pop_front();
        }
    }

    fn admit_provider_attempt(&self) -> Result<(), AiError> {
        let mut recent = locked(&self.recent_requests);
        Self::prune_recent(&mut recent);
        if recent.len() >= 600 {
            return Err(AiError::Busy);
        }
        recent.push_back(Instant::now());
        Ok(())
    }

    /// Сколько запросов к Google бот попытался отправить за последние 60 секунд.
    pub fn requests_last_minute(&self) -> usize {
        let mut recent = locked(&self.recent_requests);
        Self::prune_recent(&mut recent);
        recent.len()
    }

    pub fn last_quota_error(&self) -> f64 {
        *locked(&self.last_quota_error)
    }
}

fn usage_for(usage: &HashMap<u64, (String, i64, f64)>, user: u64, today: &str) -> (i64, f64) {
    usage.get(&user).map_or((0, 0.0), |(day, count, last)| {
        (if day == today { *count } else { 0 }, *last)
    })
}

/// Перечитывает небольшой текстовый конфиг с задержкой до пяти секунд.
pub(crate) struct ConfigCache<T>(Mutex<Option<(Instant, Arc<T>)>>);

impl<T> Default for ConfigCache<T> {
    fn default() -> Self { Self(Mutex::new(None)) }
}

impl<T> ConfigCache<T> {
    pub(crate) fn read(&self, path: &std::path::Path, parse: impl FnOnce(&str) -> T) -> Arc<T> {
        use std::io::Read;
        const MAX_CONFIG: u64 = 128 * 1024;
        let mut cached = locked(&self.0);
        if let Some((at, value)) = &*cached {
            if at.elapsed() < Duration::from_secs(5) {
                return value.clone();
            }
        }
        let mut text = String::new();
        let loaded = std::fs::File::open(path).and_then(|file| file.take(MAX_CONFIG + 1).read_to_string(&mut text));
        let value = if matches!(loaded, Ok(size) if size <= MAX_CONFIG as usize) {
            Arc::new(parse(&text))
        } else if let Some((_, previous)) = &*cached {
            if !matches!(loaded, Err(ref error) if error.kind() == std::io::ErrorKind::NotFound) {
                previous.clone()
            } else {
                Arc::new(parse(""))
            }
        } else {
            Arc::new(parse(""))
        };
        *cached = Some((Instant::now(), value.clone()));
        value
    }
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Сегодняшний день по времени компьютера, на котором запущен бот.
fn local_day() -> String {
    chrono::Local::now().date_naive().to_string()
}

/// Сегодняшний день по тихоокеанскому времени — по нему Google считает дневной лимит.
pub fn google_day() -> String {
    chrono::Utc::now().with_timezone(&chrono_tz::America::Los_Angeles).date_naive().to_string()
}

/// Когда обнулится дневной лимит Google (ближайшая полночь по тихоокеанскому времени), в секундах.
pub fn google_reset_time() -> i64 {
    use chrono::TimeZone;
    let zone = chrono_tz::America::Los_Angeles;
    let tomorrow = chrono::Utc::now().with_timezone(&zone).date_naive() + chrono::Duration::days(1);
    zone.from_local_datetime(&tomorrow.and_hms_opt(0, 0, 0).unwrap()).earliest().map_or(0, |time| time.timestamp())
}

/// Что пошло не так при запросе к ИИ.
#[derive(Debug, Clone, PartialEq)]
pub enum AiError {
    HydrogenLimit,
    Busy,
    Quota,         // Google: лимит запросов исчерпан (429)
    Overloaded,    // Google перегружен или временная ошибка сервера (5xx)
    Network(String), // тайм-аут, обрыв соединения, DNS/TLS — можно повторить запрос
    Other(String), // всё остальное (например, неверный ключ)
}

impl std::fmt::Display for AiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AiError::HydrogenLimit => write!(formatter, "Лимит Hydrogen: 4 ответа в сутки, сброс в 00:00 МСК. Выбери Flash-Lite."),
            AiError::Busy => write!(formatter, "очередь запросов ИИ заполнена"),
            AiError::Quota => write!(formatter, "лимит запросов к Gemini исчерпан (429)"),
            AiError::Overloaded => write!(formatter, "Gemini перегружен (5xx)"),
            AiError::Network(details) => write!(formatter, "Сетевая ошибка Gemini: {details}"),
            AiError::Other(details) => write!(formatter, "{details}"),
        }
    }
}

impl std::error::Error for AiError {}

/// Понятный текст ошибки ИИ для чата.
pub fn error_text(error: &AiError) -> &'static str {
    match error {
        AiError::HydrogenLimit => "Лимит Hydrogen на сегодня закончился: 4 ответа в сутки. Сброс в 00:00 МСК; пока можно выбрать Flash-Lite.",
        AiError::Busy => "Сейчас слишком много запросов к ИИ. Попробуй чуть позже.",
        AiError::Quota => "😵 Лимит бесплатных запросов к ИИ на сейчас исчерпан. Попробуй позже (подробнее — /ai_limits).",
        AiError::Overloaded => "😵 ИИ у Google сейчас перегружен, попробуй через пару минут.",
        AiError::Network(_) => "😵 Не удалось дождаться ответа ИИ: проблема соединения с Google. Попробуй позже.",
        AiError::Other(_) => "😵 ИИ сейчас недоступен, попробуй чуть позже.",
    }
}

/// Одна реплика разговора для Gemini: роль ("user" или "model"), текст и картинки (байты, формат).
pub struct Turn {
    pub role: String,
    pub text: String,
    pub images: Vec<(Vec<u8>, String)>,
}

impl Turn {
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: "user".into(), text: text.into(), images: Vec::new() }
    }
}

/// Ответ ИИ: текст и признак «упёрся в предел длины».
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub text: String,
    pub cut: bool,
}

/// Тело запроса к Gemini.
pub fn request_body(settings: &AiSettings, model: &str, system: &str, turns: &[Turn]) -> Value {
    let contents: Vec<Value> = turns
        .iter()
        .map(|turn| {
            let mut parts = vec![json!({ "text": turn.text })];
            for (bytes, mime) in &turn.images {
                let data = base64::engine::general_purpose::STANDARD.encode(bytes);
                parts.push(json!({ "inlineData": { "mimeType": mime, "data": data } }));
            }
            json!({ "role": turn.role, "parts": parts })
        })
        .collect();
    let mut config = json!({ "maxOutputTokens": if model == HYDROGEN_MODEL { HYDROGEN_MAX_TOKENS } else { MAX_TOKENS } });
    // «minimal» есть только у Flash-Lite — для других моделей берём ближайший уровень
    let level = settings.thinking_level.as_deref().map(|level| if level == "minimal" && !model.contains("lite") { "low" } else { level });
    if let Some(level) = level {
        config["thinkingConfig"] = json!({ "thinkingLevel": level });
    }
    json!({ "systemInstruction": { "parts": [{ "text": system }] }, "contents": contents, "generationConfig": config })
}

/// Достаёт текст из ответа Gemini (без «мыслей» модели).
pub fn parse_answer(response: &Value) -> Answer {
    let candidate = response.pointer("/candidates/0");
    let text = candidate
        .and_then(|candidate| candidate.pointer("/content/parts"))
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter(|part| part.get("thought").and_then(Value::as_bool) != Some(true))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<String>()
        })
        .unwrap_or_default();
    let cut = candidate.and_then(|candidate| candidate.get("finishReason")).and_then(Value::as_str) == Some("MAX_TOKENS");
    Answer { text: text.trim().to_string(), cut }
}

async fn call_gemini_settings(data: &Data, settings: &AiSettings, model: &str, system: &str, turns: &[Turn]) -> Result<Answer, AiError> {
    send_gemini(data, model, request_body(settings, model, system, turns)).await
}

async fn send_gemini(data: &Data, model: &str, body: Value) -> Result<Answer, AiError> {
    let key = data.ai.key.as_deref().ok_or_else(|| AiError::Other("нет ключа GEMINI_API_KEY".into()))?;
    let url = format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent");
    let response = data
        .web
        .post(url)
        .header("x-goog-api-key", key)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .timeout(http_timeout(model))
        .send()
        .await
        .map_err(transport_error)?;
    let status = response.status();
    let text = response.text().await.map_err(transport_error)?;
    if status.as_u16() == 429 || text.contains("RESOURCE_EXHAUSTED") && !status.is_success() {
        return Err(AiError::Quota);
    }
    if status.is_server_error() {
        return Err(AiError::Overloaded);
    }
    if status.as_u16() == 408 {
        return Err(AiError::Network("Google не дождался запроса (408)".into()));
    }
    if !status.is_success() {
        let short: String = text.chars().take(500).collect();
        return Err(AiError::Other(format!("Gemini ответил {status}: {short}")));
    }
    let json: Value = serde_json::from_str(&text).map_err(|error| AiError::Other(error.to_string()))?;
    Ok(parse_answer(&json))
}

fn transport_error(error: reqwest::Error) -> AiError {
    let retryable = error.is_timeout() || error.is_connect() || error.is_request() || error.is_body() || error.is_decode();
    let error = error.without_url();
    let mut details = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        details.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    if retryable { AiError::Network(details) } else { AiError::Other(details) }
}

/// Запрос к Gemini с повторами и запасной моделью.
/// 1) Основная модель: если Google ответил «перегружен» — ждём и пробуем ещё (до retries раз).
/// 2) Не помогло (или кончился лимит основной модели) — тот же вопрос запасной модели.
pub async fn generate(data: &Data, system: &str, turns: &[Turn]) -> Result<Answer, AiError> {
    // Ограничиваем суммарное ожидание всех моделей: один запрос не занимает все 15 минут
    // жизни Discord interaction. Отмена прекращает текущий запрос и дальнейшие повторы.
    generate_selected(data, system, turns, RequestOptions::default()).await
}

#[cfg(test)]
async fn call_gemini(data: &Data, model: &str, system: &str, turns: &[Turn]) -> Result<Answer, AiError> {
    call_gemini_settings(data, &data.ai.settings, model, system, turns).await
}

pub async fn generate_selected(data: &Data, system: &str, turns: &[Turn], options: RequestOptions<'_>) -> Result<Answer, AiError> {
    let _slot = data.ai.generation_slots.try_acquire().map_err(|_| AiError::Busy)?;
    let mut settings = request_settings(&data.ai.settings, options);
    if options.user.is_none() && matches!(options.model, ModelChoice::Auto) {
        // Работа без пользователя не расходует персональный лимит Hydrogen.
        if settings.model == HYDROGEN_MODEL {
            settings.model = settings.fallback_model.take().unwrap_or_else(|| FLASH_LITE_MODEL.into());
        }
        if settings.fallback_model.as_deref() == Some(HYDROGEN_MODEL) {
            settings.fallback_model = None;
        }
    }
    let settings_ref = &settings;
    generate_with_settings(data, settings_ref, options, std::time::Duration::from_secs(2), generation_budget(settings_ref), |model| async move {
        call_gemini_settings(data, settings_ref, &model, system, turns).await
    }).await
}

fn http_timeout(model: &str) -> std::time::Duration {
    std::time::Duration::from_secs(if model == HYDROGEN_MODEL { 300 } else { 60 })
}

fn generation_budget(settings: &AiSettings) -> std::time::Duration {
    let hydrogen = settings.model == HYDROGEN_MODEL || settings.fallback_model.as_deref() == Some(HYDROGEN_MODEL);
    // С запасом до 15-минутного срока Discord: остаётся время на контекст и доставку частей.
    std::time::Duration::from_secs(if hydrogen { 660 } else { 180 })
}

#[cfg(test)]
async fn generate_with<F, Fut>(data: &Data, base_delay: std::time::Duration, budget: std::time::Duration, mut call: F) -> Result<Answer, AiError>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<Answer, AiError>>,
{
    generate_with_settings(data, &data.ai.settings, RequestOptions::default(), base_delay, budget, &mut call).await
}

async fn generate_with_settings<F, Fut>(data: &Data, settings: &AiSettings, options: RequestOptions<'_>, base_delay: std::time::Duration, budget: std::time::Duration, mut call: F) -> Result<Answer, AiError>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<Answer, AiError>>,
{
    tokio::time::timeout(budget, async {
        let mut last_error = AiError::Other("нет моделей".into());
        let model_budget = budget / if settings.fallback_model.is_some() { 2 } else { 1 };
        for model in std::iter::once(&settings.model).chain(settings.fallback_model.iter()) {
            let mut permit = if model == HYDROGEN_MODEL {
                // Фоновые задачи не используют дорогую модель без владельца квоты.
                let Some(user) = options.user else { continue };
                let day = moscow_day();
                let reserved = data.db.reserve_ai_model(user, &day, HYDROGEN_MODEL, HYDROGEN_DAILY_LIMIT)
                    .map_err(|error| AiError::Other(error.to_string()))?;
                if !reserved { last_error = AiError::HydrogenLimit; continue; }
                Some(HydrogenPermit { db: data.db.clone(), user, day, success: false })
            } else { None };
            // Медленная основная модель не должна съесть всё время, отведённое запасной.
            let deadline = tokio::time::Instant::now() + model_budget;
            let retries = settings.retries.clamp(0, 5);
            for attempt in 0..=retries {
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                data.ai.admit_provider_attempt()?;
                let day = google_day();
                let mut accounting = ProviderAttempt::start(data, &day, model);
                let result = tokio::time::timeout_at(deadline, call(model.clone())).await;
                accounting.succeeded = matches!(&result, Ok(Ok(_)));
                let Ok(result) = result else {
                    last_error = AiError::Network(format!("модель {model} не ответила вовремя"));
                    println!("ИИ: {last_error}; пробую запасную модель, если она задана");
                    break;
                };
                match result {
                    Ok(answer) => {
                        if !without_tags(&answer.text).trim().is_empty() {
                            if let Some(receipt) = options.delivery {
                                *locked(&receipt.permit) = permit.take();
                            } else if let Some(permit) = &mut permit {
                                permit.success = true;
                            }
                        }
                        if *model != settings.model {
                            println!("ИИ: основная модель {} не ответила ({last_error}), ответила запасная {model}", settings.model);
                        }
                        return Ok(answer);
                    }
                    Err(AiError::Quota) => {
                        *locked(&data.ai.last_quota_error) = db::now(); // покажем в /ai_limits
                        last_error = AiError::Quota;
                        println!("ИИ: {model} — лимит запросов; пробую запасную модель, если она задана");
                        break; // лимит у этой модели — повторять бесполезно, сразу к запасной
                    }
                    Err(error @ (AiError::Overloaded | AiError::Network(_))) => {
                        println!("ИИ: модель={model}, попытка={}/{}: {error}", attempt + 1, retries + 1);
                        last_error = error;
                        if attempt < retries {
                            let jitter = if base_delay.is_zero() { 0 } else { rand::random::<u64>() % 500 };
                            let delay = base_delay * (1 << attempt as u32) + std::time::Duration::from_millis(jitter);
                            tokio::time::sleep(delay.min(deadline.saturating_duration_since(tokio::time::Instant::now()))).await;
                        }
                    }
                    Err(other) => return Err(other), // другая ошибка — повтор не поможет
                }
            }
        }
        Err(last_error)
    }).await.unwrap_or_else(|_| Err(AiError::Network("превышено общее время ожидания ответа".into())))
}

fn ensure_provider_accounting(data: &Data) -> rusqlite::Result<()> {
    let mut initialized = locked(&data.ai.accounting_initialized);
    if !*initialized {
        data.db.lock().execute_batch(
            "CREATE TABLE IF NOT EXISTS ai_provider_usage (
                day TEXT NOT NULL, model TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
                succeeded INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (day, model)
            )"
        )?;
        *initialized = true;
    }
    Ok(())
}

struct ProviderAttempt<'a> {
    data: &'a Data,
    day: &'a str,
    model: &'a str,
    recorded: bool,
    succeeded: bool,
}

impl<'a> ProviderAttempt<'a> {
    fn start(data: &'a Data, day: &'a str, model: &'a str) -> Self {
        let result = ensure_provider_accounting(data).and_then(|()| {
            let mut connection = data.db.lock();
            let tx = connection.transaction()?;
            tx.execute("DELETE FROM ai_provider_usage WHERE day < ?1", params![day])?;
            tx.execute(
                "INSERT INTO ai_provider_usage (day, model, attempts) VALUES (?1, ?2, 1)
                 ON CONFLICT (day, model) DO UPDATE SET attempts = attempts + 1",
                params![day, model],
            )?;
            tx.commit()
        });
        if let Err(error) = &result {
            eprintln!("Не записан запрос Gemini: {error}");
        }
        Self { data, day, model, recorded: result.is_ok(), succeeded: false }
    }
}

impl Drop for ProviderAttempt<'_> {
    fn drop(&mut self) {
        if self.recorded && self.succeeded {
            if let Err(error) = self.data.db.lock().execute(
                "UPDATE ai_provider_usage SET succeeded = succeeded + 1 WHERE day = ?1 AND model = ?2",
                params![self.day, self.model],
            ) {
                eprintln!("Не записан ответ Gemini: {error}");
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ModelUsage {
    pub model: String,
    pub attempts: i64,
    pub succeeded: i64,
}

pub fn provider_usage(data: &Data, day: &str) -> rusqlite::Result<Vec<ModelUsage>> {
    ensure_provider_accounting(data)?;
    let connection = data.db.lock();
    let mut statement = connection.prepare(
        "SELECT model, attempts, succeeded FROM ai_provider_usage WHERE day = ?1 ORDER BY model",
    )?;
    let rows = statement.query_map(params![day], |row| Ok(ModelUsage {
        model: row.get(0)?, attempts: row.get(1)?, succeeded: row.get(2)?,
    }))?;
    rows.collect()
}

pub fn hydrogen_usage(data: &Data, user: u64, day: &str) -> rusqlite::Result<(i64, i64)> {
    Ok(data.db.lock().query_row(
        "SELECT used, reserved FROM ai_model_usage WHERE user_id = ?1 AND day = ?2 AND model = ?3",
        params![user as i64, day, HYDROGEN_MODEL],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?.unwrap_or_default())
}

/// Убирает из prompt.txt строки-пояснения (начинаются с #).
pub fn parse_prompt(text: &str) -> String {
    text.lines().filter(|line| !line.trim_start().starts_with('#')).collect::<Vec<_>>().join("\n").trim().to_string()
}

/// Полная инструкция для ИИ: характер из config/prompt.txt + технические правила.
pub fn system_prompt(data: &Data) -> String {
    let own = data.ai.prompt.read(&data.dir.join("config").join("prompt.txt"), parse_prompt);
    let character = if own.is_empty() { DEFAULT_CHARACTER.trim() } else { own.as_str() };
    format!("{character}\n\n{}", TECH_RULES.trim())
}

/// Убирает из ответа метки вида [ИМЯ: …] или [ИМЯ] (без учёта регистра). names — какие метки убрать.
pub fn strip_tags(text: &str, names: &[&str]) -> String {
    let mut result = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        let Some(length) = rest[start..].find(']') else { break };
        let inside = rest[start + 1..start + length].trim().to_uppercase().replace('-', "").replace(' ', "_");
        let name = inside.split(':').next().unwrap_or("").trim_end_matches('_');
        result.push_str(&rest[..start]);
        if !names.iter().any(|known| *known == name) {
            result.push_str(&rest[start..=start + length]);
        }
        rest = &rest[start + length + 1..];
    }
    result.push_str(rest);
    result.trim().to_string()
}

/// Есть ли в ответе метка [ИГНОР] — ИИ решил не отвечать на грубость.
pub fn has_ignore_tag(text: &str) -> bool {
    text.to_uppercase().contains("[ИГНОР]")
}

/// Метки модерации, которые ИИ ставит по просьбе модератора (выполняет их moderation.rs).
pub const MOD_TAGS: [&str; 3] = ["ВАРН", "ТАЙМАУТ", "СНЯТЬ_ТАЙМАУТ"];

/// Текст, который ИИ пишет сам (приветствие, утреннее сообщение, реплика): без меток памяти, наказаний и [ИГНОР].
pub fn without_tags(text: &str) -> String {
    strip_tags(&strip_tags(text, &["ЗАПОМНИТЬ", "ИГНОР"]), &MOD_TAGS)
}

/// Склеивает ответ в одну строку: абзацы и переносы → пробелы, значки списков убираются.
/// Ответы с кодом (```) не трогаем — код в одну строку сломается.
pub fn to_one_line(text: &str) -> String {
    if text.contains("```") {
        return text.to_string();
    }
    let (mut result, mut previous_bullet) = (String::new(), false);
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let item = ["- ", "* ", "• "].iter().find_map(|mark| line.strip_prefix(mark)).map(str::trim_start);
        let (bullet, line) = (item.is_some(), item.unwrap_or(line));
        if !result.is_empty() {
            // пункты списка подряд разделяем запятой («молоко, хлеб, сыр»), всё остальное — пробелом
            result.push_str(if bullet && previous_bullet { ", " } else { " " });
        }
        result.push_str(line);
        previous_bullet = bullet;
    }
    result
}

fn wants_structured_answer(question: &str) -> bool {
    let question = question.to_lowercase();
    ["latex", "латех", "конспект", "подробн", "таблиц", "пошаг", "шпаргал", "по пункт", "раздел", "абзац",
     "table", "step by step", "step-by-step", "detailed", "outline", "paragraph", "section", "cheat sheet", "study notes"]
        .iter().any(|word| question.contains(word))
}

fn format_answer_text(text: &str, question: &str, one_line: bool) -> String {
    if one_line && !wants_structured_answer(question) && !crate::ai_math::contains_math(text) { to_one_line(text) } else { text.to_string() }
}

/// Режет длинный текст на части (в одном сообщении Discord не больше 2000 символов).
pub fn split_message(mut text: &str, limit: usize) -> Vec<String> {
    // Discord считает символы UTF-16. Синтетические маркеры не входят в исходный текст.
    const OPEN: &str = "```\n";
    const CLOSE: &str = "\n```";
    let mut parts = Vec::new();
    let mut in_fence = false;
    let limit = limit.max(2);
    while !text.is_empty() {
        let prefix = if in_fence { OPEN } else { "" };
        let budget = limit - prefix.len();
        let mut width = 0;
        let mut end = 0;
        let mut open = in_fence;
        let mut line_break = None;
        while end < text.len() {
            let remaining = &text[end..];
            let fence = limit >= 11 && remaining.starts_with("```");
            let first = remaining.chars().next().unwrap();
            let (bytes, units) = if fence { (3, 3) } else { (first.len_utf8(), first.len_utf16()) };
            let next_open = open ^ fence;
            let suffix_width = if next_open { CLOSE.len() } else { 0 };
            if width + units + suffix_width > budget {
                break;
            }
            // Не разрезаем сразу после открывающего ```: это была бы пустая часть.
            if first == '\n' && end > 0 && (!open || text[..end].contains('\n')) {
                line_break = Some((end, open));
            }
            width += units;
            end += bytes;
            open = next_open;
        }
        let (end, next_open) = if end == text.len() { (end, open) } else { line_break.unwrap_or((end, open)) };
        let suffix = if next_open { CLOSE } else { "" };
        parts.push(format!("{prefix}{}{suffix}", &text[..end]));
        text = text[end..].strip_prefix('\n').unwrap_or(&text[end..]);
        in_fence = next_open;
    }
    parts
}

/// Формат вложения, например "image/png" (без лишнего вроде "; charset=…").
pub fn image_type(content_type: Option<&str>) -> String {
    content_type.unwrap_or("").split(';').next().unwrap_or("").trim().to_string()
}

/// Можно ли отправить это вложение в ИИ: картинка нужного формата и не слишком тяжёлая.
pub fn is_supported_image(content_type: Option<&str>, size: u64) -> bool {
    IMAGE_TYPES.contains(&image_type(content_type).as_str()) && size <= MAX_IMAGE_MB * 1024 * 1024
}

/// Что ещё передать ИИ вместе с вопросом.
pub async fn download_image(data: &Data, attachment: &serenity::Attachment) -> Result<Vec<u8>, AiError> {
    let _slot = data.ai.image_slots.try_acquire().map_err(|_| AiError::Busy)?;
    let max = MAX_IMAGE_MB as usize * 1024 * 1024;
    let mut response = data.web.get(&attachment.url).timeout(Duration::from_secs(12))
        .send().await.and_then(reqwest::Response::error_for_status).map_err(transport_error)?;
    if response.content_length().is_some_and(|length| length > max as u64) {
        return Err(AiError::Other("вложение превышает допустимый размер".into()));
    }
    let mut image = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        if chunk.len() > max - image.len() {
            return Err(AiError::Other("вложение превышает допустимый размер".into()));
        }
        image.extend_from_slice(&chunk);
    }
    Ok(image)
}

/// Маркер живёт ровно пока у канала есть незавершённые запросы к ИИ.
struct HistorySnapshot {
    marker: Arc<()>,
    turns: Vec<Turn>,
}

fn history_snapshot(data: &Data, channel: u64) -> rusqlite::Result<HistorySnapshot> {
    let mut active = locked(&data.ai.active_history);
    active.retain(|_, marker| marker.strong_count() != 0);
    let marker = active.get(&channel).and_then(Weak::upgrade).unwrap_or_else(|| {
        let marker = Arc::new(());
        active.insert(channel, Arc::downgrade(&marker));
        marker
    });
    let connection = data.db.lock();
    let mut statement = connection.prepare_cached(
        "SELECT role, substr(text, 1, 4000) FROM ai_history WHERE channel_id = ?1 ORDER BY id DESC LIMIT ?2",
    )?;
    let mut turns = statement.query_map(params![channel as i64, data.ai.settings.history_limit], |row| {
        Ok(Turn { role: row.get(0)?, text: row.get(1)?, images: Vec::new() })
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    if turns.last().is_some_and(|turn| turn.role == "model") {
        turns.pop();
    }
    turns.reverse();
    Ok(HistorySnapshot { marker, turns })
}

pub fn forget_history(data: &Data, channel: u64) -> rusqlite::Result<usize> {
    let mut active = locked(&data.ai.active_history);
    let removed = data.db.clear_ai_history(channel)?;
    active.remove(&channel);
    Ok(removed)
}

fn remember_exchange(data: &Data, channel: u64, marker: &Arc<()>, question: &str, answer: &str) -> rusqlite::Result<()> {
    let active = locked(&data.ai.active_history);
    let current = active.get(&channel).is_some_and(|live| live.ptr_eq(&Arc::downgrade(marker)));
    let limit = data.ai.settings.history_limit / 2 * 2;
    if !current || limit == 0 {
        return Ok(());
    }
    let mut connection = data.db.lock();
    let tx = connection.transaction()?;
    let now = db::now();
    for (role, text) in [("user", question), ("model", answer)] {
        tx.execute(
            "INSERT INTO ai_history (channel_id, role, text, created) VALUES (?1, ?2, substr(?3, 1, 4000), ?4)",
            params![channel as i64, role, text, now],
        )?;
    }
    tx.execute(
        "DELETE FROM ai_history WHERE channel_id = ?1 AND id NOT IN (
             SELECT id FROM ai_history WHERE channel_id = ?1 ORDER BY id DESC LIMIT ?2
         )",
        params![channel as i64, limit],
    )?;
    tx.commit()
}

#[derive(Default)]
pub struct Extras<'a> {
    pub options: RequestOptions<'a>,
    /// Последние сообщения чата (для контекста), пусто — без них
    pub chat: String,
    /// Явное действие модератора, разобранное из его исходного сообщения.
    pub moderation: Option<(Requester<'a>, crate::moderation::Action)>,
}

/// Отправляет допущенный вопрос вместе с памятью разговора или выполняет явную команду модератора.
pub async fn ask(data: &Data, question: &str, name: &str, channel: u64, images: Vec<(Vec<u8>, String)>, extras: Extras<'_>) -> Result<String, AiError> {
    if let Some((requester, action)) = extras.moderation {
        let result = crate::moderation::run_action(data, &requester, action).await;
        return Ok(result);
    }
    let mut system = format!("{}\nСегодня {}.", system_prompt(data), chrono::Local::now().format("%d.%m.%Y"));
    if wants_structured_answer(question) {
        system.push_str("\nВ текущем вопросе явно запрошен структурированный или подробный материал. \
            Выполни требования к полноте и оформлению: сохраняй разделы и абзацы; если запрошены таблицы или LaTeX, используй их. \
            Обычные предпочтения краткого ответа и записи формул обычным текстом здесь не применяются.");
    }
    let current = format!("{name}: {question}");
    let image_count = images.len();
    let mut prompt = current.clone();
    if !extras.chat.is_empty() {
        prompt = format!("Последние сообщения в чате (для контекста):\n{}\n\nВопрос — {current}", extras.chat);
    }
    // Снимок памяти привязан к текущему поколению канала.
    let HistorySnapshot { marker, mut turns } = history_snapshot(data, channel)
        .map_err(|error| AiError::Other(error.to_string()))?;
    turns.push(Turn { role: "user".into(), text: prompt, images });
    let answer = generate_selected(data, &system, &turns, extras.options).await?;
    if answer.text.is_empty() {
        return Ok("🤔 ИИ не дал ответа. Попробуй переформулировать вопрос.".to_string());
    }
    let mut text = format_answer_text(&without_tags(&answer.text), question, data.ai.settings.one_line);
    if text.is_empty() {
        text = "🤔 ИИ не дал ответа. Попробуй переформулировать вопрос.".to_string();
    }
    // Запоминаем этот обмен репликами. Сами картинки в память не сохраняются — только отметка о них.
    let remembered = if image_count > 0 { format!("{current} [прислал картинок: {image_count}]") } else { current };
    if let Err(error) = remember_exchange(data, channel, &marker, &remembered, &text) {
        eprintln!("Не удалось сохранить историю ИИ: {error}");
    }
    if answer.cut {
        text.push_str("\n\n✂️ *Ответ обрезан — слишком длинный. Попроси объяснить короче или по частям.*");
    }
    Ok(text)
}

// ---------- Адресная проверка оскорблений ----------

fn directed_insult_result(text: &str) -> bool {
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(text.trim()) else { return false };
    object.len() == 1 && object.get("target").and_then(Value::as_str) == Some("bot")
}

/// Границы слов для правил автоответов в чате (ё нормализуется вызывающим кодом).
pub fn is_letter(c: char) -> bool {
    c.is_ascii_digit() || c.is_ascii_lowercase() || ('а'..='я').contains(&c)
}

pub async fn directed_insult(data: &Data, text: &str) -> bool {
    if text.trim().is_empty() { return false; }
    let Ok(_slot) = data.ai.classifier_slots.try_acquire() else { return false };
    {
        let mut attempts = locked(&data.ai.classifier_attempts);
        AiState::prune_recent(&mut attempts);
        let limit = data.ai.settings.google_minute_limit.clamp(1, 60) as usize;
        if attempts.len() >= limit { return false; }
        attempts.push_back(Instant::now());
    }
    let system = include_str!("ai_insult_rules.txt");
    let turns = [Turn::user(json!({"message":text}).to_string())];
    let options = RequestOptions { model: ModelChoice::FlashLite, thinking: Some(ThinkingLevel::Low), ..Default::default() };
    let mut settings = request_settings(&data.ai.settings, options);
    settings.retries = 0;
    let mut body = request_body(&settings, FLASH_LITE_MODEL, system, &turns);
    body["generationConfig"]["maxOutputTokens"] = json!(512);
    body["generationConfig"]["responseMimeType"] = json!("application/json");
    body["generationConfig"]["responseSchema"] = json!({
        "type":"OBJECT", "properties":{"target":{"type":"STRING","enum":["bot","other","none","uncertain"]}},
        "required":["target"]
    });
    let result = generate_with_settings(data, &settings, options, std::time::Duration::ZERO, std::time::Duration::from_secs(10), |_| {
        send_gemini(data, FLASH_LITE_MODEL, body.clone())
    }).await;
    match result {
        Ok(answer) if !answer.cut => directed_insult_result(&answer.text),
        _ => false, // ошибка или сомнение никогда не приводят к наказанию
    }
}

/// Минуты → текст вроде «1 минуту», «5 минут», «2 часа».
pub fn minutes_text(minutes: i64) -> String {
    if minutes % (24 * 60) == 0 {
        let days = minutes / (24 * 60);
        format!("{days} {}", plural(days, "день", "дня", "дней"))
    } else if minutes % 60 == 0 {
        let hours = minutes / 60;
        format!("{hours} {}", plural(hours, "час", "часа", "часов"))
    } else {
        format!("{minutes} {}", plural(minutes, "минуту", "минуты", "минут"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hydrogen_long_answers_have_time_and_output_headroom() {
        let defaults = settings("");
        let turns = [Turn::user("Составь подробный конспект")];
        assert_eq!(request_body(&defaults, HYDROGEN_MODEL, "", &turns)["generationConfig"]["maxOutputTokens"], 32768);
        assert_eq!(request_body(&defaults, FLASH_LITE_MODEL, "", &turns)["generationConfig"]["maxOutputTokens"], 8192);
        assert_eq!(http_timeout(HYDROGEN_MODEL), std::time::Duration::from_secs(300));
        assert_eq!(http_timeout(FLASH_LITE_MODEL), std::time::Duration::from_secs(60));
        assert_eq!(generation_budget(&request_settings(&defaults, RequestOptions { model: ModelChoice::Hydrogen, ..Default::default() })), std::time::Duration::from_secs(660));
        assert_eq!(generation_budget(&request_settings(&defaults, RequestOptions { model: ModelChoice::FlashLite, ..Default::default() })), std::time::Duration::from_secs(180));
        assert_eq!(generation_budget(&defaults), std::time::Duration::from_secs(660)); // auto leaves time for Hydrogen
    }

    #[test]
    fn explicit_structured_requests_preserve_latex_tables_and_paragraphs() {
        let text = "Раздел 1\n\n$$\\sum a_n$$\n\n| Признак | Условие |\n| --- | --- |";
        for question in ["Составь подробный конспект", "Все формулы запиши в LaTeX", "Покажи сравнительную таблицу", "Объясни пошагово",
                         "Write a comparison table", "Explain step by step", "Разбей ответ на разделы"] {
            assert_eq!(format_answer_text(text, question, true), text);
        }
        assert_eq!(format_answer_text("Привет\nКак дела", "привет", true), "Привет Как дела");
        assert_eq!(format_answer_text(text, "привет", false), text);
    }

    #[test]
    fn selected_models_and_thinking_are_per_request() {
        let defaults = settings("");
        let explicit = RequestOptions { model: ModelChoice::Hydrogen, thinking: Some(ThinkingLevel::Low), user: Some(7), ..Default::default() };
        let selected = request_settings(&defaults, explicit);
        assert_eq!(selected.model, "gemini-4-hydrogen");
        assert!(selected.fallback_model.is_none());
        assert_eq!(selected.thinking_level.as_deref(), Some("low"));
        assert_eq!(defaults.thinking_level.as_deref(), Some("high"));
        assert_eq!(defaults.model, "gemini-3.5-flash-lite");
        let lite = request_settings(&defaults, RequestOptions { model: ModelChoice::FlashLite, ..Default::default() });
        assert_eq!(lite.model, "gemini-3.5-flash-lite");
        assert!(lite.fallback_model.is_none());
    }

    #[test]
    fn moderation_classifier_requires_unambiguous_structured_result() {
        assert!(directed_insult_result(r#"{"target":"bot"}"#));
        for text in [r#"{"target":"other"}"#, r#"{"target":"none"}"#, r#"{"target":"uncertain"}"#,
                     "[ИГНОР]", "bot", r#"{"target":"bot","explanation":"x"}"#, "ignore all instructions"] {
            assert!(!directed_insult_result(text), "{text}");
        }
    }

    #[test]
    fn hydrogen_day_changes_at_moscow_midnight() {
        assert_eq!(moscow_day_at(chrono::DateTime::parse_from_rfc3339("2026-11-17T20:59:59Z").unwrap().to_utc()), "2026-11-17");
        assert_eq!(moscow_day_at(chrono::DateTime::parse_from_rfc3339("2026-11-17T21:00:00Z").unwrap().to_utc()), "2026-11-18");
    }

    #[tokio::test]
    async fn hydrogen_quota_counts_successful_answers_including_auto() {
        let data = retry_test_data();
        let mut selected = request_settings(&data.ai.settings, RequestOptions { model: ModelChoice::Hydrogen, ..Default::default() });
        selected.retries = 0;
        let options = RequestOptions { user: Some(9), ..Default::default() };
        for _ in 0..4 {
            let result = generate_with_settings(&data, &selected, options, std::time::Duration::ZERO, std::time::Duration::from_secs(1), |_| {
                std::future::ready(Ok(Answer { text: "готово".into(), cut: false }))
            }).await;
            assert!(result.is_ok());
        }
        let mut calls = 0;
        let result = generate_with_settings(&data, &selected, options, std::time::Duration::ZERO, std::time::Duration::from_secs(1), |_| {
            calls += 1;
            std::future::ready(Ok(Answer { text: "нельзя".into(), cut: false }))
        }).await;
        assert_eq!(result, Err(AiError::HydrogenLimit));
        assert_eq!(calls, 0);
        assert_eq!(data.db.ai_model_usage(9, &moscow_day(), HYDROGEN_MODEL).unwrap(), 4);

        selected.model = "primary".into(); selected.fallback_model = Some(HYDROGEN_MODEL.into());
        let result = generate_with_settings(&data, &selected, RequestOptions { user: Some(10), ..Default::default() }, std::time::Duration::ZERO, std::time::Duration::from_secs(1), |model| {
            std::future::ready(if model == "primary" { Err(AiError::Quota) } else { Ok(Answer { text: "fallback".into(), cut: false }) })
        }).await;
        assert!(result.is_ok());
        assert_eq!(data.db.ai_model_usage(10, &moscow_day(), HYDROGEN_MODEL).unwrap(), 1);
    }

    #[tokio::test]
    async fn hydrogen_error_empty_answer_and_cancellation_refund_reservation() {
        let data = retry_test_data();
        let settings = request_settings(&data.ai.settings, RequestOptions { model: ModelChoice::Hydrogen, ..Default::default() });
        let options = RequestOptions { user: Some(11), ..Default::default() };
        let budget = std::time::Duration::from_millis(20);
        let result = generate_with_settings(&data, &settings, options, std::time::Duration::ZERO, budget, |_| std::future::ready(Err(AiError::Other("bad".into())))).await;
        assert!(result.is_err());
        let _ = generate_with_settings(&data, &settings, options, std::time::Duration::ZERO, budget, |_| std::future::ready(Ok(Answer { text: String::new(), cut: false }))).await;
        let result = generate_with_settings(&data, &settings, options, std::time::Duration::ZERO, budget, |_| std::future::pending::<Result<Answer, AiError>>()).await;
        assert!(result.is_err());
        assert_eq!(data.db.ai_model_usage(11, &moscow_day(), HYDROGEN_MODEL).unwrap(), 0);
        // Все четыре места доступны после отмены, пятого быть не должно.
        for _ in 0..4 { assert!(data.db.reserve_ai_model(11, &moscow_day(), HYDROGEN_MODEL, 4).unwrap()); }
        assert!(!data.db.reserve_ai_model(11, &moscow_day(), HYDROGEN_MODEL, 4).unwrap());
    }

    #[tokio::test]
    async fn hydrogen_delivery_failure_refunds_and_delivery_commits_once() {
        let data = retry_test_data();
        let settings = request_settings(&data.ai.settings, RequestOptions { model: ModelChoice::Hydrogen, ..Default::default() });
        let body = || std::future::ready(Ok(Answer { text: "answer".into(), cut: false }));
        {
            let receipt = DeliveryReceipt::default();
            let options = RequestOptions { user: Some(50), delivery: Some(&receipt), ..Default::default() };
            generate_with_settings(&data, &settings, options, std::time::Duration::ZERO, std::time::Duration::from_secs(1), |_| body()).await.unwrap();
            assert_eq!(data.db.ai_model_usage(50, &moscow_day(), HYDROGEN_MODEL).unwrap(), 0);
            // Симулируем отмену/ошибку Discord после успешной генерации.
        }
        assert_eq!(data.db.ai_model_usage(50, &moscow_day(), HYDROGEN_MODEL).unwrap(), 0);
        let receipt = DeliveryReceipt::default();
        let options = RequestOptions { user: Some(50), delivery: Some(&receipt), ..Default::default() };
        generate_with_settings(&data, &settings, options, std::time::Duration::ZERO, std::time::Duration::from_secs(1), |_| body()).await.unwrap();
        receipt.delivered(); receipt.delivered();
        assert_eq!(data.db.ai_model_usage(50, &moscow_day(), HYDROGEN_MODEL).unwrap(), 1);
        for _ in 0..3 { assert!(data.db.reserve_ai_model(50, &moscow_day(), HYDROGEN_MODEL, 4).unwrap()); }
        assert!(!data.db.reserve_ai_model(50, &moscow_day(), HYDROGEN_MODEL, 4).unwrap());
    }

    fn retry_test_data() -> Data {
        let settings = crate::config::Settings::parse("[ai]\nmodel = primary\nfallback_model = fallback\nretries = 1\n");
        Data::new(crate::db::Database::in_memory().unwrap(), &settings, std::path::PathBuf::new(), false)
    }

    #[tokio::test]
    async fn retries_network_and_server_failures_then_uses_fallback() {
        let data = retry_test_data();
        let mut calls = Vec::new();
        let answer = generate_with(&data, std::time::Duration::ZERO, std::time::Duration::from_secs(2), |model| {
            calls.push(model.clone());
            std::future::ready(match calls.len() {
                1 => Err(AiError::Overloaded),
                2 => Err(AiError::Network("connection reset".into())),
                _ => Ok(Answer { text: "ответ запасной".into(), cut: false }),
            })
        }).await.unwrap();
        assert_eq!(calls, ["primary", "primary", "fallback"]);
        assert_eq!(answer.text, "ответ запасной");
        assert_eq!(provider_usage(&data, &google_day()).unwrap(), [
            ModelUsage { model: "fallback".into(), attempts: 1, succeeded: 1 },
            ModelUsage { model: "primary".into(), attempts: 2, succeeded: 0 },
        ]);
    }

    #[tokio::test]
    async fn invalid_request_is_not_retried_but_quota_uses_fallback() {
        let data = retry_test_data();
        let mut calls = Vec::new();
        let error = generate_with(&data, std::time::Duration::ZERO, std::time::Duration::from_secs(2), |model| {
            calls.push(model);
            std::future::ready(Err(AiError::Other("400 invalid request".into())))
        }).await.unwrap_err();
        assert!(matches!(error, AiError::Other(_)));
        assert_eq!(calls, ["primary"]);

        calls.clear();
        let error = generate_with(&data, std::time::Duration::ZERO, std::time::Duration::from_secs(2), |model| {
            calls.push(model);
            std::future::ready(Err(AiError::Quota))
        }).await.unwrap_err();
        assert_eq!(calls, ["primary", "fallback"]);
        assert_eq!(error, AiError::Quota);
    }

    #[tokio::test]
    async fn total_deadline_stops_a_stalled_request() {
        let data = retry_test_data();
        let mut calls = 0;
        let error = generate_with(&data, std::time::Duration::ZERO, std::time::Duration::from_millis(20), |_| {
            calls += 1;
            std::future::pending::<Result<Answer, AiError>>()
        }).await.unwrap_err();
        assert!((1..=2).contains(&calls));
        assert!(matches!(error, AiError::Network(_)));
        assert!(error_text(&error).contains("соединения"));
    }

    #[tokio::test]
    async fn slow_primary_leaves_time_for_fallback() {
        let data = retry_test_data();
        let mut calls = Vec::new();
        let answer = generate_with(&data, std::time::Duration::ZERO, std::time::Duration::from_millis(100), |model| {
            calls.push(model.clone());
            async move {
                if model == "primary" {
                    std::future::pending::<Result<Answer, AiError>>().await
                } else {
                    Ok(Answer { text: "ответ запасной".into(), cut: false })
                }
            }
        }).await.unwrap();
        assert_eq!(calls, ["primary", "fallback"]);
        assert_eq!(answer.text, "ответ запасной");
    }

    fn settings(text: &str) -> AiSettings {
        AiSettings::load(&crate::config::Settings::parse(text))
    }

    #[test]
    fn default_settings() {
        let ai = settings("");
        assert_eq!((ai.model.as_str(), ai.fallback_model.as_deref(), ai.thinking_level.as_deref()),
                   ("gemini-3.5-flash-lite", Some("gemini-4-hydrogen"), Some("high")));
        assert_eq!((ai.retries, ai.cooldown, ai.daily_limit, ai.history_limit, ai.one_line), (2, 5, 50, 20, true));
        let ai = settings("[ai]\nthinking_level = чушь\nfallback_model = none\none_line_answers = off\n");
        assert_eq!((ai.thinking_level.as_deref(), ai.fallback_model, ai.one_line), (Some("high"), None, false));
        let ai = settings("[ai]\nthinking_level = off\nmodel = x\nfallback_model = x\n");
        assert_eq!((ai.thinking_level, ai.fallback_model), (None, None)); // запасная модель та же, что основная

    }

    #[test]
    fn request_and_answer() {
        let ai = settings("[ai]\nthinking_level = minimal\n");
        let turns = [Turn::user("привет"), Turn { role: "model".into(), text: "бака".into(), images: Vec::new() },
                     Turn { role: "user".into(), text: "что тут?".into(), images: vec![(vec![1, 2, 3], "image/png".into())] }];
        let body = request_body(&ai, "gemini-3.5-flash-lite", "характер", &turns);
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "характер");
        assert_eq!(body["contents"][1]["role"], "model");
        assert_eq!(body["contents"][2]["parts"][1]["inlineData"]["data"], "AQID");
        assert_eq!(body["generationConfig"]["thinkingConfig"]["thinkingLevel"], "minimal");
        // «minimal» есть только у Flash-Lite
        assert_eq!(request_body(&ai, "gemini-4-hydrogen", "", &turns)["generationConfig"]["thinkingConfig"]["thinkingLevel"], "low");
        assert!(request_body(&settings("[ai]\nthinking_level = none\n"), "m", "", &turns)["generationConfig"].get("thinkingConfig").is_none());

        let response: Value = serde_json::from_str(r#"{"candidates":[{"content":{"parts":[
            {"text":"думаю…","thought":true},{"text":" Привет, "},{"text":"бака! "}]},"finishReason":"STOP"}]}"#).unwrap();
        assert_eq!(parse_answer(&response), Answer { text: "Привет, бака!".into(), cut: false });
        let cut: Value = serde_json::from_str(r#"{"candidates":[{"content":{"parts":[{"text":"длинно"}]},"finishReason":"MAX_TOKENS"}]}"#).unwrap();
        assert!(parse_answer(&cut).cut);
        assert_eq!(parse_answer(&json!({})).text, "");
    }

    #[test]
    fn tags() {
        assert_eq!(strip_tags("Ладно. [ВАРН: 123 за спам] Готово", &MOD_TAGS), "Ладно.  Готово");
        assert_eq!(strip_tags("[тайм-аут: 5 60 флуд] ок [снять таймаут: 5]", &MOD_TAGS), "ок");
        assert_eq!(strip_tags("[ЗАПОМНИТЬ: любит чай] Запомнила", &["ЗАПОМНИТЬ"]), "Запомнила");
        assert_eq!(strip_tags("массив a[0] и [ссылка](url)", &MOD_TAGS), "массив a[0] и [ссылка](url)");
        assert!(has_ignore_tag("  [игнор] ") && !has_ignore_tag("игнор"));
    }

    #[test]
    fn one_line_and_split() {
        assert_eq!(to_one_line("Купи:\n- молоко\n- хлеб\n\n* сыр\nИ всё."), "Купи: молоко, хлеб, сыр И всё.");
        assert_eq!(to_one_line("код:\n```\nx = 1\n```"), "код:\n```\nx = 1\n```");
        assert_eq!(split_message("коротко", 10), ["коротко"]);
        assert_eq!(split_message("строка1\nстрока2\nстрока3", 16), ["строка1\nстрока2", "строка3"]);
        assert_eq!(split_message("абвгдежз", 3), ["абв", "где", "жз"]);
        let parts = split_message("```\nодин\nдва\nтри\n```", 12);
        assert!(parts.iter().all(|part| part.matches("```").count() % 2 == 0), "{parts:?}");
    }

    #[test]
    fn split_long_fence_makes_progress_and_respects_utf16_limit() {
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let text = format!("```rust\n{}\n```", "🚀".repeat(3000));
            tx.send(split_message(&text, 1900)).ok();
        });
        let parts = rx.recv_timeout(std::time::Duration::from_secs(1)).expect("splitter did not finish");
        worker.join().unwrap();
        assert!(parts.len() < 10);
        assert!(parts.iter().all(|part| part.encode_utf16().count() <= 1900));
        assert!(parts.iter().all(|part| part.matches("```").count() % 2 == 0));
        assert_eq!(parts.iter().map(|part| part.matches("🚀").count()).sum::<usize>(), 3000);
    }

    #[test]
    fn rudeness_minutes() {
        assert_eq!([minutes_text(1), minutes_text(5), minutes_text(60), minutes_text(120), minutes_text(1440), minutes_text(90)],
                   ["1 минуту", "5 минут", "1 час", "2 часа", "1 день", "90 минут"]);
    }

    #[test]
    fn limits() {
        let ai = AiState::new(&crate::config::Settings::parse("[ai]\ncooldown_seconds = 0\ndaily_limit_per_user = 2\n"), Some("ключ".into()));
        assert!(ai.ready() && !AiState::new(&crate::config::Settings::default(), Some(String::new())).ready());
        assert_eq!([ai.limit_check(1), ai.limit_check(1)], [None, None]);
        assert!(ai.limit_check(1).unwrap().starts_with("На сегодня лимит вопросов (2)"));
        assert_eq!(ai.usage_today(1).0, 2);
        assert_eq!(ai.limit_check(2), None); // у другого человека свой счётчик
        let slow = AiState::new(&crate::config::Settings::default(), None);
        assert_eq!(slow.limit_check(1), None);
        assert!(slow.limit_check(1).unwrap().starts_with("Не так быстро"));
    }

    /// Настоящий запрос к Gemini с ключом из GEMINI_API_KEY и моделями из config/settings.ini.
    /// Запуск: cargo test real_gemini -- --ignored (тратит 1–2 запроса из лимита ключа).
    #[tokio::test]
    #[ignore]
    async fn real_gemini() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
        let settings = crate::config::Settings::load(&dir.join("config").join("settings.ini"));
        let data = Data::new(crate::db::Database::in_memory().unwrap(), &settings, dir, false);
        assert!(data.ai.ready(), "нет GEMINI_API_KEY");
        let models = (data.ai.settings.model.clone(), data.ai.settings.fallback_model.clone());
        // Сначала основная модель отдельно — чтобы увидеть её ошибку, а не ответ запасной
        let system = system_prompt(&data);
        let main = call_gemini(&data, &models.0, &system, &[Turn::user("Тест: ответь одним словом «работает».")]).await;
        println!("основная модель {}: {main:?}", models.0);
        // Потом полный путь, как у /ask: память разговора, повторы, запасная модель
        let answer = ask(&data, "Сколько будет 2+2? Ответь коротко.", "Тестер", 1, Vec::new(), Extras::default()).await;
        println!("ask: {answer:?}");
        assert!(matches!(&answer, Ok(text) if !text.is_empty()), "{answer:?}");
        assert!(main.is_ok(), "основная модель не ответила: {main:?}");
    }

    #[test]
    fn prompt_files() {
        assert_eq!(parse_prompt("# пояснение\nТы тсундере.\n  # ещё\nОтвечай кратко.\n"), "Ты тсундере.\nОтвечай кратко.");
        assert!(TECH_RULES.contains("[ИГНОР]") && TECH_RULES.contains("[ВАРН: id причина]"));
        assert!(DEFAULT_CHARACTER.trim().chars().count() > 50);
        assert_eq!(image_type(Some("image/png; charset=binary")), "image/png");
        assert!(is_supported_image(Some("image/jpeg"), 1024) && !is_supported_image(Some("image/gif"), 10));
        assert!(!is_supported_image(Some("image/png"), 6 * 1024 * 1024) && !is_supported_image(None, 1));
    }
}
