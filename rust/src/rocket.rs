//! Ракетка в Discord Activity. Часы, взрыв и выплата принадлежат серверу.

use crate::economy::MAX_BET;
use crate::{wagers, Data, Shared};
use poise::serenity_prelude as s;
use rand::Rng;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::Instant;

const COUNTDOWN: Duration = Duration::from_secs(3);
const MAX_WAGER_CRASH: u32 = 1000;
const DOUBLE_SECONDS: f64 = 6.0;
const CINEMATIC_DOUBLE_SECONDS: f64 = 0.6;
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
    cinematic_peak: Option<u32>,
    auto: Option<u32>,
    launch: Instant,
    outcome: Option<Outcome>,
    finished_at: Option<Instant>,
}

#[derive(Default)]
pub struct State {
    flights: Mutex<HashMap<Player, Flight>>,
}

impl State {
    fn flights(&self) -> std::sync::MutexGuard<'_, HashMap<Player, Flight>> {
        self.flights
            .lock()
            .unwrap_or_else(|error| error.into_inner())
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
                flight
                    .finished_at
                    .is_none_or(|finished| now - finished <= RESULT_WINDOW)
            });
        }
    });
}

fn crash_point(random: u32) -> u32 {
    (97_000_000 / random).clamp(100, MAX_WAGER_CRASH)
}

fn cinematic_peak(roll: u32) -> Option<u32> {
    // Independent of the wager crash. Of paid flights, 0.9% reach 100× and 0.1% reach
    // 1,000×. At 600 ms per doubling, even 1× to 1,000× adds less than six seconds.
    match roll {
        1 => Some(100_000),
        2..=10 => Some(10_000),
        _ => None,
    }
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
    (101..MAX_WAGER_CRASH).contains(&value).then_some(value)
}

impl Flight {
    fn end_multiplier(&self) -> u32 {
        if matches!(self.outcome, Some(Outcome::Paid { .. })) {
            self.cinematic_peak.unwrap_or(self.crash_at)
        } else {
            self.crash_at
        }
    }

    fn crash_time(&self) -> Instant {
        let ratio = f64::from(self.end_multiplier()) / f64::from(self.crash_at);
        self.launch
            + flight_time(self.crash_at)
            + Duration::from_secs_f64(ratio.log2() * CINEMATIC_DOUBLE_SECONDS)
    }

    fn automatic_cashout(&self) -> Option<(Instant, u32)> {
        self.auto
            .filter(|target| self.outcome.is_none() && *target < self.crash_at)
            .map(|target| (self.launch + flight_time(target), target))
    }

    fn next_deadline(&self) -> Option<Instant> {
        if self.finished_at.is_some() {
            return None;
        }
        Some(
            self.automatic_cashout()
                .map_or_else(|| self.crash_time(), |(at, _)| at),
        )
    }

    fn payout(&self, multiplier: u32, automatic: bool) -> Outcome {
        Outcome::Paid {
            multiplier,
            payout: self.stake * i64::from(multiplier) / 100,
            automatic,
        }
    }

    fn payout_multiplier(&self, now: Instant) -> u32 {
        let elapsed = now.saturating_duration_since(self.launch).as_secs_f64();
        ((100.0 * (elapsed / DOUBLE_SECONDS).exp2()) as u32)
            .min(self.crash_at.saturating_sub(1))
            .max(100)
    }

    fn settle(&mut self, data: &Data, outcome: Outcome) -> rusqlite::Result<()> {
        let payout = match outcome {
            Outcome::Paid { payout, .. } => payout,
            _ => 0,
        };
        wagers::settle(&data.db, self.id, &[(self.id, payout)])?;
        self.outcome = Some(outcome);
        Ok(())
    }

    fn advance(&mut self, data: &Data, now: Instant) -> rusqlite::Result<()> {
        if self.finished_at.is_some() {
            return Ok(());
        }
        // Даже при позднем пробуждении автовывод предшествует аварии.
        if let Some((_, target)) = self.automatic_cashout().filter(|(at, _)| *at <= now) {
            self.settle(data, self.payout(target, true))?;
        }
        let crash_time = self.crash_time();
        if crash_time <= now {
            if self.outcome.is_none() {
                self.settle(data, Outcome::Crashed)?;
            }
            self.finished_at = Some(crash_time);
        }
        Ok(())
    }

