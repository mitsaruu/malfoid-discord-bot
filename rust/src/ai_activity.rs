//! Private Gemini conversations for the Discord Activity.

use crate::{ai, db::Database, Data};
use axum::http::StatusCode;
use base64::{engine::general_purpose::STANDARD, Engine};
use image::{ImageFormat, ImageReader, Limits};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use std::{future::Future, io::Cursor, sync::Arc, time::Duration};
use tokio::sync::Semaphore;

const SEND_TIMEOUT: Duration = Duration::from_secs(95);
const AUTO_PRIMARY_TIMEOUT: Duration = Duration::from_secs(35);
const MAX_PROMPT_CHARS: usize = 16_000;
const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_IMAGE_BASE64: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;
const MAX_IMAGE_DIMENSION: u32 = 4096;
const CONTEXT_EXCHANGES: i64 = 10;
static IMAGE_VALIDATION_SLOTS: Semaphore = Semaphore::const_new(4);

#[derive(Debug)]
pub enum Error {
    Invalid(&'static str),
    NotFound,
    InProgress,
    Quota,
    Timeout,
    Provider(ai::AiError),
    Storage(rusqlite::Error),
}

impl Error {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Invalid(_) => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::InProgress => StatusCode::CONFLICT,
            Self::Quota => StatusCode::TOO_MANY_REQUESTS,
            Self::Timeout => StatusCode::GATEWAY_TIMEOUT,
            Self::Provider(ai::AiError::Busy | ai::AiError::Quota | ai::AiError::Overloaded) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::Provider(_) => StatusCode::BAD_GATEWAY,
            Self::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn message(&self) -> &'static str {
        match self {
            Self::Invalid(message) => message,
            Self::NotFound => "Чат или изображение не найдены.",
            Self::InProgress => "Предыдущий запрос ещё выполняется. Попробуй чуть позже.",
            Self::Quota => "Лимит Hydrogen на сегодня исчерпан. Сброс в 00:00 МСК. Можно выбрать Flash-Lite.",
            Self::Timeout => "Не удалось дождаться ответа ИИ. Попробуй ещё раз.",
            Self::Provider(ai::AiError::Other(_)) => {
                "Gemini не смог обработать запрос с выбранными настройками. Попробуй позже."
            }
            Self::Provider(error) => ai::error_text(error),
            Self::Storage(_) => "Не удалось обработать запрос к чату.",
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Provider(error) => Some(error),
            Self::Storage(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error)
    }
}

#[derive(Debug)]
pub struct Request {
    request_id: String,
    chat_id: Option<i64>,
    prompt: String,
    image: Option<(String, String)>,
    model: ai::ModelChoice,
    thinking: ai::ThinkingLevel,
}

impl Request {
    pub fn from_json(body: &Value) -> Result<Self, Error> {
        let request_id = body["request_id"]
            .as_str()
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 128
                    && !id.chars().any(|c| c.is_control() || c.is_whitespace())
            })
            .ok_or(Error::Invalid("Некорректный идентификатор запроса."))?;
        let chat_id = match body.get("chat_id") {
            None | Some(Value::Null) => None,
            Some(id) => Some(
                id.as_str()
                    .and_then(parse_id)
                    .ok_or(Error::Invalid("Некорректный идентификатор чата."))?,
            ),
        };
        let prompt = body["prompt"]
            .as_str()
            .map(str::trim)
            .filter(|prompt| prompt.chars().take(MAX_PROMPT_CHARS + 1).count() <= MAX_PROMPT_CHARS)
            .ok_or(Error::Invalid(
                "Текст сообщения должен содержать не более 16 000 символов.",
            ))?;
        let image = match body.get("image") {
            None | Some(Value::Null) => None,
            Some(image) => {
                let mime = image["mime"]
                    .as_str()
                    .filter(|mime| image_format(mime).is_some())
                    .ok_or(Error::Invalid(
                        "Поддерживаются изображения PNG, JPEG и WebP.",
                    ))?;
                let encoded = image["data"]
                    .as_str()
                    .filter(|data| !data.is_empty() && data.len() <= MAX_IMAGE_BASE64)
                    .ok_or(Error::Invalid(
                        "Размер изображения не должен превышать 4 МиБ.",
                    ))?;
                Some((mime.to_owned(), encoded.to_owned()))
            }
        };
        if prompt.is_empty() && image.is_none() {
            return Err(Error::Invalid(
                "Введите сообщение или прикрепите изображение.",
            ));
        }
        // Pages cached before the selector keep their previous Flash-Lite/medium behavior.
        let model = match body.get("model") {
            None => ai::ModelChoice::FlashLite,
            Some(value) => match value.as_str() {
                Some("auto") => ai::ModelChoice::Auto,
                Some("flash_lite") => ai::ModelChoice::FlashLite,
                Some("hydrogen") => ai::ModelChoice::Hydrogen,
                _ => return Err(Error::Invalid("Выбери доступную модель ИИ.")),
            },
        };
        let thinking = match body.get("thinking") {
            None => ai::ThinkingLevel::Medium,
            Some(value) => match value.as_str() {
                Some("minimal") if model == ai::ModelChoice::FlashLite => ai::ThinkingLevel::Minimal,
                Some("low") => ai::ThinkingLevel::Low,
                Some("medium") => ai::ThinkingLevel::Medium,
                Some("high") => ai::ThinkingLevel::High,
                _ => return Err(Error::Invalid("Выбери уровень рассуждений, доступный для этой модели.")),
            },
        };
        Ok(Self {
            request_id: request_id.to_owned(),
            chat_id,
            prompt: prompt.to_owned(),
            image,
            model,
            thinking,
        })
    }
}

