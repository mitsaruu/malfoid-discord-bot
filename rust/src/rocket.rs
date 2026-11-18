//! Ракетка в Discord Activity. Часы, взрыв и выплата принадлежат серверу.

use crate::economy::MAX_BET;
use crate::reply::whisper;
use crate::{wagers, Context, Data, Error, Shared};
use poise::serenity_prelude as s;
use rand::Rng;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::Instant;

const COUNTDOWN: Duration = Duration::from_secs(3);
const MAX: u32 = 1000;
const DOUBLE_SECONDS: f64 = 6.0;
const OPEN_WINDOW: Duration = Duration::from_secs(120);
const RESULT_WINDOW: Duration = Duration::from_secs(12 * 3600);

type Player = (u64, u64); // сервер, игрок

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Crashed,
    Paid {
        multiplier: u32,
        payout: i64,
        automatic: bool,
    },
    Refunded,
}

struct Flight {
    id: u64,
    stake: i64,
    crash_at: u32,
    auto: Option<u32>,
    launch: Instant,
    finished: Option<(Instant, Outcome)>,
}

#[derive(Default)]
pub struct State {
    flights: Mutex<HashMap<Player, Flight>>,
    opening: Mutex<HashMap<Player, Instant>>,
}

impl State {
    fn flights(&self) -> std::sync::MutexGuard<'_, HashMap<Player, Flight>> {
        self.flights
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn mark_open(&self, player: Player) {
        let now = Instant::now();
        let mut opening = self
            .opening
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        opening.retain(|_, expires| *expires > now);
        opening.insert(player, now + OPEN_WINDOW);
    }

    pub(crate) fn take_open(&self, guild: u64, user: u64) -> bool {
        let mut opening = self
            .opening
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        opening
            .remove(&(guild, user))
            .is_some_and(|expires| expires > Instant::now())
    }
}

pub fn start_cleanup(data: Shared) {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(30 * 60));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            let now = Instant::now();
            data.rocket.flights().retain(|_, flight| {
                flight.finished.is_none_or(|(finished, _)| now - finished <= RESULT_WINDOW)
            });
            data.rocket.opening.lock().unwrap_or_else(|error| error.into_inner())
                .retain(|_, expires| *expires > now);
        }
    });
}

fn crash_point(random: u32) -> u32 {
    (97_000_000 / random).clamp(100, MAX)
}

fn flight_time(multiplier: u32) -> Duration {
    Duration::from_secs_f64((f64::from(multiplier) / 100.0).log2() * DOUBLE_SECONDS)
}

fn parse_auto(raw: &str) -> Option<u32> {
    let raw = raw.trim();
    let (whole, fraction) = raw.split_once(['.', ',']).unwrap_or((raw, ""));
    if whole.len() != 1
        || fraction.len() > 2
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let decimals = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<u32>().ok()? * 10,
        _ => fraction.parse::<u32>().ok()?,
    };
    let value = whole.parse::<u32>().ok()? * 100 + decimals;
    (101..MAX).contains(&value).then_some(value)
}

impl Flight {
    fn deadline(&self) -> (Instant, Outcome) {
        if let Some(target) = self.auto.filter(|target| *target < self.crash_at) {
            // Сравниваем события по моменту, когда они должны были произойти, даже если воркер проснулся поздно.
            return (self.launch + flight_time(target), self.payout(target, true));
        }
        (self.launch + flight_time(self.crash_at), Outcome::Crashed)
    }

    fn payout(&self, multiplier: u32, automatic: bool) -> Outcome {
        Outcome::Paid {
            multiplier,
            payout: self.stake * i64::from(multiplier) / 100,
            automatic,
        }
    }

    fn live_multiplier(&self, now: Instant) -> u32 {
        let elapsed = now.saturating_duration_since(self.launch).as_secs_f64();
        ((100.0 * (elapsed / DOUBLE_SECONDS).exp2()) as u32)
            .min(self.crash_at.saturating_sub(1))
            .max(100)
    }

    fn settle(&mut self, data: &Data, at: Instant, outcome: Outcome) -> rusqlite::Result<()> {
        let payout = match outcome {
            Outcome::Paid { payout, .. } => payout,
            _ => 0,
        };
        wagers::settle(&data.db, self.id, &[(self.id, payout)])?;
        self.finished = Some((at, outcome));
        Ok(())
    }

    fn advance(&mut self, data: &Data, now: Instant) -> rusqlite::Result<()> {
        if self.finished.is_none() {
            let (at, outcome) = self.deadline();
            if at <= now {
                self.settle(data, at, outcome)?;
            }
        }
        Ok(())
    }