    fn snapshot(&self, now: Instant) -> Value {
        let at = self.finished_at.unwrap_or(now);
        let elapsed_ms = if at < self.launch {
            -((self.launch - at).as_millis() as i64)
        } else {
            (at - self.launch).as_millis() as i64
        };
        let original_duration = flight_time(self.crash_at);
        let cinematic_elapsed = (self.end_multiplier() > self.crash_at)
            .then(|| at.checked_duration_since(self.launch + original_duration))
            .flatten();
        let (status, multiplier) = match self.finished_at {
            Some(_) if self.outcome == Some(Outcome::Refunded) => ("refunded", 100),
            Some(_) => ("crashed", self.end_multiplier()),
            None if at < self.launch => ("countdown", 100),
            None => (
                "flying",
                if let Some(elapsed) = cinematic_elapsed {
                    let factor = f64::from(self.crash_at)
                        * (elapsed.as_secs_f64() / CINEMATIC_DOUBLE_SECONDS).exp2();
                    (factor as u32).min(self.end_multiplier() - 1)
                } else {
                    self.payout_multiplier(at)
                },
            ),
        };
        let (cashout_multiplier, payout, automatic) = match self.outcome {
            Some(Outcome::Paid {
                multiplier,
                payout,
                automatic,
            }) => (Some(multiplier), Some(payout), automatic),
            _ => (None, None, false),
        };
        json!({ "id": self.id.to_string(), "status": status, "stake": self.stake, "elapsed_ms": elapsed_ms,
                "multiplier": multiplier, "auto": self.auto, "cashout_multiplier": cashout_multiplier,
                "payout": payout, "automatic": automatic,
                "cinematic_from_ms": cinematic_elapsed.map(|_| original_duration.as_millis() as u64) })
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
                .finished_at
                .is_none_or(|at| now - at <= RESULT_WINDOW)
    });
    if let Some(previous) = flights.get_mut(&player) {
        previous.advance(data, now)?;
        if previous.finished_at.is_none() {
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
        cinematic_peak: cinematic_peak(rand::rng().random_range(1..=1000)),
        auto,
        launch: now + COUNTDOWN,
        outcome: None,
        finished_at: None,
    };
    flights.insert(player, flight);
    tokio::spawn(run_flight(data.clone(), player, id));
    Ok(Ok(result(data, player, flights.get(&player), now)?))
}

async fn run_flight(data: Shared, player: Player, id: u64) {
    loop {
        let deadline = {
            let mut flights = data.rocket.flights();
            let Some(flight) = flights.get_mut(&player).filter(|flight| flight.id == id) else {
                return;
            };
            let now = Instant::now();
            match flight.advance(&data, now) {
                Ok(()) => match flight.next_deadline() {
                    Some(deadline) => deadline,
                    None => return,
                },
                Err(error) => {
                    eprintln!("Ракетка {id}: расчёт не удался: {error}");
                    if wagers::refund(&data.db, Some(id)).is_ok() {
                        flight.outcome = Some(Outcome::Refunded);
                        flight.finished_at = Some(now);
                        return;
                    }
                    now + Duration::from_secs(2)
                }
            }
        };
        tokio::time::sleep_until(deadline).await;
    }
}

