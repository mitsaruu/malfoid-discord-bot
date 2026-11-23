//! Ракетка в Discord Activity. Часы, взрыв и выплата принадлежат серверу.

use crate::{config::Settings, economy::MAX_BET};
use crate::{wagers, Data, Shared};
use poise::serenity_prelude as s;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::ops::{Range, RangeInclusive};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use tokio::{sync::Notify, time::Instant};

const ROUND_PERIOD: Duration = Duration::from_secs(15);
const MIN_COUNTDOWN: Duration = Duration::from_secs(3);
const CRASH_HOLD: Duration = Duration::from_millis(900);
const REFUND_RETRY: Duration = Duration::from_secs(2);
const RECEIPT_WINDOW: Duration = Duration::from_secs(12 * 3600);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(30 * 60);
const CRASH_CAP: u32 = 100_000;
const GRACE_MS: f64 = 1_200.0;
const DOUBLE_MS: f64 = 7_000.0;
const GROWTH_POWER: f64 = 1.35;
const HISTORY_LIMIT: usize = 16;
const PPM: u32 = 1_000_000;
const REACH_TIERS: [(u32, &str, u32); 5] = [
    (200, "reach_x2_ppm", 485_000),
    (500, "reach_x5_ppm", 194_000),
    (1_000, "reach_x10_ppm", 97_000),
    (10_000, "reach_x100_ppm", 9_700),
    (CRASH_CAP, "reach_x1000_ppm", 970),
];
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
    name: String,
    stake: i64,
    auto: Option<u32>,
    outcome: Outcome,
}

#[derive(Clone, Copy)]
struct Round {
    id: u64,
    launch: Instant,
    crash_at: u32,
}

struct ReachOdds([u32; REACH_TIERS.len()]);

struct Game {
    round: Round,
    odds: ReachOdds,
    history: VecDeque<(u64, u32)>,
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
        Self::with_settings(&Settings::default())
    }
}

impl State {
    pub fn with_settings(settings: &Settings) -> Self {
        let now = Instant::now();
        let odds = ReachOdds::load(settings);
        Self {
            game: Mutex::new(Game {
                round: Round::new(
                    ACTIVITY_ID | rand::random_range(0..(1 << 62)),
                    now + ROUND_PERIOD,
                    &odds,
                ),
                odds,
                history: VecDeque::with_capacity(HISTORY_LIMIT),
                wagers: HashMap::new(),
                refunds: Vec::new(),
                receipts: HashMap::new(),
                next_cleanup: now + CLEANUP_INTERVAL,
            }),
            changed: Notify::new(),
        }
    }

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

impl ReachOdds {
    fn load(settings: &Settings) -> Self {
        let mut previous = PPM;
        Self(REACH_TIERS.map(|(_, key, default)| {
            let configured = settings.int("rocket", key, i64::from(default));
            let value = if (0..=i64::from(PPM)).contains(&configured) {
                (configured as u32).max(1)
            } else {
                eprintln!("settings.ini: [rocket] {key} = {configured}; using {default} (valid range: 0–{PPM})");
                default
            };
            if value > previous {
                eprintln!("settings.ini: [rocket] {key} = {value}; lowering to {previous} to keep reach odds monotonic");
            }
            previous = previous.min(value);
            previous
        }))
    }

    fn band(&self, roll: u32) -> (RangeInclusive<u32>, Range<u32>) {
        let reached = self.0.iter().take_while(|&&odds| roll < odds).count();
        let (lower, roll_end) = if reached == 0 {
            (100, PPM)
        } else {
            (REACH_TIERS[reached - 1].0, self.0[reached - 1])
        };
        let upper = REACH_TIERS
            .get(reached)
            .map_or(CRASH_CAP, |(minimum, _, _)| minimum - 1);
        let roll_start = self.0.get(reached).copied().unwrap_or(0);
        (lower..=upper, roll_start..roll_end)
    }