fn parse_id(id: &str) -> Option<i64> {
    id.parse::<i64>().ok().filter(|id| *id > 0)
}

fn image_format(mime: &str) -> Option<ImageFormat> {
    match mime {
        "image/png" => Some(ImageFormat::Png),
        "image/jpeg" => Some(ImageFormat::Jpeg),
        "image/webp" => Some(ImageFormat::WebP),
        _ => None,
    }
}

fn decode_image(mime: String, encoded: &str) -> Result<(Vec<u8>, String), Error> {
    let invalid = || Error::Invalid("Повреждённое изображение или неверный формат.");
    let bytes = STANDARD.decode(encoded).map_err(|_| invalid())?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(Error::Invalid(
            "Размер изображения не должен превышать 4 МиБ.",
        ));
    }
    let format = image::guess_format(&bytes).map_err(|_| invalid())?;
    if Some(format) != image_format(&mime) {
        return Err(invalid());
    }
    let reader = || ImageReader::with_format(Cursor::new(&bytes), format);
    let (width, height) = reader().into_dimensions().map_err(|_| invalid())?;
    if !(1..=MAX_IMAGE_DIMENSION).contains(&width) || !(1..=MAX_IMAGE_DIMENSION).contains(&height) {
        return Err(Error::Invalid(
            "Размеры изображения не должны превышать 4096 × 4096.",
        ));
    }
    let mut reader = reader();
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(160 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().map_err(|_| invalid())?;
    Ok((bytes, mime))
}

fn quota(connection: &Connection, user: u64) -> Result<Value, Error> {
    let now = chrono::Utc::now();
    let (used, reserved) = Database::ai_model_counts_on(
        connection,
        user,
        &ai::moscow_day_at(now),
        ai::HYDROGEN_MODEL,
    )?;
    Ok(json!({
        "limit": ai::HYDROGEN_DAILY_LIMIT,
        "remaining": (ai::HYDROGEN_DAILY_LIMIT - used - reserved).max(0),
        "reset_at": ai::moscow_reset_time_at(now),
    }))
}