pub fn cash_out(data: &Data, player: Player, id: u64) -> rusqlite::Result<Result<Value, String>> {
    let mut flights = data.rocket.flights();
    let Some(flight) = flights.get_mut(&player).filter(|flight| flight.id == id) else {
        return Ok(Err("Этот раунд уже закрыт.".into()));
    };
    let now = Instant::now();
    flight.advance(data, now)?;
    if flight.outcome.is_none() {
        if now < flight.launch {
            return Ok(Err("Ракета ещё не стартовала.".into()));
        }
        let outcome = flight.payout(flight.payout_multiplier(now), false);
        flight.settle(data, outcome)?;
    }
    Ok(Ok(result(data, player, Some(flight), now)?))
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
                    .content("Открой ракетку через /games.")
                    .ephemeral(true),
            ),
        )
        .await;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cinematic_roll_has_two_rare_tiers() {
        assert_eq!(cinematic_peak(1), Some(100_000));
        assert_eq!(cinematic_peak(2), Some(10_000));
        assert_eq!(cinematic_peak(10), Some(10_000));
        assert_eq!(cinematic_peak(11), None);
        assert_eq!(cinematic_peak(1000), None);
    }

    #[test]
    fn cinematic_peak_is_revealed_only_after_a_paid_flight_finishes() {
        let data = Data::new(
            crate::db::Database::in_memory().unwrap(),
            &crate::config::Settings::default(),
            Default::default(),
            false,
        );
        let player = (1, 2);
        data.db.wallet(1, 2, "pilot", 500).unwrap();
        wagers::place(&data.db, 5, 5, 1, 2, "100", 10)
            .unwrap()
            .unwrap();
        let launch = Instant::now() - Duration::from_secs(30);
        let mut flight = Flight {
            id: 5,
            stake: 100,
            crash_at: 400,
            cinematic_peak: Some(100_000),
            auto: Some(200),
            launch,
            outcome: None,
            finished_at: None,
        };
        let before = flight.snapshot(launch + Duration::from_secs(5));
        assert_eq!(before["multiplier"], 178);
        assert_eq!(before["payout"], Value::Null);
        assert_eq!(before["cinematic_from_ms"], Value::Null);

        // A late auto deadline still settles at 2× before evaluating the crash.
        let during = launch + Duration::from_millis(12_600);
        flight.advance(&data, during).unwrap();
        let resumed = result(&data, player, Some(&flight), during).unwrap();
        assert_eq!(resumed["flight"]["status"], "flying");
        assert_eq!(resumed["flight"]["multiplier"], 800);
        assert_eq!(resumed["flight"]["cinematic_from_ms"], 12_000);
        assert_eq!(resumed["flight"]["payout"], 200);
        assert_eq!(resumed["flight"]["cashout_multiplier"], 200);
        assert_eq!(resumed["balance"], 600);
        let end = flight.next_deadline().unwrap();
        assert!(end < launch + Duration::from_secs(18));
        let almost = end - Duration::from_nanos(1);
        flight.advance(&data, almost).unwrap();
        assert_eq!(flight.snapshot(almost)["status"], "flying");
        assert_eq!(flight.snapshot(almost)["multiplier"], 99_999);

        flight.advance(&data, end).unwrap();
        let crashed = result(&data, player, Some(&flight), end).unwrap();
        assert_eq!(crashed["flight"]["status"], "crashed");
        assert_eq!(crashed["flight"]["multiplier"], 100_000);
        assert_eq!(crashed["flight"]["cashout_multiplier"], 200);
        assert_eq!(crashed["flight"]["payout"], 200);
        assert_eq!(crashed["balance"], 600);
        assert!(flight.next_deadline().is_none());
    }

    #[tokio::test]
    async fn manual_payout_keeps_cinematic_flight_active_and_fixed_through_crash() {
        let settings = crate::config::Settings::default();
        let data = std::sync::Arc::new(Data::new(
            crate::db::Database::in_memory().unwrap(),
            &settings,
            Default::default(),
            false,
        ));
        let player = (1, 2);
        data.db.wallet(1, 2, "pilot", 500).unwrap();
        wagers::place(&data.db, 5, 5, 1, 2, "100", 10)
            .unwrap()
            .unwrap();
        data.rocket.flights().insert(
            player,
            Flight {
                id: 5,
                stake: 100,
                crash_at: 300,
                cinematic_peak: Some(100_000),
                auto: Some(200),
                launch: Instant::now(),
                outcome: None,
                finished_at: None,
            },
        );

        let paid = cash_out(&data, player, 5).unwrap().unwrap();
        assert_eq!(paid["flight"]["status"], "flying");
        let cashout_multiplier = paid["flight"]["cashout_multiplier"].as_i64().unwrap();
        assert!((100..200).contains(&cashout_multiplier));
        assert_eq!(paid["flight"]["payout"], cashout_multiplier);
        let paid_balance = 400 + cashout_multiplier;
        assert_eq!(paid["balance"], paid_balance);
        assert!(paid["flight"].get("crash_at").is_none());
        assert!(start(&data, player, "pilot", "100", None, Some(5))
            .unwrap()
            .is_err());

        // The wager would have crashed at 3×, but the paid flight is still active.
        data.rocket.flights().get_mut(&player).unwrap().launch =
            Instant::now() - Duration::from_secs(12);
        let resumed = read(&data, player).unwrap();
        assert_eq!(resumed["flight"]["status"], "flying");
        assert!(resumed["flight"]["multiplier"].as_u64().unwrap() > 1000);
        assert!(resumed["flight"]["multiplier"].as_u64().unwrap() < 100_000);
        assert_eq!(resumed["flight"]["cashout_multiplier"], cashout_multiplier);
        assert!(start(&data, player, "pilot", "100", None, Some(5))
            .unwrap()
            .is_err());
        let repeated = cash_out(&data, player, 5).unwrap().unwrap();
        assert_eq!(repeated["flight"]["payout"], cashout_multiplier);
        assert_eq!(repeated["flight"]["automatic"], false);
        assert_eq!(repeated["balance"], paid_balance);

        data.rocket.flights().get_mut(&player).unwrap().launch =
            Instant::now() - Duration::from_secs(16);
        let crashed = cash_out(&data, player, 5).unwrap().unwrap();
        assert_eq!(crashed["flight"]["status"], "crashed");
        assert_eq!(crashed["flight"]["multiplier"], 100_000);
        assert_eq!(crashed["flight"]["cashout_multiplier"], cashout_multiplier);
        assert_eq!(crashed["flight"]["payout"], cashout_multiplier);
        assert_eq!(crashed["balance"], paid_balance);
        let next = start(&data, player, "pilot", "100", None, Some(5))
            .unwrap()
            .unwrap();
        assert_eq!(next["balance"], paid_balance - 100);
        assert!(cash_out(&data, player, 5).unwrap().is_err());
        assert!(start(&data, player, "pilot", "100", None, Some(5))
            .unwrap()
            .is_err());
    }

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
                cinematic_peak: Some(10_000),
                auto: Some(target),
                launch,
                outcome: None,
                finished_at: None,
            };
            data.rocket.flights().insert(player, flight);
            run_flight(data.clone(), player, id).await;
            let snapshot = cash_out(&data, player, id).unwrap().unwrap();
            assert_eq!(snapshot["flight"]["status"], "crashed");
            assert_eq!(
                snapshot["flight"]["multiplier"],
                if expected > 0 { 10_000 } else { 200 }
            );
            assert_eq!(snapshot["flight"]["payout"].as_i64().unwrap_or(0), expected);
            assert_eq!(snapshot["flight"]["automatic"], expected > 0);
        }
        assert_eq!(data.db.wallet(1, 2, "pilot", 500).unwrap().0, 350);
    }

    #[tokio::test]
    async fn failed_cinematic_cashout_refunds_the_stake_and_ends_the_flight() {
        let data = std::sync::Arc::new(Data::new(
            crate::db::Database::in_memory().unwrap(),
            &crate::config::Settings::default(),
            Default::default(),
            false,
        ));
        let player = (1, 2);
        data.db.wallet(1, 2, "pilot", 500).unwrap();
        wagers::place(&data.db, 5, 5, 1, 2, "100", 10)
            .unwrap()
            .unwrap();
        data.db
            .lock()
            .execute_batch(
                "CREATE TRIGGER reject_rocket_payout BEFORE UPDATE OF balance ON economy
             WHEN NEW.balance > 500 BEGIN SELECT RAISE(ABORT, 'test payout failure'); END;",
            )
            .unwrap();
        data.rocket.flights().insert(
            player,
            Flight {
                id: 5,
                stake: 100,
                crash_at: 300,
                cinematic_peak: Some(100_000),
                auto: Some(200),
                launch: Instant::now() - Duration::from_secs(8),
                outcome: None,
                finished_at: None,
            },
        );

        run_flight(data.clone(), player, 5).await;
        let restored = read(&data, player).unwrap();
        assert_eq!(restored["balance"], 500);
        assert_eq!(restored["flight"]["status"], "refunded");
        assert_eq!(restored["flight"]["payout"], Value::Null);
        assert_eq!(restored["flight"]["cinematic_from_ms"], Value::Null);
        assert_eq!(wagers::refund(&data.db, Some(5)).unwrap(), 0);
    }

    #[test]
    fn auto_pays_before_crash_and_manual_retry_returns_the_same_payout() {
        let data = Data::new(
            crate::db::Database::in_memory().unwrap(),
            &crate::config::Settings::default(),
            Default::default(),
            false,
        );
        let player = (1, 2);
        data.db.wallet(1, 2, "pilot", 500).unwrap();
        wagers::place(&data.db, 5, 5, 1, 2, "100", 10)
            .unwrap()
            .unwrap();
        data.rocket.flights().insert(
            player,
            Flight {
                id: 5,
                stake: 100,
                crash_at: 300,
                cinematic_peak: None,
                auto: Some(200),
                launch: Instant::now() - Duration::from_secs(8),
                outcome: None,
                finished_at: None,
            },
        );
        let paid = read(&data, player).unwrap();
        assert_eq!(paid["flight"]["status"], "flying");
        assert_eq!(paid["flight"]["cashout_multiplier"], 200);
        assert_eq!(paid["flight"]["payout"], 200);
        assert_eq!(paid["flight"]["automatic"], true);
        assert_eq!(paid["balance"], 600);
        let retried = cash_out(&data, player, 5).unwrap().unwrap();
        assert_eq!(retried["flight"]["payout"], 200);
        assert_eq!(retried["flight"]["automatic"], true);
        assert_eq!(retried["balance"], 600);
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