    fn snapshot(&self, now: Instant) -> Value {
        let (at, outcome) = self
            .finished
            .map_or((now, None), |(at, outcome)| (at, Some(outcome)));
        let elapsed_ms = if at < self.launch {
            -((self.launch - at).as_millis() as i64)
        } else {
            (at - self.launch).as_millis() as i64
        };
        let (status, multiplier, payout, automatic) = match outcome {
            Some(Outcome::Paid {
                multiplier,
                payout,
                automatic,
            }) => ("paid", multiplier, Some(payout), automatic),
            Some(Outcome::Crashed) => ("crashed", self.crash_at, None, false),
            Some(Outcome::Refunded) => ("refunded", 100, None, false),
            None if at < self.launch => ("countdown", 100, None, false),
            None => ("flying", self.live_multiplier(at), None, false),
        };
        json!({ "id": self.id.to_string(), "status": status, "stake": self.stake, "elapsed_ms": elapsed_ms,
                "multiplier": multiplier, "auto": self.auto, "payout": payout, "automatic": automatic })
    }
}

fn result(
    data: &Data,
    player: Player,
    flight: Option<&Flight>,
    now: Instant,
) -> rusqlite::Result<Value> {
    let balance = data
        .db
        .lock()
        .query_row(
            "SELECT balance FROM economy WHERE guild_id = ?1 AND user_id = ?2",
            params![player.0 as i64, player.1 as i64],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(data.economy.start_balance);
    Ok(
        json!({ "balance": balance, "min_bet": data.economy.min_bet, "max_bet": MAX_BET,
               "flight": flight.map(|flight| flight.snapshot(now)) }),
    )
}

pub fn read(data: &Data, player: Player) -> rusqlite::Result<Value> {
    let mut flights = data.rocket.flights();
    let flight = flights.get_mut(&player);
    let now = Instant::now();
    if let Some(flight) = flight {
        flight.advance(data, now)?;
        result(data, player, Some(flight), now)
    } else {
        result(data, player, None, now)
    }
}

pub fn start(
    data: &Shared,
    player: Player,
    name: &str,
    bet: &str,
    auto: Option<&str>,
    expected: Option<u64>,
) -> rusqlite::Result<Result<Value, String>> {
    let auto = match auto {
        Some(raw) => match parse_auto(raw) {
            Some(target) => Some(target),
            None => {
                return Ok(Err(
                    "Коэффициент автовывода — от 1,01 до 9,99, до двух знаков после запятой."
                        .into(),
                ))
            }
        },
        None => None,
    };
    let mut flights = data.rocket.flights();
    let now = Instant::now();
    flights.retain(|key, flight| {
        *key == player
            || flight
                .finished
                .is_none_or(|(at, _)| now - at <= RESULT_WINDOW)
    });
    if let Some(previous) = flights.get_mut(&player) {
        previous.advance(data, now)?;
        if previous.finished.is_none() {
            return Ok(Err("У тебя уже идёт полёт.".into()));
        }
    }
    if expected != flights.get(&player).map(|flight| flight.id) {
        return Ok(Err(
            "Раунд уже изменился. Проверь состояние игры и поставь снова.".into(),
        ));
    }
    data.db
        .wallet(player.0, player.1, name, data.economy.start_balance)?;
    // Старший бит отделяет билеты Activity от положительных идентификаторов Discord.
    let id = rand::random::<u64>() | (1 << 63);
    let stake = match wagers::place(
        &data.db,
        id,
        id,
        player.0,
        player.1,
        bet,
        data.economy.min_bet,
    )? {
        Ok(stake) => stake,
        Err(error) => return Ok(Err(error)),
    };
    let now = Instant::now();
    let flight = Flight {
        id,
        stake,
        crash_at: crash_point(rand::rng().random_range(1..=1_000_000)),
        auto,
        launch: now + COUNTDOWN,
        finished: None,
    };
    let (deadline, _) = flight.deadline();
    flights.insert(player, flight);
    tokio::spawn(settle_at_deadline(data.clone(), player, id, deadline));
    Ok(Ok(result(data, player, flights.get(&player), now)?))
}

async fn settle_at_deadline(data: Shared, player: Player, id: u64, deadline: Instant) {
    tokio::time::sleep_until(deadline).await;
    loop {
        {
            let mut flights = data.rocket.flights();
            let Some(flight) = flights.get_mut(&player).filter(|flight| flight.id == id) else {
                return;
            };
            let now = Instant::now();
            match flight.advance(&data, now) {
                Ok(()) => return,
                Err(error) => {
                    eprintln!("Ракетка {id}: расчёт не удался: {error}");
                    if wagers::refund(&data.db, Some(id)).is_ok() {
                        flight.finished = Some((now, Outcome::Refunded));
                        return;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

pub fn cash_out(data: &Data, player: Player, id: u64) -> rusqlite::Result<Result<Value, String>> {
    let mut flights = data.rocket.flights();
    let Some(flight) = flights.get_mut(&player).filter(|flight| flight.id == id) else {
        return Ok(Err("Этот раунд уже закрыт.".into()));
    };
    let now = Instant::now();
    flight.advance(data, now)?;
    if flight.finished.is_none() {
        if now < flight.launch {
            return Ok(Err("Ракета ещё не стартовала.".into()));
        }
        let outcome = flight.payout(flight.live_multiplier(now), false);
        flight.settle(data, now, outcome)?;
    }
    Ok(Ok(result(data, player, Some(flight), now)?))
}

/// Открыть ракетку в мини-приложении Discord
#[poise::command(slash_command, guild_only)]
pub async fn rocket(ctx: Context<'_>) -> Result<(), Error> {
    if !ctx.data().miniapp_ready() {
        return whisper(ctx, "Мини-приложение ещё не настроено.").await;
    }
    let poise::Context::Application(app) = ctx else {
        return Ok(());
    };
    let player = (ctx.guild_id().unwrap().get(), ctx.author().id.get());
    ctx.data().rocket.mark_open(player);
    if !crate::wordboard::launch_activity(|response| {
        app.interaction.create_response(ctx.http(), response)
    })
    .await
    {
        ctx.data().rocket.take_open(player.0, player.1);
    }
    Ok(())
}

pub async fn pressed(ctx: &s::Context, press: &s::ComponentInteraction) -> bool {
    if !press.data.custom_id.starts_with("rocket:cash:") {
        return false;
    }
    let _ = press
        .create_response(
            ctx,
            s::CreateInteractionResponse::Message(
                s::CreateInteractionResponseMessage::new()
                    .content("Открой ракетку через /rocket.")
                    .ephemeral(true),
            ),
        )
        .await;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auto_pays_once_offline_only_when_target_precedes_crash() {
        let settings = crate::config::Settings::default();
        let data = std::sync::Arc::new(Data::new(
            crate::db::Database::in_memory().unwrap(),
            &settings,
            Default::default(),
            false,
        ));
        let player = (1, 2);
        data.db.wallet(1, 2, "pilot", 500).unwrap();
        let launch = Instant::now() - Duration::from_secs(30);
        for (id, target, expected) in [(5, 150, 150), (6, 200, 0), (7, 220, 0)] {
            assert_eq!(
                wagers::place(&data.db, id, id, 1, 2, "100", 10).unwrap(),
                Ok(100)
            );
            let flight = Flight {
                id,
                stake: 100,
                crash_at: 200,
                auto: Some(target),
                launch,
                finished: None,
            };
            let (deadline, _) = flight.deadline();
            data.rocket.flights().insert(player, flight);
            settle_at_deadline(data.clone(), player, id, deadline).await;
            let snapshot = cash_out(&data, player, id).unwrap().unwrap();
            assert_eq!(snapshot["flight"]["payout"].as_i64().unwrap_or(0), expected);
            assert_eq!(snapshot["flight"]["automatic"], expected > 0);
        }
        assert_eq!(data.db.wallet(1, 2, "pilot", 500).unwrap().0, 350);
    }

    #[test]
    fn cashout_target_is_exact_and_bounded() {
        assert_eq!(parse_auto("1,01"), Some(101));
        assert_eq!(parse_auto("2.5"), Some(250));
        assert_eq!(parse_auto("9.99"), Some(999));
        for invalid in ["1", "10", "1.001", "+2", "2e0", "NaN", ""] {
            assert_eq!(parse_auto(invalid), None);
        }
    }

    #[tokio::test]
    async fn round_guard_prevents_duplicate_debit_and_other_players_cashout() {
        let settings = crate::config::Settings::default();
        let data = std::sync::Arc::new(Data::new(
            crate::db::Database::in_memory().unwrap(),
            &settings,
            Default::default(),
            false,
        ));
        data.db.wallet(1, 2, "pilot", 500).unwrap();
        let opened = start(&data, (1, 2), "pilot", "100", Some("2.00"), None)
            .unwrap()
            .unwrap();
        let round = opened["flight"]["id"].as_str().unwrap().parse().unwrap();
        assert_eq!(opened["balance"], 400);
        assert_eq!(opened["flight"]["status"], "countdown");
        assert!(opened["flight"].get("crash_at").is_none());
        assert!(start(&data, (1, 2), "pilot", "100", None, None)
            .unwrap()
            .is_err());
        assert!(cash_out(&data, (1, 3), round).unwrap().is_err());
        assert!(cash_out(&data, (1, 2), round).unwrap().is_err());
        assert_eq!(read(&data, (1, 2)).unwrap()["balance"], 400);
    }
}