    fn crash_at(&self, roll: u32) -> u32 {
        let (band, rolls) = self.band(roll);
        let lower = u64::from(*band.start());
        if lower == u64::from(CRASH_CAP) {
            return CRASH_CAP;
        }

        // Interpolate 1/multiplier at the midpoint of each roll's probability
        // bucket. Integer division floors the result inside the selected band.
        let upper = u64::from(*band.end()) + 1;
        let total = 2 * u64::from(rolls.end - rolls.start);
        let position = 2 * u64::from(roll - rolls.start) + 1;
        let denominator = (total - position) * lower + position * upper;
        (total * lower * upper / denominator) as u32
    }
}

fn flight_time(multiplier: u32) -> Duration {
    let ms = GRACE_MS
        + DOUBLE_MS
            * (f64::from(multiplier) / 100.0)
                .log2()
                .powf(1.0 / GROWTH_POWER);
    Duration::from_nanos((ms * 1_000_000.0).ceil() as u64)
}

fn multiplier_at(elapsed: Duration) -> u32 {
    let elapsed_ms = elapsed.as_secs_f64() * 1_000.0;
    let progress = (elapsed_ms - GRACE_MS).max(0.0) / DOUBLE_MS;
    ((100.0 * progress.powf(GROWTH_POWER).exp2()).floor() as u32).min(CRASH_CAP)
}

fn parse_auto(raw: &str) -> Option<u32> {
    let raw = raw.trim();
    let (whole, fraction) = raw.split_once(['.', ',']).unwrap_or((raw, ""));
    if !(1..=4).contains(&whole.len())
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
    (101..=CRASH_CAP).contains(&value).then_some(value)
}

impl Wager {
    fn snapshot(&self, user: u64) -> Value {
        let status = match self.outcome {
            Outcome::Open | Outcome::RefundPending(_) => "open",
            Outcome::Crashed => "lost",
            Outcome::Paid { .. } => "paid",
            Outcome::Refunded => "refunded",
        };
        let mut bet = json!({
            "id": user.to_string(), "name": self.name, "stake": self.stake, "status": status,
        });
        if let Outcome::Paid {
            multiplier, payout, ..
        } = self.outcome
        {
            bet["multiplier"] = json!(multiplier);
            bet["payout"] = json!(payout);
        }
        bet
    }

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
    fn new(id: u64, launch: Instant, odds: &ReachOdds) -> Self {
        // rand's thread-local CSPRNG is seeded and periodically reseeded by the OS.
        Self {
            id,
            launch,
            crash_at: odds.crash_at(rand::random_range(0..PPM)),
        }
    }

    fn end(&self) -> Instant {
        self.launch + flight_time(self.crash_at)
    }

    fn automatic_cashout(&self, wager: &Wager) -> Option<(Instant, u32)> {
        wager
            .auto
            .filter(|target| *target < self.crash_at)
            .map(|target| (self.launch + flight_time(target), target))
    }

    fn payout_multiplier(&self, now: Instant) -> u32 {
        multiplier_at(now.saturating_duration_since(self.launch))
            .min(self.crash_at - 1)
            .max(100)
    }

    fn following_launch(&self) -> Instant {
        let reserved = flight_time(self.crash_at) + CRASH_HOLD + MIN_COUNTDOWN;
        let periods = reserved.as_nanos().div_ceil(ROUND_PERIOD.as_nanos()) as u32;
        self.launch + ROUND_PERIOD * periods
    }

    fn next_launch(&self, now: Instant) -> Option<Instant> {
        if now < self.launch {
            Some(self.launch)
        } else if now >= self.end() {
            Some(self.following_launch())
        } else {
            // The chosen slot could reveal the crash band before the flight ends.
            None
        }
    }

