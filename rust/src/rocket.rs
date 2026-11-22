//! Ракетка в Discord Activity. Часы, взрыв и выплата принадлежат серверу.

use crate::economy::MAX_BET;
use crate::{wagers, Data, Shared};
use poise::serenity_prelude as s;
use rand::Rng;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use tokio::{sync::Notify, time::Instant};

const ROUND_PERIOD: Duration = Duration::from_secs(15);
const CRASH_HOLD: Duration = Duration::from_millis(900);
const REFUND_RETRY: Duration = Duration::from_secs(2);
const RECEIPT_WINDOW: Duration = Duration::from_secs(12 * 3600);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(30 * 60);
const MAX_WAGER_CRASH: u32 = 1000;
const DOUBLE_SECONDS: f64 = 2.7;
const CINEMATIC_DOUBLE_SECONDS: f64 = 0.25;
const ACTIVITY_ID: u64 = 1 << 63;

type Player = (u64, u64); // сервер, игрок

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Open,
    Crashed,
    Paid {
        multiplier: u32,
        payout: i64,
        automatic: bool,
    },
    RefundPending(Instant),
    Refunded,
}

struct Wager {
    ticket: u64,
    stake: i64,
    auto: Option<u32>,
    outcome: Outcome,
}

#[derive(Clone, Copy)]
struct Round {
    id: u64,
    launch: Instant,
    crash_at: u32,
    cinematic_peak: Option<u32>,
    has_paid: bool,
}

struct Game {
    round: Round,
    wagers: HashMap<Player, Wager>,
    refunds: Vec<Wager>,
    receipts: HashMap<Player, (Round, Wager)>,
    next_cleanup: Instant,
}

pub struct State {
    game: Mutex<Game>,
    changed: Notify,
}

impl Default for State {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            game: Mutex::new(Game {
                round: Round::new(
                    ACTIVITY_ID | rand::random_range(0..(1 << 62)),
                    now + ROUND_PERIOD,
                ),
                wagers: HashMap::new(),
                refunds: Vec::new(),
                receipts: HashMap::new(),
                next_cleanup: now + CLEANUP_INTERVAL,
            }),
            changed: Notify::new(),
        }
    }
}

impl State {
    fn game(&self) -> MutexGuard<'_, Game> {
        self.game.lock().unwrap_or_else(|error| error.into_inner())
    }
}

pub fn start_cleanup(data: Shared) {
    tokio::spawn(async move {
        loop {
            let deadline = {
                let mut game = data.rocket.game();
                let now = Instant::now();
                game.advance(&data, now);
                game.next_deadline(now)
            };
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {}
                _ = data.rocket.changed.notified() => {}
            }
        }
    });
}

fn crash_point(random: u32) -> u32 {
    (97_000_000 / random).clamp(100, MAX_WAGER_CRASH)
}