pub fn overview(data: &Data, user: u64) -> Result<Value, Error> {
    let connection = data.db.lock();
    let mut statement = connection.prepare(
        "SELECT id, title FROM ai_activity_chats WHERE user_id = ?1 ORDER BY updated DESC, id DESC",
    )?;
    let chats = statement
        .query_map([user as i64], |row| {
            Ok(json!({ "id": row.get::<_, i64>(0)?.to_string(), "title": row.get::<_, String>(1)? }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({ "chats": chats, "quota": quota(&connection, user)? }))
}

pub fn chat(data: &Data, user: u64, chat_id: &str) -> Result<Value, Error> {
    read_chat(data, user, parse_id(chat_id).ok_or(Error::NotFound)?)
}

fn chat_title(connection: &Connection, user: u64, chat_id: i64) -> Result<String, Error> {
    connection
        .query_row(
            "SELECT title FROM ai_activity_chats WHERE id = ?1 AND user_id = ?2",
            params![chat_id, user as i64],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(Error::NotFound)
}

fn read_chat(data: &Data, user: u64, chat_id: i64) -> Result<Value, Error> {
    let connection = data.db.lock();
    let title = chat_title(&connection, user, chat_id)?;
    let mut statement = connection.prepare(
        "SELECT id, prompt, answer, image_mime IS NOT NULL FROM ai_activity_turns
         WHERE chat_id = ?1 AND user_id = ?2 ORDER BY id",
    )?;
    let turns = statement
        .query_map(params![chat_id, user as i64], |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0)?.to_string(),
                "prompt": row.get::<_, String>(1)?,
                "answer": row.get::<_, String>(2)?,
                "image": row.get::<_, bool>(3)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({
        "chat": { "id": chat_id.to_string(), "title": title },
        "turns": turns,
        "quota": quota(&connection, user)?,
    }))
}

pub fn image(data: &Data, user: u64, turn_id: &str) -> Result<(String, Vec<u8>), Error> {
    let turn_id = parse_id(turn_id).ok_or(Error::NotFound)?;
    data.db
        .lock()
        .query_row(
            "SELECT image_mime, image_data FROM ai_activity_turns
         WHERE id = ?1 AND user_id = ?2 AND image_data IS NOT NULL",
            params![turn_id, user as i64],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or(Error::NotFound)
}

enum Admission<'a> {
    Replay(i64),
    Pending(Pending<'a>),
}

struct Pending<'a> {
    db: &'a Database,
    user: u64,
    request_id: &'a str,
    day: String,
    hydrogen: bool,
    active: bool,
}

fn admit<'a>(
    db: &'a Database,
    user: u64,
    request_id: &'a str,
    chat_id: Option<i64>,
    day: String,
    hydrogen: bool,
) -> Result<Admission<'a>, Error> {
    let mut connection = db.lock();
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(chat_id) = tx
        .query_row(
            "SELECT chat_id FROM ai_activity_turns WHERE user_id = ?1 AND request_id = ?2",
            params![user as i64, request_id],
            |row| row.get(0),
        )
        .optional()?
    {
        return Ok(Admission::Replay(chat_id));
    }
    if let Some(chat_id) = chat_id {
        chat_title(&tx, user, chat_id)?;
    }
    if tx.execute(
        "INSERT INTO ai_activity_pending (user_id, request_id, chat_id, day)
         VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
        params![user as i64, request_id, chat_id, day],
    )? == 0
    {
        return Err(Error::InProgress);
    }
    if hydrogen && !Database::reserve_ai_model_on(
        &tx,
        user,
        &day,
        ai::HYDROGEN_MODEL,
        ai::HYDROGEN_DAILY_LIMIT,
    )? {
        return Err(Error::Quota);
    }
    tx.commit()?;
    Ok(Admission::Pending(Pending {
        db,
        user,
        request_id,
        day,
        hydrogen,
        active: true,
    }))
}

impl Pending<'_> {
    fn reserve_hydrogen(&mut self, day: String) -> Result<(), Error> {
        let mut connection = self.db.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !Database::reserve_ai_model_on(&tx, self.user, &day, ai::HYDROGEN_MODEL, ai::HYDROGEN_DAILY_LIMIT)? {
            return Err(Error::Quota);
        }
        if day != self.day && tx.execute(
            "UPDATE ai_activity_pending SET day = ?1 WHERE user_id = ?2 AND request_id = ?3 AND day = ?4",
            params![day, self.user as i64, self.request_id, self.day],
        )? != 1 {
            return Err(rusqlite::Error::QueryReturnedNoRows.into());
        }
        tx.commit()?;
        self.day = day;
        self.hydrogen = true;
        Ok(())
    }

    fn finish(&self, connection: &Connection, success: bool) -> rusqlite::Result<()> {
        let found = connection.execute(
            "DELETE FROM ai_activity_pending WHERE user_id = ?1 AND request_id = ?2 AND day = ?3",
            params![self.user as i64, self.request_id, self.day],
        )?;
        if found == 1
            && (!self.hydrogen || Database::finish_ai_model_on(
                connection,
                self.user,
                &self.day,
                ai::HYDROGEN_MODEL,
                success,
            )?)
        {
            Ok(())
        } else {
            Err(rusqlite::Error::QueryReturnedNoRows)
        }
    }

    fn save(
        mut self,
        chat_id: Option<i64>,
        prompt: &str,
        answer: &str,
        image: Option<&(Vec<u8>, String)>,
        now: impl FnOnce() -> chrono::DateTime<chrono::Utc>,
    ) -> Result<i64, Error> {
        let mut connection = self.db.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let saved_at = now();
        let saved_day = ai::moscow_day_at(saved_at);
        let updated = saved_at.timestamp_micros() as f64 / 1_000_000.0;
        let chat_id = if let Some(chat_id) = chat_id {
            if tx.execute(
                "UPDATE ai_activity_chats SET updated = ?1 WHERE id = ?2 AND user_id = ?3",
                params![updated, chat_id, self.user as i64],
            )? != 1
            {
                return Err(Error::NotFound);
            }
            chat_id
        } else {
            let title = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
            let title = if title.is_empty() {
                "Изображение".into()
            } else {
                title.chars().take(80).collect::<String>()
            };
            tx.execute(
                "INSERT INTO ai_activity_chats (user_id, title, updated) VALUES (?1, ?2, ?3)",
                params![self.user as i64, title, updated],
            )?;
            tx.last_insert_rowid()
        };
        tx.execute(
            "INSERT INTO ai_activity_turns (user_id, request_id, chat_id, prompt, answer, image_mime, image_data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                self.user as i64, self.request_id, chat_id, prompt, answer,
                image.map(|(_, mime)| mime.as_str()), image.map(|(bytes, _)| bytes.as_slice()),
            ],
        )?;
        if !self.hydrogen || self.day == saved_day {
            self.finish(&tx, true)?;
        } else {
            // A saved answer belongs to its Moscow day. Move the charge only
            // if that day's shared bucket still has space.
            if !Database::reserve_ai_model_on(
                &tx,
                self.user,
                &saved_day,
                ai::HYDROGEN_MODEL,
                ai::HYDROGEN_DAILY_LIMIT,
            )? {
                return Err(Error::Quota);
            }
            self.finish(&tx, false)?;
            if !Database::finish_ai_model_on(&tx, self.user, &saved_day, ai::HYDROGEN_MODEL, true)?
            {
                return Err(rusqlite::Error::QueryReturnedNoRows.into());
            }
        }
        tx.commit()?;
        self.active = false;
        Ok(chat_id)
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if self.active {
            let release = || -> rusqlite::Result<()> {
                let mut connection = self.db.lock();
                let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                self.finish(&tx, false)?;
                tx.commit()
            };
            if let Err(error) = release() {
                eprintln!("Не удалось снять резерв запроса Activity: {error}");
            }
        }
    }
}