    fn snapshot(&self, wager: Option<&Wager>, now: Instant, next_launch: Option<Instant>) -> Value {
        let end = self.end();
        let countdown = now < self.launch;
        let ended = now >= end;
        let next_launch_ms = next_launch.map_or(0, |launch| {
            launch
                .saturating_duration_since(now)
                .as_nanos()
                .div_ceil(1_000_000) as u64
        });
        let elapsed_ms = if countdown {
            json!(-(next_launch_ms as i64))
        } else {
            json!((now.min(end) - self.launch).as_secs_f64() * 1_000.0)
        };
        let outcome = wager.map(|wager| wager.outcome);
        let (status, multiplier) = if countdown {
            ("countdown", 100)
        } else if ended {
            ("crashed", self.crash_at)
        } else {
            ("flying", self.payout_multiplier(now))
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
        json!({
            "id": self.id.to_string(), "status": status, "stake": wager.map(|wager| wager.stake),
            "elapsed_ms": elapsed_ms, "multiplier": multiplier, "auto": wager.and_then(|wager| wager.auto),
            "cashout_multiplier": cashout_multiplier, "payout": payout, "automatic": automatic,
            "final_multiplier": ended.then_some(self.crash_at), "next_launch_ms": next_launch_ms,
            "can_bet": countdown && wager.is_none(),
            "can_cashout": !countdown && !ended && outcome == Some(Outcome::Open),
        })
    }
}

impl Game {
    fn advance(&mut self, data: &Data, now: Instant) {
        if now >= self.next_cleanup {
            self.receipts
                .retain(|_, (round, _)| now - round.end() <= RECEIPT_WINDOW);
            self.next_cleanup = now + CLEANUP_INTERVAL;
        }
        self.refunds.retain_mut(|wager| {
            if matches!(wager.outcome, Outcome::RefundPending(at) if now >= at) {
                wager.refund(data, now);
            }
            wager.outcome != Outcome::Refunded
        });

        loop {
            let end = self.round.end();
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
                        } else if now >= end {
                            let cap =
                                (self.round.crash_at == CRASH_CAP).then_some((CRASH_CAP, true));
                            wager.settle(data, now, cap);
                        }
                    }
                    Outcome::RefundPending(at) if now >= at => wager.refund(data, now),
                    _ => {}
                }
            }
            if now < end {
                return;
            }
            if self.history.front().map(|(id, _)| *id) != Some(self.round.id) {
                if self.history.len() == HISTORY_LIMIT {
                    self.history.pop_back();
                }
                self.history
                    .push_front((self.round.id, self.round.crash_at));
            }
            if now < end + CRASH_HOLD {
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
            let previous_launch = self.round.launch;
            let following_launch = self.round.following_launch();
            let period_secs = ROUND_PERIOD.as_secs();
            let periods = ((now - previous_launch).as_secs() / period_secs)
                .max((following_launch - previous_launch).as_secs() / period_secs);
            let launch = previous_launch + Duration::from_secs(periods * period_secs);
            self.round = Round::new(self.round.id + periods, launch, &self.odds);
        }
    }

    fn next_deadline(&self, now: Instant) -> Instant {
        let round = &self.round;
        let end = round.end();
        let phase = if now < round.launch {
            round.launch
        } else if now < end {
            end
        } else {
            end + CRASH_HOLD
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

    fn cash_out(
        &mut self,
        data: &Data,
        player: Player,
        id: u64,
        now: Instant,
    ) -> rusqlite::Result<Result<Value, String>> {
        self.advance(data, now);
        if id != self.round.id {
            if let Some(receipt) = self
                .receipts
                .get(&player)
                .filter(|(round, _)| round.id == id && now - round.end() <= RECEIPT_WINDOW)
            {
                return Ok(Ok(self.response(data, player, now, Some(receipt))?));
            }
            return Ok(Err("Этот раунд уже закрыт.".into()));
        }
        let round = self.round;
        let Some(wager) = self.wagers.get_mut(&player) else {
            return Ok(Err("Этот раунд уже закрыт.".into()));
        };
        if now < round.launch {
            return Ok(Err("Ракета ещё не стартовала.".into()));
        }
        if now < round.end() && wager.outcome == Outcome::Open {
            wager.settle(data, now, Some((round.payout_multiplier(now), false)));
            data.rocket.changed.notify_one();
        }
        Ok(Ok(self.response(data, player, now, None)?))
    }

    fn response(
        &self,
        data: &Data,
        player: Player,
        now: Instant,
        receipt: Option<&(Round, Wager)>,
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
        let (flight, bets) = if let Some((round, wager)) = receipt {
            let flight = round.snapshot(Some(wager), now, self.round.next_launch(now));
            (flight, Vec::new())
        } else {
            let mut local_wagers: Vec<_> = self
                .wagers
                .iter()
                .filter_map(|(&(guild, user), wager)| (guild == player.0).then_some((user, wager)))
                .collect();
            local_wagers.sort_unstable_by_key(|(user, _)| *user);
            let bets = local_wagers
                .into_iter()
                .map(|(user, wager)| wager.snapshot(user))
                .collect();
            (self.snapshot(player, now), bets)
        };
        let history: Vec<_> = self
            .history
            .iter()
            .map(|(id, multiplier)| json!({ "id": id.to_string(), "multiplier": multiplier }))
            .collect();
        Ok(json!({
            "balance": balance, "min_bet": data.economy.min_bet, "max_bet": MAX_BET,
            "flight": flight, "bets": bets, "history": history,
        }))
    }
}

pub fn read(data: &Data, player: Player) -> rusqlite::Result<Value> {
    let mut game = data.rocket.game();
    let now = Instant::now();
    game.advance(data, now);
    game.response(data, player, now, None)
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
                    "Коэффициент автовывода — от 1,01 до 1000, до двух знаков после запятой."
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
            name: name.to_owned(),
            stake,
            auto,
            outcome: Outcome::Open,
        },
    );
    game.receipts.remove(&player);
    data.rocket.changed.notify_one();
    Ok(Ok(game.response(data, player, accepted_at, None)?))
}