fn cinematic_peak(roll: u32, rng: &mut impl Rng) -> Option<u32> {
    // One independent roll per round. Reveal the tail only after the wager crash
    // and only if a payout succeeded. The two bands occur in 1% and 9% of paid rounds.
    match roll {
        1..=10 => Some(rng.random_range(80_000..=120_000)),
        11..=100 => Some(rng.random_range(8_000..=12_000)),
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

impl Wager {
    fn refund(&mut self, data: &Data, now: Instant) {
        self.outcome = match wagers::refund(&data.db, Some(self.ticket)) {
            Ok(_) => Outcome::Refunded,
            Err(error) => {
                eprintln!("Ракетка {}: возврат не удался: {error}", self.ticket);
                Outcome::RefundPending(now + REFUND_RETRY)
            }
        };
    }

    fn settle(&mut self, data: &Data, now: Instant, cashout: Option<(u32, bool)>) {
        let outcome = cashout.map_or(Outcome::Crashed, |(multiplier, automatic)| Outcome::Paid {
            multiplier,
            payout: self.stake * i64::from(multiplier) / 100,
            automatic,
        });
        let payout = match outcome {
            Outcome::Paid { payout, .. } => payout,
            _ => 0,
        };
        // Each ticket is its own ledger round so one player can settle independently.
        match wagers::settle(&data.db, self.ticket, &[(self.ticket, payout)]) {
            Ok(()) => self.outcome = outcome,
            Err(error) => {
                eprintln!("Ракетка {}: расчёт не удался: {error}", self.ticket);
                self.refund(data, now);
            }
        }
    }
}

impl Round {
    fn new(id: u64, launch: Instant) -> Self {
        let mut rng = rand::rng();
        Self {
            id,
            launch,
            crash_at: crash_point(rng.random_range(1..=1_000_000)),
            cinematic_peak: cinematic_peak(rng.random_range(1..=1000), &mut rng),
            has_paid: false,
        }
    }

    fn wager_crash(&self) -> Instant {
        self.launch + flight_time(self.crash_at)
    }

    fn end_multiplier(&self) -> u32 {
        if self.has_paid {
            return self.cinematic_peak.unwrap_or(self.crash_at);
        }
        self.crash_at
    }

    fn display_crash(&self) -> Instant {
        let ratio = f64::from(self.end_multiplier()) / f64::from(self.crash_at);
        self.wager_crash() + Duration::from_secs_f64(ratio.log2() * CINEMATIC_DOUBLE_SECONDS)
    }

    fn automatic_cashout(&self, wager: &Wager) -> Option<(Instant, u32)> {
        wager
            .auto
            .filter(|target| *target < self.crash_at)
            .map(|target| (self.launch + flight_time(target), target))
    }

    fn payout_multiplier(&self, now: Instant) -> u32 {
        let elapsed = now.saturating_duration_since(self.launch).as_secs_f64();
        ((100.0 * (elapsed / DOUBLE_SECONDS).exp2()) as u32)
            .min(self.crash_at.saturating_sub(1))
            .max(100)
    }

    fn next_launch(&self, now: Instant) -> Instant {
        if now < self.launch {
            self.launch
        } else {
            self.launch + ROUND_PERIOD
        }
    }

    fn snapshot(&self, wager: Option<&Wager>, now: Instant, next_launch: Instant) -> Value {
        let wager_crash = self.wager_crash();
        let display_crash = self.display_crash();
        let at = now.min(display_crash);
        let countdown = at < self.launch;
        let next_launch_ms = next_launch
            .saturating_duration_since(now)
            .as_nanos()
            .div_ceil(1_000_000) as u64;
        let elapsed_ms = if countdown {
            -(next_launch_ms as i64)
        } else {
            (at - self.launch).as_millis() as i64
        };
        let outcome = wager.map(|wager| wager.outcome);
        // Instant can tolerate tiny reversed durations on Windows. The displayed
        // phase and cashout gate must both use the same exact deadline comparison.
        let cinematic_elapsed = (self.end_multiplier() > self.crash_at && at >= wager_crash)
            .then(|| at - wager_crash);
        let (status, multiplier) = match cinematic_elapsed {
            _ if countdown => ("countdown", 100),
            _ if now >= display_crash => ("crashed", self.end_multiplier()),
            Some(elapsed) => {
                let factor = f64::from(self.crash_at)
                    * (elapsed.as_secs_f64() / CINEMATIC_DOUBLE_SECONDS).exp2();
                ("flying", (factor as u32).min(self.end_multiplier() - 1))
            }
            _ => ("flying", self.payout_multiplier(at)),
        };
        let status = if outcome == Some(Outcome::Refunded) {
            "refunded"
        } else {
            status
        };
        let (cashout_multiplier, payout, automatic) = match outcome {
            Some(Outcome::Paid {
                multiplier,
                payout,
                automatic,
            }) => (Some(multiplier), Some(payout), automatic),
            _ => (None, None, false),
        };
        let cinematic_from_ms =
            cinematic_elapsed.map(|_| (wager_crash - self.launch).as_millis() as u64);
        json!({
            "id": self.id.to_string(), "status": status, "stake": wager.map(|wager| wager.stake),
            "elapsed_ms": elapsed_ms, "multiplier": multiplier, "auto": wager.and_then(|wager| wager.auto),
            "cashout_multiplier": cashout_multiplier, "payout": payout, "automatic": automatic,
            "cinematic_from_ms": cinematic_from_ms, "next_launch_ms": next_launch_ms,
            "can_bet": countdown && wager.is_none(),
            "can_cashout": !countdown && now < wager_crash && outcome == Some(Outcome::Open),
        })
    }
}

impl Game {
    fn advance(&mut self, data: &Data, now: Instant) {
        if now >= self.next_cleanup {
            self.receipts
                .retain(|_, (round, _)| now - round.display_crash() <= RECEIPT_WINDOW);
            self.next_cleanup = now + CLEANUP_INTERVAL;
        }
        self.refunds.retain_mut(|wager| {
            if matches!(wager.outcome, Outcome::RefundPending(at) if now >= at) {
                wager.refund(data, now);
            }
            wager.outcome != Outcome::Refunded
        });

        loop {
            let wager_crash = self.round.wager_crash();
            for wager in self.wagers.values_mut() {
                match wager.outcome {
                    Outcome::Open => {
                        // Settle an eligible automatic cashout even after a late wakeup.
                        let auto = self
                            .round
                            .automatic_cashout(wager)
                            .filter(|(at, _)| now >= *at);
                        if let Some((_, target)) = auto {
                            wager.settle(data, now, Some((target, true)));
                            self.round.has_paid |= matches!(wager.outcome, Outcome::Paid { .. });
                        } else if now >= wager_crash {
                            wager.settle(data, now, None);
                        }
                    }
                    Outcome::RefundPending(at) if now >= at => wager.refund(data, now),
                    _ => {}
                }
            }
            if now < self.round.display_crash() + CRASH_HOLD {
                return;
            }

            for (player, wager) in self.wagers.drain() {
                match wager.outcome {
                    Outcome::Paid { .. } => {
                        self.receipts.insert(player, (self.round, wager));
                    }
                    Outcome::RefundPending(_) => self.refunds.push(wager),
                    _ => {}
                }
            }
            // The launch cadence survives delays. Skip elapsed slots only after
            // settling the round that could have accepted wagers.
            let elapsed = now - self.round.launch;
            let periods = (elapsed.as_secs() / ROUND_PERIOD.as_secs()).max(1);
            let launch = self.round.launch + Duration::from_secs(periods * ROUND_PERIOD.as_secs());
            self.round = Round::new(self.round.id + periods, launch);
        }
    }

    fn next_deadline(&self, now: Instant) -> Instant {
        let round = &self.round;
        let phase = if now < round.launch {
            round.launch
        } else if now < round.wager_crash() {
            round.wager_crash()
        } else if now < round.display_crash() {
            round.display_crash()
        } else {
            round.display_crash() + CRASH_HOLD
        };
        self.wagers
            .values()
            .chain(&self.refunds)
            .filter_map(|wager| match wager.outcome {
                Outcome::Open => round.automatic_cashout(wager).map(|(at, _)| at),
                Outcome::RefundPending(at) => Some(at),
                _ => None,
            })
            .fold(phase.min(self.next_cleanup), Instant::min)
    }

    fn snapshot(&self, player: Player, now: Instant) -> Value {
        self.round
            .snapshot(self.wagers.get(&player), now, self.round.next_launch(now))
    }
}

fn result(data: &Data, player: Player, flight: Value) -> rusqlite::Result<Value> {
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
    Ok(json!({
        "balance": balance, "min_bet": data.economy.min_bet, "max_bet": MAX_BET,
        "flight": flight,
    }))
}

pub fn read(data: &Data, player: Player) -> rusqlite::Result<Value> {
    let mut game = data.rocket.game();
    let now = Instant::now();
    game.advance(data, now);
    result(data, player, game.snapshot(player, now))
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
    let mut game = data.rocket.game();
    let now = Instant::now();
    game.advance(data, now);
    if expected != Some(game.round.id) {
        return Ok(Err(
            "Раунд уже изменился. Проверь состояние игры и поставь снова.".into(),
        ));
    }
    let launch = game.round.launch;
    let closed = "Приём ставок на этот раунд закрыт.";
    if now >= launch {
        return Ok(Err(closed.into()));
    }
    if game.wagers.contains_key(&player) {
        return Ok(Err("У тебя уже есть ставка на этот раунд.".into()));
    }
    data.db
        .wallet(player.0, player.1, name, data.economy.start_balance)?;
    // The high bit separates Activity tickets from positive Discord identifiers.
    let ticket = rand::random::<u64>() | ACTIVITY_ID;
    let (stake, accepted_at) = match data.db.transaction(|tx| {
        let accepted_at = Instant::now();
        if accepted_at >= launch {
            return Ok(Err(closed.into()));
        }
        let stake = wagers::place_on(
            tx,
            ticket,
            ticket,
            player.0,
            player.1,
            bet,
            data.economy.min_bet,
        )?;
        Ok(stake.map(|stake| (stake, accepted_at)))
    })? {
        Ok(accepted) => accepted,
        Err(error) => return Ok(Err(error)),
    };
    game.wagers.insert(
        player,
        Wager {
            ticket,
            stake,
            auto,
            outcome: Outcome::Open,
        },
    );
    game.receipts.remove(&player);
    data.rocket.changed.notify_one();
    Ok(Ok(result(
        data,
        player,
        game.snapshot(player, accepted_at),
    )?))
}

pub fn cash_out(data: &Data, player: Player, id: u64) -> rusqlite::Result<Result<Value, String>> {
    let mut game = data.rocket.game();
    let now = Instant::now();
    game.advance(data, now);
    if id != game.round.id {
        if let Some((round, wager)) = game
            .receipts
            .get(&player)
            .filter(|(round, _)| round.id == id && now - round.display_crash() <= RECEIPT_WINDOW)
        {
            let flight = round.snapshot(Some(wager), now, game.round.next_launch(now));
            return Ok(Ok(result(data, player, flight)?));
        }
        return Ok(Err("Этот раунд уже закрыт.".into()));
    }
    let launch = game.round.launch;
    let multiplier = game.round.payout_multiplier(now);
    let Some(wager) = game.wagers.get_mut(&player) else {
        return Ok(Err("Этот раунд уже закрыт.".into()));
    };
    if now < launch {
        return Ok(Err("Ракета ещё не стартовала.".into()));
    }
    if wager.outcome == Outcome::Open {
        wager.settle(data, now, Some((multiplier, false)));
        game.round.has_paid |= matches!(wager.outcome, Outcome::Paid { .. });
        data.rocket.changed.notify_one();
    }
    Ok(Ok(result(data, player, game.snapshot(player, now))?))
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
    use rand::{rngs::StdRng, SeedableRng};
    use std::{collections::HashSet, sync::Arc};

    const PILOT: Player = (1, 2);
    const OTHER: Player = (1, 3);
    const SPECTATOR: Player = (1, 4);

    fn test_data() -> Shared {
        Arc::new(Data::new(
            crate::db::Database::in_memory().unwrap(),
            &crate::config::Settings::default(),
            Default::default(),
            false,
        ))
    }

    fn set_round(data: &Data, launch: Instant, crash_at: u32, cinematic_peak: Option<u32>) -> u64 {
        let mut game = data.rocket.game();
        game.round.launch = launch;
        game.round.crash_at = crash_at;
        game.round.cinematic_peak = cinematic_peak;
        game.round.id
    }

    fn bet(data: &Shared, player: Player, id: u64, auto: Option<&str>) -> Value {
        start(data, player, "pilot", "100", auto, Some(id))
            .unwrap()
            .unwrap()
    }

    fn balance(data: &Data, player: Player) -> i64 {
        data.db.wallet(player.0, player.1, "pilot", 500).unwrap().0
    }

    #[test]
    fn spectators_share_a_scheduled_round_and_can_bet() {
        let data = test_data();
        let first = read(&data, PILOT).unwrap();
        let second = read(&data, OTHER).unwrap();
        assert!(first["flight"]["id"].is_string());
        assert_eq!(first["flight"]["id"], second["flight"]["id"]);
        assert_eq!(first["flight"]["status"], "countdown");
        assert_eq!(first["flight"]["stake"], Value::Null);
        assert_eq!(first["flight"]["payout"], Value::Null);
        assert_eq!(first["flight"]["cashout_multiplier"], Value::Null);
        assert_eq!(first["flight"]["cinematic_from_ms"], Value::Null);
        assert_eq!(first["flight"]["can_bet"], true);
        assert_eq!(first["flight"]["can_cashout"], false);
        let next_launch_ms = first["flight"]["next_launch_ms"].as_i64().unwrap();
        assert!((1..=15_000).contains(&next_launch_ms));
        assert_eq!(first["flight"]["elapsed_ms"], -next_launch_ms);
    }

    #[test]
    fn countdown_takes_one_debit_per_player_and_late_bets_are_rejected() {
        let data = test_data();
        let launch = Instant::now() + ROUND_PERIOD;
        let id = set_round(&data, launch, 300, None);
        for expected in [None, Some(id - 1)] {
            assert!(start(&data, PILOT, "pilot", "100", None, expected)
                .unwrap()
                .is_err());
        }
        let opened = bet(&data, PILOT, id, Some("2.00"));
        assert_eq!(opened["balance"], 400);
        assert_eq!(opened["flight"]["id"], id.to_string());
        assert_eq!(opened["flight"]["stake"], 100);
        assert_eq!(opened["flight"]["auto"], 200);
        assert_eq!(opened["flight"]["can_bet"], false);
        assert_eq!(opened["flight"]["can_cashout"], false);
        assert!(start(&data, PILOT, "pilot", "100", None, Some(id))
            .unwrap()
            .is_err());
        assert_eq!(bet(&data, OTHER, id, None)["balance"], 400);
        assert_eq!(data.rocket.game().round.launch, launch);
        assert!(cash_out(&data, PILOT, id).unwrap().is_err());
        assert_eq!(balance(&data, PILOT), 400);

        set_round(&data, Instant::now() - Duration::from_secs(1), 300, None);
        assert_eq!(read(&data, PILOT).unwrap()["flight"]["can_cashout"], true);
        assert_eq!(
            read(&data, SPECTATOR).unwrap()["flight"]["can_cashout"],
            false
        );
        assert!(cash_out(&data, SPECTATOR, id).unwrap().is_err());
        assert!(start(&data, SPECTATOR, "pilot", "100", None, Some(id))
            .unwrap()
            .is_err());
        assert_eq!(balance(&data, SPECTATOR), 500);

        let paid = cash_out(&data, PILOT, id).unwrap().unwrap();
        let amount = paid["flight"]["payout"].as_i64().unwrap();
        assert_eq!(paid["flight"]["status"], "flying");
        assert_eq!(paid["flight"]["cashout_multiplier"], amount);
        assert_eq!(paid["flight"]["can_cashout"], false);
        assert_eq!(paid["flight"]["automatic"], false);
        let replay = cash_out(&data, PILOT, id).unwrap().unwrap();
        assert_eq!(replay["flight"]["payout"], amount);
        assert_eq!(replay["balance"], 400 + amount);
        assert_eq!(read(&data, OTHER).unwrap()["flight"]["can_cashout"], true);
        assert_eq!(balance(&data, OTHER), 400);
    }

    #[test]
    fn simultaneous_bets_for_the_same_player_debit_once() {
        let data = test_data();
        let id = data.rocket.game().round.id;
        let ready = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let place = || {
                ready.wait();
                start(&data, PILOT, "pilot", "100", None, Some(id))
                    .unwrap()
                    .is_ok()
            };
            let first = scope.spawn(place);
            let second = scope.spawn(place);
            assert_ne!(first.join().unwrap(), second.join().unwrap());
        });
        assert_eq!(balance(&data, PILOT), 400);
        assert_eq!(data.rocket.game().wagers.len(), 1);
    }

    #[test]
    fn shared_tail_reveals_no_peak_early_and_cannot_pay_a_losing_wager() {
        let data = test_data();
        let launch = Instant::now() + ROUND_PERIOD;
        let id = set_round(&data, launch, 400, Some(100_000));
        bet(&data, PILOT, id, Some("2.00"));
        bet(&data, OTHER, id, Some("4.00"));
        bet(&data, (1, 5), id, Some("4.01"));

        let mut game = data.rocket.game();
        let real_crash = game.round.wager_crash();
        let before = real_crash - Duration::from_nanos(1);
        game.advance(&data, before);
        for player in [PILOT, OTHER, SPECTATOR] {
            let flight = game.snapshot(player, before);
            assert_eq!(flight["id"], id.to_string());
            assert_eq!(flight["status"], "flying");
            assert_eq!(flight["multiplier"], 399);
            assert_eq!(flight["next_launch_ms"], 9_601);
            assert_eq!(flight["cinematic_from_ms"], Value::Null);
            assert!(flight.get("crash_at").is_none());
            assert!(flight.get("cinematic_peak").is_none());
        }
        assert_eq!(game.snapshot(PILOT, before)["payout"], 200);
        assert_eq!(game.snapshot(OTHER, before)["can_cashout"], true);

        // Equality at the actual crash never pays; the cosmetic phase is view-only.
        game.advance(&data, real_crash);
        for player in [PILOT, OTHER, SPECTATOR] {
            let flight = game.snapshot(player, real_crash);
            assert_eq!(flight["multiplier"], 400);
            assert_eq!(flight["cinematic_from_ms"], 5_400);
            assert_eq!(flight["can_cashout"], false);
        }
        let during = real_crash + Duration::from_millis(250);
        game.advance(&data, during);
        for player in [PILOT, OTHER, (1, 5), SPECTATOR] {
            let flight = game.snapshot(player, during);
            assert_eq!(flight["id"], id.to_string());
            assert_eq!(flight["status"], "flying");
            assert_eq!(flight["multiplier"], 800);
            assert_eq!(flight["cinematic_from_ms"], 5_400);
            assert_eq!(flight["next_launch_ms"], 9_350);
            assert_eq!(flight["can_bet"], false);
            assert_eq!(flight["can_cashout"], false);
        }
        assert_eq!(game.snapshot(PILOT, during)["payout"], 200);
        assert_eq!(game.snapshot(PILOT, during)["automatic"], true);
        assert_eq!(game.snapshot(OTHER, during)["payout"], Value::Null);
        assert_eq!(game.snapshot((1, 5), during)["payout"], Value::Null);
        assert_eq!(game.snapshot(SPECTATOR, during)["stake"], Value::Null);
        assert_eq!(balance(&data, PILOT), 600);
        assert_eq!(balance(&data, OTHER), 400);
        assert_eq!(balance(&data, (1, 5)), 400);

        let end = game.round.display_crash();
        assert_eq!(
            game.snapshot(OTHER, end - Duration::from_nanos(1))["multiplier"],
            99_999
        );
        game.advance(&data, end);
        for player in [PILOT, OTHER, SPECTATOR] {
            let flight = game.snapshot(player, end);
            assert_eq!(flight["status"], "crashed");
            assert_eq!(flight["multiplier"], 100_000);
            assert_eq!(flight["cinematic_from_ms"], 5_400);
        }
        let hold_end = end + CRASH_HOLD;
        game.advance(&data, hold_end - Duration::from_nanos(1));
        assert_eq!(game.round.id, id);
        game.advance(&data, hold_end);
        assert_eq!(game.round.id, id + 1);
        assert_eq!(game.round.launch, launch + ROUND_PERIOD);
        assert!(game.wagers.is_empty());
        assert_eq!(game.snapshot(PILOT, hold_end)["can_bet"], true);
    }

    #[test]
    fn late_manual_cashout_during_the_shared_tail_cannot_turn_a_loss_into_a_payout() {
        let data = test_data();
        let launch = Instant::now() + ROUND_PERIOD;
        let id = set_round(&data, launch, 400, Some(100_000));
        bet(&data, PILOT, id, Some("2.00"));
        bet(&data, OTHER, id, None);
        set_round(
            &data,
            Instant::now() - Duration::from_millis(5_650),
            400,
            Some(100_000),
        );

        let late = cash_out(&data, OTHER, id).unwrap().unwrap();
        assert_eq!(late["flight"]["status"], "flying");
        assert_eq!(late["flight"]["cinematic_from_ms"], 5_400);
        assert_eq!(late["flight"]["payout"], Value::Null);
        assert_eq!(late["flight"]["can_cashout"], false);
        assert_eq!(late["balance"], 400);
        assert_eq!(balance(&data, PILOT), 600);
    }

    #[test]
    fn paid_receipt_replays_after_rollover_until_the_next_bet_or_cleanup() {
        let data = test_data();
        let id = data.rocket.game().round.id;
        bet(&data, PILOT, id, None);
        set_round(&data, Instant::now() - Duration::from_secs(1), 300, None);
        let paid = cash_out(&data, PILOT, id).unwrap().unwrap();

        set_round(&data, Instant::now() - Duration::from_secs(6), 300, None);
        let current = read(&data, PILOT).unwrap();
        assert_eq!(current["flight"]["id"], (id + 1).to_string());
        assert_eq!(current["flight"]["stake"], Value::Null);
        assert_eq!(current["flight"]["status"], "countdown");
        assert!(cash_out(&data, OTHER, id).unwrap().is_err());
        let replay = cash_out(&data, PILOT, id).unwrap().unwrap();
        assert_eq!(replay["flight"]["id"], id.to_string());
        assert_eq!(replay["flight"]["payout"], paid["flight"]["payout"]);
        assert_eq!(replay["flight"]["status"], "crashed");
        assert_eq!(replay["flight"]["can_cashout"], false);
        assert_eq!(replay["balance"], paid["balance"]);
        assert!(start(&data, PILOT, "pilot", "100", None, Some(id))
            .unwrap()
            .is_err());
        bet(&data, PILOT, id + 1, None);
        assert!(cash_out(&data, PILOT, id).unwrap().is_err());

        set_round(&data, Instant::now() - Duration::from_secs(1), 300, None);
        cash_out(&data, PILOT, id + 1).unwrap().unwrap();
        let mut game = data.rocket.game();
        let end = game.round.display_crash();
        game.advance(&data, end + CRASH_HOLD);
        let expires = end + RECEIPT_WINDOW;
        game.advance(&data, expires + CLEANUP_INTERVAL);
        assert!(game.receipts.is_empty());
        assert!(game.wagers.is_empty());
    }

    #[test]
    fn late_advance_settles_auto_then_skips_elapsed_rounds_without_shifting_launches() {
        let data = test_data();
        let launch = Instant::now() + ROUND_PERIOD;
        let id = set_round(&data, launch, 200, Some(10_000));
        bet(&data, PILOT, id, Some("1.50"));
        bet(&data, OTHER, id, Some("2.00"));
        let mut game = data.rocket.game();
        let after = launch + Duration::from_secs(45);
        game.advance(&data, after);
        assert_eq!(game.round.id, id + 3);
        assert_eq!(game.round.launch, after);
        assert_eq!(game.snapshot(PILOT, after)["next_launch_ms"], 15_000);
        assert_eq!(balance(&data, PILOT), 550);
        assert_eq!(balance(&data, OTHER), 400);
        assert_eq!(game.receipts.len(), 1);
        assert!(game.wagers.is_empty());
    }

    #[test]
    fn failed_payout_refunds_only_that_player_and_other_wager_settles() {
        let data = test_data();
        let launch = Instant::now() + ROUND_PERIOD;
        let id = set_round(&data, launch, 300, Some(100_000));
        bet(&data, PILOT, id, Some("2.00"));
        bet(&data, OTHER, id, Some("2.00"));
        data.db.lock().execute_batch(
            "CREATE TRIGGER reject_rocket_payout BEFORE UPDATE OF balance ON economy
             WHEN NEW.user_id = 2 AND NEW.balance > 500 BEGIN SELECT RAISE(ABORT, 'test payout failure'); END;",
        ).unwrap();

        set_round(
            &data,
            Instant::now() - Duration::from_millis(3_600),
            300,
            Some(100_000),
        );
        let refunded = read(&data, PILOT).unwrap();
        assert_eq!(refunded["flight"]["status"], "refunded");
        assert_eq!(refunded["flight"]["payout"], Value::Null);
        assert_eq!(refunded["flight"]["cinematic_from_ms"], Value::Null);
        assert_eq!(refunded["flight"]["can_bet"], false);
        assert_eq!(refunded["flight"]["can_cashout"], false);
        assert_eq!(refunded["balance"], 500);
        let replay = cash_out(&data, PILOT, id).unwrap().unwrap();
        assert_eq!(replay["balance"], 500);
        assert_eq!(read(&data, OTHER).unwrap()["flight"]["payout"], 200);
        assert_eq!(balance(&data, OTHER), 600);
    }

    #[test]
    fn failed_refund_survives_rollover_and_cleanup_retries_it_once() {
        let data = test_data();
        let launch = Instant::now() + ROUND_PERIOD;
        let id = set_round(&data, launch, 100, Some(100_000));
        bet(&data, PILOT, id, None);
        data.db
            .lock()
            .execute_batch(
                "CREATE TRIGGER reject_ticket_deletion BEFORE DELETE ON game_wagers
             BEGIN SELECT RAISE(ABORT, 'test ticket deletion failure'); END;",
            )
            .unwrap();

        let mut game = data.rocket.game();
        game.advance(&data, launch);
        assert_eq!(game.snapshot(PILOT, launch)["status"], "crashed");
        assert_eq!(game.snapshot(PILOT, launch)["multiplier"], 100);
        assert_eq!(game.snapshot(PILOT, launch)["can_cashout"], false);
        game.advance(&data, launch + CRASH_HOLD);
        assert_eq!(game.round.id, id + 1);
        assert!(game.wagers.is_empty());
        assert_eq!(game.refunds.len(), 1);
        assert_eq!(balance(&data, PILOT), 400);

        data.db
            .lock()
            .execute_batch("DROP TRIGGER reject_ticket_deletion;")
            .unwrap();
        game.advance(&data, launch + REFUND_RETRY);
        game.advance(&data, launch + REFUND_RETRY * 2);
        assert!(game.refunds.is_empty());
        assert_eq!(balance(&data, PILOT), 500);
    }

    #[test]
    fn cinematic_bands_have_exact_odds() {
        let mut rng = StdRng::seed_from_u64(42);
        let mut peaks = [HashSet::new(), HashSet::new()];
        let mut counts = [0, 0];
        for roll in 1..=1000 {
            let tier = match cinematic_peak(roll, &mut rng) {
                Some(peak @ 8_000..=12_000) => (0, peak),
                Some(peak @ 80_000..=120_000) => (1, peak),
                peak => {
                    assert_eq!(peak, None);
                    continue;
                }
            };
            let (band, peak) = tier;
            peaks[band].insert(peak);
            counts[band] += 1;
        }
        assert_eq!(counts, [90, 10]);
        assert!(peaks.iter().all(|band| band.len() > 1));
    }

    #[test]
    fn longest_flight_and_crash_hold_leave_exact_betting_window() {
        let launch = Instant::now();
        let longest_flight = (100..=MAX_WAGER_CRASH)
            // Both bands are bounded by 1200×, and flight time increases with the peak.
            .map(|crash_at| Round {
                id: 1,
                launch,
                crash_at,
                cinematic_peak: Some(120_000),
                has_paid: true,
            })
            .map(|round| round.display_crash() - launch)
            .max()
            .unwrap();
        let period = Duration::from_secs(15);
        assert_eq!(ROUND_PERIOD, period);
        assert_eq!(longest_flight, Duration::from_nanos(10_695_928_505));
        assert_eq!(
            period - longest_flight - CRASH_HOLD,
            Duration::from_nanos(3_404_071_495)
        );
    }

    async fn eventually(check: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(4), async {
            while !check() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Rocket worker missed a deadline");
    }

    #[tokio::test]
    async fn worker_advances_an_empty_round_on_natural_time() {
        let data = test_data();
        let launch = Instant::now() + Duration::from_millis(50);
        let id = set_round(&data, launch, 100, Some(120_000));
        start_cleanup(data.clone());
        // Inspect the stored clock without calling read, which could advance it itself.
        eventually(|| data.rocket.game().round.id == id + 1).await;
        let game = data.rocket.game();
        assert_eq!(game.round.launch, launch + Duration::from_secs(15));
        assert!(game.wagers.is_empty());
        assert!(game.receipts.is_empty());
    }

    #[tokio::test]
    async fn worker_pays_an_offline_auto_cashout_on_natural_time() {
        let data = test_data();
        let id = data.rocket.game().round.id;
        bet(&data, PILOT, id, Some("1.01"));
        set_round(&data, Instant::now() + Duration::from_millis(50), 300, None);
        start_cleanup(data.clone());
        eventually(|| balance(&data, PILOT) == 501).await;
        assert_eq!(wagers::refund(&data.db, None).unwrap(), 0);
        let round = data.rocket.game();
        assert_eq!(round.round.id, id);
        assert_eq!(
            round.wagers[&PILOT].outcome,
            Outcome::Paid {
                multiplier: 101,
                payout: 101,
                automatic: true,
            }
        );
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
}