fn history(
    db: &Database,
    user: u64,
    chat_id: i64,
    image_slots: usize,
) -> rusqlite::Result<Vec<ai::Turn>> {
    let connection = db.lock();
    // At most three 4 MiB images including the current attachment keep the
    // encoded request under Gemini's 20 MB inline limit.
    let mut statement = connection.prepare(
        "SELECT prompt, answer, CASE WHEN id IN (
             SELECT id FROM ai_activity_turns WHERE chat_id = ?1 AND user_id = ?2
                 AND image_mime IS NOT NULL ORDER BY id DESC LIMIT ?4
         ) THEN image_data END, image_mime
         FROM (
             SELECT id, prompt, answer, image_data, image_mime FROM ai_activity_turns
             WHERE chat_id = ?1 AND user_id = ?2 ORDER BY id DESC LIMIT ?3
         ) ORDER BY id",
    )?;
    let rows = statement.query_map(
        params![chat_id, user as i64, CONTEXT_EXCHANGES, image_slots as i64],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<Vec<u8>>>(2)?
                    .zip(row.get::<_, Option<String>>(3)?),
            ))
        },
    )?;
    let mut turns = Vec::new();
    for row in rows {
        let (prompt, answer, image) = row?;
        turns.push(user_turn(prompt, image));
        turns.push(ai::Turn {
            role: "model".into(),
            text: answer,
            images: Vec::new(),
        });
    }
    Ok(turns)
}

fn user_turn(prompt: String, image: Option<(Vec<u8>, String)>) -> ai::Turn {
    let text = if prompt.is_empty() {
        "[Пользователь прислал изображение.]".into()
    } else {
        prompt
    };
    ai::Turn {
        role: "user".into(),
        text,
        images: image.into_iter().collect(),
    }
}