pub fn cash_out(data: &Data, player: Player, id: u64) -> rusqlite::Result<Result<Value, String>> {
    let mut game = data.rocket.game();
    game.cash_out(data, player, id, Instant::now())
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
    use std::sync::Arc;

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

    fn set_round(data: &Data, launch: Instant, crash_at: u32) -> u64 {
        let mut game = data.rocket.game();
        game.round.launch = launch;
        game.round.crash_at = crash_at;
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
        assert_eq!(first["flight"]["final_multiplier"], Value::Null);
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
        let id = set_round(&data, launch, 300);
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

        set_round(&data, Instant::now() - Duration::from_secs(1), 300);
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
        assert_eq!(amount, 100);
        assert_eq!(paid["flight"]["cashout_multiplier"], 100);
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
    fn ordinary_crash_is_strict_at_the_exact_boundary() {
        let data = test_data();
        let launch = Instant::now() + ROUND_PERIOD;
        let id = set_round(&data, launch, 400);
        bet(&data, PILOT, id, Some("2.00"));
        bet(&data, OTHER, id, Some("4.00"));
        bet(&data, (1, 5), id, None);

        let mut game = data.rocket.game();
        let end = launch + Duration::from_nanos(12_897_235_187);
        assert_eq!(game.round.end(), end);
        let before = end - Duration::from_nanos(1);
        let paid = game.cash_out(&data, (1, 5), id, before).unwrap().unwrap();
        assert_eq!(paid["flight"]["cashout_multiplier"], 399);
        assert_eq!(paid["flight"]["final_multiplier"], Value::Null);
        assert_eq!(paid["flight"]["next_launch_ms"], 0);
        assert_eq!(game.snapshot(OTHER, before)["can_cashout"], true);

        let late = game.cash_out(&data, OTHER, id, end).unwrap().unwrap();
        assert_eq!(late["flight"]["status"], "crashed");
        assert_eq!(late["flight"]["final_multiplier"], 400);
        assert_eq!(late["flight"]["payout"], Value::Null);
        assert_eq!(late["flight"]["can_cashout"], false);
        assert_eq!(late["balance"], 400);
        assert_eq!(game.snapshot(PILOT, end)["payout"], 200);
        assert_eq!(game.snapshot(PILOT, end)["automatic"], true);
        assert_eq!(
            late["history"],
            json!([{ "id": id.to_string(), "multiplier": 400 }])
        );
        game.advance(&data, end + CRASH_HOLD - Duration::from_nanos(1));
        assert_eq!(game.round.id, id);
        game.advance(&data, end + CRASH_HOLD);
        assert_eq!(game.round.id, id + 2);
        assert_eq!(game.round.launch, launch + ROUND_PERIOD * 2);
        assert!(game.wagers.is_empty());
    }

    #[test]
    fn cap_automatically_pays_remaining_wagers_once() {
        let data = test_data();
        let launch = Instant::now() + ROUND_PERIOD;
        let id = set_round(&data, launch, 100_000);
        bet(&data, PILOT, id, None);
        bet(&data, OTHER, id, Some("1000"));
        bet(&data, SPECTATOR, id, Some("999.99"));

        let mut game = data.rocket.game();
        let end = launch + Duration::from_nanos(39_635_822_146);
        assert_eq!(game.round.end(), end);
        let before = end - Duration::from_nanos(1);
        game.advance(&data, before);
        assert_eq!(game.snapshot(PILOT, before)["multiplier"], 99_999);
        assert_eq!(game.snapshot(PILOT, before)["can_cashout"], true);
        assert_eq!(
            game.snapshot(PILOT, before)["final_multiplier"],
            Value::Null
        );
        assert_eq!(balance(&data, PILOT), 400);
        assert_eq!(
            game.snapshot(SPECTATOR, before)["cashout_multiplier"],
            99_999
        );
        assert_eq!(balance(&data, SPECTATOR), 100_399);

        for (player, at) in [(PILOT, end), (OTHER, end), (PILOT, end + CRASH_HOLD)] {
            let response = game.cash_out(&data, player, id, at).unwrap().unwrap();
            assert_eq!(response["flight"]["final_multiplier"], 100_000);
            assert_eq!(response["flight"]["payout"], 100_000);
            assert_eq!(response["flight"]["automatic"], true);
            assert_eq!(response["balance"], 100_400);
            assert_eq!(
                response["history"],
                json!([{ "id": id.to_string(), "multiplier": 100_000 }])
            );
        }
        assert_eq!(balance(&data, SPECTATOR), 100_399);
        assert_eq!(game.round.launch, launch + ROUND_PERIOD * 3);
    }

    #[test]
    fn round_timeline_is_independent_of_stakes_and_payouts() {
        let empty = test_data();
        let active = test_data();
        let launch = Instant::now() + ROUND_PERIOD;
        let empty_id = set_round(&empty, launch, 500);
        let active_id = set_round(&active, launch, 500);
        bet(&active, PILOT, active_id, Some("2"));
        start(&active, OTHER, "other", "250", None, Some(active_id))
            .unwrap()
            .unwrap();
        let mut empty_game = empty.rocket.game();
        let mut active_game = active.rocket.game();
        active_game
            .cash_out(&active, OTHER, active_id, launch)
            .unwrap()
            .unwrap();

        for ms in [1_199, 1_200, 8_200, 14_264, 14_265, 15_165] {
            let now = launch + Duration::from_millis(ms);
            empty_game.advance(&empty, now);
            active_game.advance(&active, now);
            let mut first = empty_game.snapshot(SPECTATOR, now);
            let mut second = active_game.snapshot(SPECTATOR, now);
            first.as_object_mut().unwrap().remove("id");
            second.as_object_mut().unwrap().remove("id");
            assert_eq!(first, second);
        }
        assert_eq!(empty_game.history.front(), Some(&(empty_id, 500)));
        assert_eq!(active_game.history.front(), Some(&(active_id, 500)));
        assert_eq!(balance(&active, PILOT), 600);
        assert_eq!(balance(&active, OTHER), 500);
    }

    #[test]
    fn bet_roster_is_guild_scoped_and_stays_through_the_crash_hold() {
        let data = test_data();
        let launch = Instant::now() + ROUND_PERIOD;
        let id = set_round(&data, launch, 200);
        let remote = (2, PILOT.1);
        bet(&data, PILOT, id, None);
        let accepted = start(&data, remote, "remote", "55", None, Some(id))
            .unwrap()
            .unwrap();
        assert_eq!(accepted["bets"].as_array().unwrap().len(), 1);
        assert_eq!(accepted["bets"][0]["name"], "remote");
        let roster = read(&data, SPECTATOR).unwrap();
        assert_eq!(roster["flight"]["id"], accepted["flight"]["id"]);
        assert_eq!(roster["bets"].as_array().unwrap().len(), 1);
        assert_eq!(roster["bets"][0]["id"], PILOT.1.to_string());
        assert_eq!(roster["bets"][0]["name"], "pilot");
        assert_eq!(roster["bets"][0]["stake"], 100);
        assert_eq!(roster["bets"][0]["status"], "open");
        assert!(roster["bets"][0].get("payout").is_none());

        let mut game = data.rocket.game();
        let paid = game.cash_out(&data, PILOT, id, launch).unwrap().unwrap();
        assert_eq!(paid["bets"][0]["status"], "paid");
        assert_eq!(paid["bets"][0]["multiplier"], 100);
        assert_eq!(paid["bets"][0]["payout"], 100);
        let end = launch + Duration::from_millis(8_200);
        let lost = game.cash_out(&data, remote, id, end).unwrap().unwrap();
        assert_eq!(lost["bets"].as_array().unwrap().len(), 1);
        assert_eq!(lost["bets"][0]["name"], "remote");
        assert_eq!(lost["bets"][0]["status"], "lost");
        let replay = game
            .cash_out(&data, PILOT, id, end + CRASH_HOLD)
            .unwrap()
            .unwrap();
        assert_eq!(replay["bets"], json!([]));
        assert_eq!(replay["history"], lost["history"]);
        assert_eq!(replay["flight"]["payout"], 100);
    }

    #[test]
    fn paid_receipt_replays_after_rollover_until_the_next_bet_or_cleanup() {
        let data = test_data();
        let id = data.rocket.game().round.id;
        bet(&data, PILOT, id, None);
        set_round(&data, Instant::now() - Duration::from_secs(1), 300);
        let paid = cash_out(&data, PILOT, id).unwrap().unwrap();

        set_round(&data, Instant::now() - Duration::from_secs(13), 300);
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

        set_round(&data, Instant::now() - Duration::from_secs(1), 300);
        cash_out(&data, PILOT, id + 1).unwrap().unwrap();
        let mut game = data.rocket.game();
        let end = game.round.end();
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
        let id = set_round(&data, launch, 200);
        bet(&data, PILOT, id, Some("1.50"));
        bet(&data, OTHER, id, Some("2.00"));
        let mut game = data.rocket.game();
        let after = launch + Duration::from_secs(45);
        game.advance(&data, after);
        assert_eq!(game.round.id, id + 3);
        assert_eq!(game.round.launch, after);
        assert_eq!(game.snapshot(PILOT, after)["next_launch_ms"], 0);
        assert_eq!(balance(&data, PILOT), 550);
        assert_eq!(balance(&data, OTHER), 400);
        assert_eq!(game.receipts.len(), 1);
        assert!(game.wagers.is_empty());
    }

    #[test]
    fn failed_payout_refunds_only_that_player_and_other_wager_settles() {
        let data = test_data();
        let launch = Instant::now() + ROUND_PERIOD;
        let id = set_round(&data, launch, 300);
        bet(&data, PILOT, id, Some("2.00"));
        bet(&data, OTHER, id, Some("2.00"));
        data.db.lock().execute_batch(
            "CREATE TRIGGER reject_rocket_payout BEFORE UPDATE OF balance ON economy
             WHEN NEW.user_id = 2 AND NEW.balance > 500 BEGIN SELECT RAISE(ABORT, 'test payout failure'); END;",
        ).unwrap();

        set_round(&data, Instant::now() - Duration::from_millis(8_300), 300);
        let refunded = read(&data, PILOT).unwrap();
        assert_eq!(refunded["flight"]["status"], "refunded");
        assert_eq!(refunded["flight"]["payout"], Value::Null);
        assert_eq!(refunded["flight"]["final_multiplier"], Value::Null);
        assert!(refunded["bets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|bet| bet["status"] == "refunded"));
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
        let id = set_round(&data, launch, 100);
        bet(&data, PILOT, id, None);
        data.db
            .lock()
            .execute_batch(
                "CREATE TRIGGER reject_ticket_deletion BEFORE DELETE ON game_wagers
             BEGIN SELECT RAISE(ABORT, 'test ticket deletion failure'); END;",
            )
            .unwrap();

        let mut game = data.rocket.game();
        let end = launch + Duration::from_millis(1_200);
        game.advance(&data, end);
        assert_eq!(game.snapshot(PILOT, end)["status"], "crashed");
        assert_eq!(game.snapshot(PILOT, end)["multiplier"], 100);
        assert_eq!(game.snapshot(PILOT, end)["can_cashout"], false);
        game.advance(&data, end + CRASH_HOLD);
        assert_eq!(game.round.id, id + 1);
        assert!(game.wagers.is_empty());
        assert_eq!(game.refunds.len(), 1);
        assert_eq!(balance(&data, PILOT), 400);

        data.db
            .lock()
            .execute_batch("DROP TRIGGER reject_ticket_deletion;")
            .unwrap();
        game.advance(&data, end + REFUND_RETRY);
        game.advance(&data, end + REFUND_RETRY * 2);
        assert!(game.refunds.is_empty());
        assert_eq!(balance(&data, PILOT), 500);
    }

    #[test]
    fn reach_odds_use_configured_monotonic_thresholds() {
        let settings = Settings::parse(
            "[rocket]
             reach_x2_ppm = 400000
             reach_x5_ppm = 150000
             reach_x10_ppm = 90000
             reach_x100_ppm = 100000
             reach_x1000_ppm = 300",
        );
        let state = State::with_settings(&settings);
        let game = state.game();
        let odds = &game.odds;
        for (roll, band) in [
            (0, 100_000..=100_000),
            (299, 100_000..=100_000),
            (300, 1_000..=99_999),
            (89_999, 1_000..=99_999),
            (90_000, 500..=999),
            (149_999, 500..=999),
            (150_000, 200..=499),
            (399_999, 200..=499),
            (400_000, 100..=199),
            (999_999, 100..=199),
        ] {
            assert_eq!(odds.band(roll).0, band);
            assert!(band.contains(&odds.crash_at(roll)));
        }
    }

    #[test]
    fn default_reach_odds_interpolate_in_reciprocal_multiplier_space() {
        let odds = ReachOdds::load(&Settings::default());
        for (reach, multiplier) in [
            (656_667, 150),
            (277_143, 350),
            (129_333, 750),
            (19_400, 5_000),
            (1_940, 50_000),
        ] {
            assert!(odds.crash_at(reach - 1) >= multiplier);
            assert!(odds.crash_at(reach) < multiplier);
        }
        for (multiplier, _, reach) in REACH_TIERS {
            assert!(odds.crash_at(reach - 1) >= multiplier);
            assert!(odds.crash_at(reach) < multiplier);
        }
    }

    #[test]
    fn invalid_or_zero_odds_keep_a_chance_of_reaching_the_cap() {
        let settings = Settings::parse(
            "[rocket]
             reach_x2_ppm = 0
             reach_x1000_ppm = -1",
        );
        let odds = ReachOdds::load(&settings);
        assert_eq!(odds.crash_at(0), CRASH_CAP);
        assert_eq!(odds.band(1).0, 100..=199);
    }

    #[test]
    fn checked_in_reach_odds_match_the_defaults() {
        let settings = Settings::parse(include_str!("../../config/settings.ini"));
        for (_, key, default) in REACH_TIERS {
            assert_eq!(settings.int("rocket", key, -1), i64::from(default), "{key}");
        }
    }

    #[test]
    fn grace_and_growth_match_the_ui_curve() {
        for (ms, multiplier) in [
            (0, 100),
            (1_199, 100),
            (1_200, 100),
            (1_501, 100),
            (1_502, 101),
            (5_905, 149),
            (5_906, 150),
            (8_199, 199),
            (8_200, 200),
            (18_233, 999),
            (18_234, 1_000),
            (39_635, 99_980),
            (39_636, 100_000),
        ] {
            assert_eq!(
                multiplier_at(Duration::from_millis(ms)),
                multiplier,
                "at {ms} ms"
            );
        }
        for multiplier in 101..=100_000 {
            let at = flight_time(multiplier);
            assert!(multiplier_at(at) >= multiplier, "at {multiplier}");
            assert!(
                multiplier_at(at - Duration::from_nanos(1)) < multiplier,
                "before {multiplier}"
            );
        }
    }

    #[test]
    fn history_keeps_only_the_sixteen_most_recent_results() {
        let data = test_data();
        let launch = Instant::now() + ROUND_PERIOD;
        let id = set_round(&data, launch, 100);
        let mut game = data.rocket.game();
        for offset in 0..20 {
            game.round.crash_at = 100 + offset;
            let end = game.round.end();
            game.advance(&data, end);
            game.advance(&data, end + CRASH_HOLD);
        }
        let history: Vec<_> = (4..20)
            .rev()
            .map(|offset| ((id + offset).to_string(), 100 + offset))
            .collect();
        let response = game
            .response(&data, PILOT, game.round.launch, None)
            .unwrap();
        let entries = response["history"].as_array().unwrap();
        assert_eq!(entries.len(), history.len());
        for (entry, (id, multiplier)) in entries.iter().zip(history) {
            assert_eq!(entry, &json!({ "id": id, "multiplier": multiplier }));
        }
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
        let id = set_round(&data, launch, 100);
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
        set_round(&data, Instant::now() + Duration::from_millis(50), 300);
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
        assert_eq!(parse_auto("10"), Some(1_000));
        assert_eq!(parse_auto("1000.00"), Some(100_000));
        for invalid in ["1", "1000.01", "1.001", "+2", "2e0", "NaN", ""] {
            assert_eq!(parse_auto(invalid), None);
        }
    }
}