pub async fn send(data: &Data, user: u64, request: Request) -> Result<Value, Error> {
    let thinking = request.thinking;
    tokio::time::timeout(
        SEND_TIMEOUT,
        send_with(data, user, request, |model, turns| async move {
            ai::generate_activity(data, &turns, model, thinking).await
        }),
    )
    .await
    .map_err(|_| Error::Timeout)?
}

async fn send_with<F, Fut>(
    data: &Data,
    user: u64,
    request: Request,
    mut generate: F,
) -> Result<Value, Error>
where
    F: FnMut(ai::ModelChoice, Arc<Vec<ai::Turn>>) -> Fut,
    Fut: Future<Output = Result<ai::Answer, ai::AiError>>,
{
    let Request {
        request_id,
        chat_id,
        prompt,
        image,
        model,
        thinking: _,
    } = request;
    let mut pending = match admit(&data.db, user, &request_id, chat_id, ai::moscow_day(), model == ai::ModelChoice::Hydrogen)? {
        Admission::Replay(chat_id) => return read_chat(data, user, chat_id),
        Admission::Pending(pending) => pending,
    };
    let attachment = if let Some((mime, encoded)) = image {
        let slot = IMAGE_VALIDATION_SLOTS
            .try_acquire()
            .map_err(|_| Error::Provider(ai::AiError::Busy))?;
        Some(
            tokio::task::spawn_blocking(move || {
                let _slot = slot;
                decode_image(mime, &encoded)
            })
            .await
            .map_err(|error| Error::Provider(ai::AiError::Other(error.to_string())))??,
        )
    } else {
        None
    };
    let mut turns = match chat_id {
        Some(chat_id) => history(
            &data.db,
            user,
            chat_id,
            ai::MAX_IMAGES - usize::from(attachment.is_some()),
        )?,
        None => Vec::new(),
    };
    turns.push(user_turn(prompt.clone(), attachment.clone()));
    let turns = Arc::new(turns);
    let answer = if model == ai::ModelChoice::Auto {
        let primary = tokio::time::timeout(AUTO_PRIMARY_TIMEOUT, generate(ai::ModelChoice::FlashLite, turns.clone())).await;
        match primary {
            Err(_) | Ok(Err(ai::AiError::Network(_) | ai::AiError::Overloaded | ai::AiError::Quota)) => {
                pending.reserve_hydrogen(ai::moscow_day())?;
                generate(ai::ModelChoice::Hydrogen, turns).await
            }
            Ok(result) => result,
        }
    } else {
        generate(model, turns).await
    }.map_err(Error::Provider)?;
    if answer.text.trim().is_empty() {
        return Err(Error::Provider(ai::AiError::Other(
            "Gemini вернул пустой ответ".into(),
        )));
    }
    let chat_id = pending.save(
        chat_id,
        &prompt,
        &answer.text,
        attachment.as_ref(),
        chrono::Utc::now,
    )?;
    read_chat(data, user, chat_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{codecs::png::PngEncoder, ExtendedColorType, ImageEncoder};
    use std::{
        future,
        path::PathBuf,
        pin::Pin,
        task::{Context, Waker},
    };

    fn test_data() -> Data {
        Data::new(
            Database::in_memory().unwrap(),
            &crate::config::Settings::parse(""),
            PathBuf::new(),
            false,
        )
    }

    fn request(id: &str, chat_id: Option<&str>) -> Request {
        Request::from_json(
            &json!({ "request_id": id, "chat_id": chat_id, "prompt": "  A question  ", "model": "hydrogen" }),
        )
        .unwrap()
    }

    fn answer() -> Result<ai::Answer, ai::AiError> {
        Ok(ai::Answer {
            text: "**Answer**\n\n$x^2$".into(),
            cut: false,
        })
    }

    fn poll_pending(future: Pin<&mut impl Future>) {
        assert!(future
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
    }

    fn png(width: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(&vec![0; width as usize], width, 1, ExtendedColorType::L8)
            .unwrap();
        bytes
    }

    #[test]
    fn validates_model_and_thinking_pairs() {
        let defaults = Request::from_json(&json!({"request_id": "id", "prompt": "Question"})).unwrap();
        assert_eq!(defaults.model, ai::ModelChoice::FlashLite);
        assert_eq!(defaults.thinking, ai::ThinkingLevel::Medium);
        for model in ["auto", "flash_lite", "hydrogen"] {
            for thinking in ["minimal", "low", "medium", "high"] {
                let result = Request::from_json(&json!({
                    "request_id": "id", "prompt": "Question", "model": model, "thinking": thinking
                }));
                assert_eq!(result.is_ok(), thinking != "minimal" || model == "flash_lite");
            }
        }
        for extra in [json!({"model": "other"}), json!({"thinking": 1})] {
            let mut query = json!({"request_id": "id", "prompt": "Question"});
            query.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            assert!(matches!(Request::from_json(&query), Err(Error::Invalid(_))));
        }
    }

    #[tokio::test]
    async fn auto_charges_only_a_saved_hydrogen_fallback() {
        use ai::ModelChoice::{Auto, FlashLite, Hydrogen};

        let data = test_data();
        let mut models = Vec::new();
        for i in 0..3 {
            let mut req = request(&format!("fallback-{i}"), None);
            req.model = Auto;
            let saved = send_with(&data, 7, req, |model, _| {
                models.push(model);
                future::ready(if model == FlashLite { Err(ai::AiError::Overloaded) } else { answer() })
            })
            .await
            .unwrap();
            assert_eq!(saved["quota"]["remaining"], 2 - i);
        }
        assert_eq!(models, [FlashLite, Hydrogen].repeat(3));

        models.clear();
        let mut req = request("blocked", None);
        req.model = Auto;
        let blocked = send_with(&data, 7, req, |model, _| {
            models.push(model);
            future::ready(Err(ai::AiError::Overloaded))
        }).await;
        assert!(matches!(blocked, Err(Error::Quota)));
        assert_eq!(models, [FlashLite]);

        for (id, model) in [("explicit-lite", FlashLite), ("auto-lite", Auto)] {
            let mut req = request(id, None);
            req.model = model;
            let saved = send_with(&data, 7, req, |model, _| {
                assert_eq!(model, FlashLite);
                future::ready(answer())
            }).await.unwrap();
            assert_eq!(saved["quota"]["remaining"], 0);
        }

        let replay = send_with(&data, 7, request("fallback-0", None), |_, _| {
            future::ready(Err(ai::AiError::Other("provider should not run".into())))
        }).await.unwrap();
        assert_eq!(replay["quota"]["remaining"], 0);
        assert_eq!(replay["turns"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn late_fallback_reserves_the_current_moscow_day() {
        let data = test_data();
        let Admission::Pending(mut pending) =
            admit(&data.db, 7, "late", None, "2026-11-17".into(), false).unwrap()
        else {
            panic!("expected pending request");
        };
        pending.reserve_hydrogen("2026-11-18".into()).unwrap();
        assert_eq!(ai::hydrogen_usage(&data, 7, "2026-11-17").unwrap(), (0, 0));
        assert_eq!(ai::hydrogen_usage(&data, 7, "2026-11-18").unwrap(), (0, 1));
        drop(pending);
        assert_eq!(ai::hydrogen_usage(&data, 7, "2026-11-18").unwrap(), (0, 0));
    }

    #[tokio::test]
    async fn concurrent_activity_sends_share_three_slots_with_hydrogen() {
        let data = test_data();
        let day = ai::moscow_day();
        let mut first = Box::pin(send_with(&data, 7, request("first", None), |_, _| {
            future::pending()
        }));
        let mut second = Box::pin(send_with(&data, 7, request("second", None), |_, _| {
            future::pending()
        }));
        poll_pending(first.as_mut());
        assert!(data
            .db
            .reserve_ai_model(7, &day, ai::HYDROGEN_MODEL, 3)
            .unwrap());
        data.db
            .finish_ai_model(7, &day, ai::HYDROGEN_MODEL, true)
            .unwrap();
        poll_pending(second.as_mut());
        assert_eq!(overview(&data, 7).unwrap()["quota"]["remaining"], 0);
        assert!(!data
            .db
            .reserve_ai_model(7, &day, ai::HYDROGEN_MODEL, 3)
            .unwrap());
        let fourth = send_with(&data, 7, request("fourth", None), |_, _| {
            future::ready(Err(ai::AiError::Other("provider should not run".into())))
        })
        .await
        .unwrap_err();
        assert_eq!(fourth.status(), StatusCode::TOO_MANY_REQUESTS);
        drop(first);
        let saved = send_with(&data, 7, request("fourth", None), |_, _| {
            future::ready(answer())
        })
        .await
        .unwrap();
        assert_eq!(saved["quota"]["remaining"], 0);
        drop(second);
        assert_eq!(overview(&data, 7).unwrap()["quota"]["remaining"], 1);
        assert_eq!(
            data.db.ai_model_usage(7, &day, ai::HYDROGEN_MODEL).unwrap(),
            2
        );
    }

    #[test]
    fn a_send_crossing_moscow_midnight_charges_the_saved_day() {
        let data = test_data();
        let started = chrono::DateTime::parse_from_rfc3339("2026-11-17T20:59:59Z")
            .unwrap()
            .to_utc();
        let saved = chrono::DateTime::parse_from_rfc3339("2026-11-17T21:00:00Z")
            .unwrap()
            .to_utc();
        assert_eq!(ai::moscow_reset_time_at(started), saved.timestamp());
        let Admission::Pending(pending) =
            admit(&data.db, 7, "midnight", None, ai::moscow_day_at(started), true).unwrap()
        else {
            panic!("expected a new reservation");
        };
        let chat_id = pending
            .save(None, "Question", "Answer", None, || saved)
            .unwrap();
        assert_eq!(
            read_chat(&data, 7, chat_id).unwrap()["turns"][0]["answer"],
            "Answer"
        );
        assert_eq!(ai::hydrogen_usage(&data, 7, "2026-11-17").unwrap(), (0, 0));
        assert_eq!(ai::hydrogen_usage(&data, 7, "2026-11-18").unwrap(), (1, 0));
    }

    #[test]
    fn a_send_crossing_midnight_into_a_full_bucket_leaves_no_turn() {
        let data = test_data();
        let saved = chrono::DateTime::parse_from_rfc3339("2026-11-17T21:00:00Z")
            .unwrap()
            .to_utc();
        let Admission::Pending(pending) =
            admit(&data.db, 7, "midnight", None, "2026-11-17".into(), true).unwrap()
        else {
            panic!("expected a new reservation");
        };
        for _ in 0..3 {
            assert!(data
                .db
                .reserve_ai_model(7, "2026-11-18", ai::HYDROGEN_MODEL, 3)
                .unwrap());
            data.db
                .finish_ai_model(7, "2026-11-18", ai::HYDROGEN_MODEL, true)
                .unwrap();
        }
        assert!(matches!(
            pending.save(None, "Question", "Answer", None, || saved),
            Err(Error::Quota)
        ));
        assert_eq!(overview(&data, 7).unwrap()["chats"], json!([]));
        assert_eq!(ai::hydrogen_usage(&data, 7, "2026-11-17").unwrap(), (0, 0));
        assert_eq!(ai::hydrogen_usage(&data, 7, "2026-11-18").unwrap(), (3, 0));
    }

    #[tokio::test]
    async fn a_chat_serializes_pending_sends_and_replays_completed_ids() {
        let data = test_data();
        let first = send_with(&data, 7, request("new", None), |_, _| future::ready(answer()))
            .await
            .unwrap();
        let chat_id = first["chat"]["id"].as_str().unwrap();
        let mut pending = Box::pin(send_with(
            &data,
            7,
            request("second", Some(chat_id)),
            |_, turns| {
                assert_eq!(turns.len(), 3);
                assert_eq!(turns[0].text, "A question");
                assert_eq!(turns[1].role, "model");
                assert_eq!(turns[1].text, "**Answer**\n\n$x^2$");
                future::pending()
            },
        ));
        poll_pending(pending.as_mut());
        for id in ["second", "third"] {
            let blocked = send_with(&data, 7, request(id, Some(chat_id)), |_, _| {
                future::ready(answer())
            })
            .await;
            assert!(matches!(blocked, Err(Error::InProgress)));
        }
        let same_request_without_chat = send_with(&data, 7, request("second", None), |_, _| {
            future::ready(answer())
        })
        .await;
        assert!(matches!(same_request_without_chat, Err(Error::InProgress)));
        drop(pending);
        let completed = send_with(&data, 7, request("second", Some(chat_id)), |_, _| {
            future::ready(answer())
        })
        .await
        .unwrap();
        let replay = send_with(&data, 7, request("second", None), |_, _| {
            future::ready(Err(ai::AiError::Other("provider should not run".into())))
        })
        .await
        .unwrap();
        assert_eq!(completed, replay);
        assert_eq!(replay["turns"].as_array().unwrap().len(), 2);
        assert_eq!(replay["quota"]["remaining"], 1);
    }

    #[tokio::test]
    async fn provider_and_storage_failures_leave_the_request_available_to_retry() {
        let data = test_data();
        let failed = send_with(&data, 7, request("retry", None), |_, _| {
            future::ready(Err(ai::AiError::Overloaded))
        })
        .await;
        assert!(matches!(
            failed,
            Err(Error::Provider(ai::AiError::Overloaded))
        ));
        let empty = send_with(&data, 7, request("retry", None), |_, _| {
            future::ready(Ok(ai::Answer {
                text: " ".into(),
                cut: false,
            }))
        })
        .await;
        assert!(matches!(empty, Err(Error::Provider(_))));
        data.db
            .lock()
            .execute_batch(
                "CREATE TRIGGER reject_charge BEFORE UPDATE OF used ON ai_model_usage
             WHEN NEW.used > OLD.used BEGIN SELECT RAISE(ABORT, 'charge rejected'); END;",
            )
            .unwrap();
        let failed = send_with(
            &data,
            7,
            request("retry", None),
            |_, _| future::ready(answer()),
        )
        .await;
        assert!(matches!(failed, Err(Error::Storage(_))));
        let snapshot = overview(&data, 7).unwrap();
        assert_eq!(snapshot["chats"], json!([]));
        assert_eq!(snapshot["quota"]["remaining"], 3);
        assert_eq!(
            data.db
                .lock()
                .query_row("SELECT COUNT(*) FROM ai_activity_turns", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        data.db
            .lock()
            .execute_batch("DROP TRIGGER reject_charge")
            .unwrap();
        let saved = send_with(
            &data,
            7,
            request("retry", None),
            |_, _| future::ready(answer()),
        )
        .await
        .unwrap();
        assert_eq!(saved["turns"].as_array().unwrap().len(), 1);
        assert_eq!(saved["quota"]["remaining"], 2);
    }

    #[tokio::test]
    async fn chats_images_and_idempotency_keys_are_scoped_to_the_verified_user() {
        let data = test_data();
        let bytes = png(1);
        let original = Request::from_json(&json!({
            "request_id": "shared-key", "prompt": "",
            "image": { "mime": "image/png", "data": STANDARD.encode(&bytes) },
        }))
        .unwrap();
        let result = send_with(&data, 7, original, |_, turns| {
            assert_eq!(turns[0].images, [(bytes.clone(), "image/png".into())]);
            future::ready(answer())
        })
        .await
        .unwrap();
        let chat_id = result["chat"]["id"].as_str().unwrap();
        let turn_id = result["turns"][0]["id"].as_str().unwrap();
        assert_eq!(result["turns"][0]["image"], true);
        assert_eq!(chat(&data, 7, chat_id).unwrap(), result);
        assert_eq!(
            image(&data, 7, turn_id).unwrap(),
            ("image/png".into(), bytes)
        );
        assert_eq!(overview(&data, 8).unwrap()["chats"], json!([]));
        assert!(matches!(chat(&data, 8, chat_id), Err(Error::NotFound)));
        assert!(matches!(image(&data, 8, turn_id), Err(Error::NotFound)));
        let denied = send_with(&data, 8, request("shared-key", Some(chat_id)), |_, _| {
            future::ready(answer())
        })
        .await;
        assert!(matches!(denied, Err(Error::NotFound)));
        let other = send_with(&data, 8, request("shared-key", None), |_, _| {
            future::ready(answer())
        })
        .await
        .unwrap();
        assert_ne!(other["chat"]["id"], result["chat"]["id"]);
    }

    #[tokio::test]
    async fn invalid_attachments_never_generate_or_cost_quota() {
        let data = test_data();
        let valid = STANDARD.encode(png(1));
        let oversized = STANDARD.encode(png(4097));
        for (mime, encoded) in [
            ("image/png", "invalid base64".into()),
            ("image/jpeg", valid),
            ("image/png", oversized),
            ("image/png", STANDARD.encode(vec![0; MAX_IMAGE_BYTES + 1])),
        ] {
            let request = Request::from_json(&json!({
                "request_id": "invalid", "prompt": "Question",
                "image": { "mime": mime, "data": encoded },
            }))
            .unwrap();
            let result = send_with(&data, 7, request, |_, _| future::ready(answer())).await;
            assert!(matches!(result, Err(Error::Invalid(_))));
        }
        assert_eq!(overview(&data, 7).unwrap()["quota"]["remaining"], 3);
    }
}
