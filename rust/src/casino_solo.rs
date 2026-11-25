//! Одиночные игры Activity. Монеты хранятся в общем серверном кошельке.

use crate::{
    db::{Database, Wallet},
    economy::{parse_bet, settle_bet_on, CoinSide, MAX_BET},
    text::money_text,
    wagers, Data, Error,
};
use rand::{seq::SliceRandom, Rng};
use rusqlite::{params, types::Type, OptionalExtension, Transaction};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant},
};

const CELLS: u8 = 25;
const MINES_IDLE: Duration = Duration::from_secs(120);
const MINES_RESULT_RETENTION: Duration = Duration::from_secs(12 * 3600);
const MINES_EDGE_BPS: u64 = 100;
const STALE_ROUND: &str = "Раунд уже изменился. Проверь текущее состояние игры.";
type Player = (u64, u64);
type Reply = Result<Value, String>;

#[derive(Default)]
pub struct State {
    mines: Mutex<HashMap<Player, MinesRound>>,
    initial_revision: OnceLock<u64>,
    receipts_prepared: OnceLock<()>,
}

impl State {
    fn mines(&self) -> MutexGuard<'_, HashMap<Player, MinesRound>> {
        self.mines.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn prepare_receipts(&self, db: &Database) -> rusqlite::Result<()> {
        if self.receipts_prepared.get().is_none() {
            db.lock().execute_batch(
                "CREATE TABLE IF NOT EXISTS miniapp_slots_results (
                     guild_id INTEGER NOT NULL,
                     user_id INTEGER NOT NULL,
                     receipt TEXT NOT NULL,
                     PRIMARY KEY (guild_id, user_id)
                 );
                 CREATE TABLE IF NOT EXISTS miniapp_coinflip_results (
                     guild_id INTEGER NOT NULL,
                     user_id INTEGER NOT NULL,
                     receipt TEXT NOT NULL,
                     PRIMARY KEY (guild_id, user_id)
                 );",
            )?;
            let _ = self.receipts_prepared.set(());
        }
        Ok(())
    }
}

// Activity uses the negative SQLite ID space, away from Discord interaction IDs.
fn activity_id() -> u64 {
    rand::random::<u64>() | (1 << 63)
}

fn available_wager_id(tx: &Transaction<'_>, previous: Option<u64>) -> rusqlite::Result<u64> {
    loop {
        let id = activity_id();
        if previous != Some(id)
            && !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM game_wagers WHERE round_id = ?1 OR ticket = ?1)",
                [id as i64],
                |row| row.get::<_, bool>(0),
            )?
        {
            return Ok(id);
        }
    }
}

fn round_id(value: &Value) -> Option<u64> {
    let raw = value.as_str()?;
    let id = raw.parse::<u64>().ok()?;
    (id != 0 && id.to_string() == raw).then_some(id)
}

pub fn handle(
    data: &Data,
    guild: u64,
    user: u64,
    name: &str,
    game: &str,
    action: &str,
    body: &Value,
) -> Result<Reply, Error> {
    let wallet = Wallet {
        guild,
        user,
        name,
        start: data.economy.start_balance,
    };
    let minimum = data.economy.min_bet.max(1);
    match game {
        "mines" => Ok(handle_mines(data, wallet, minimum, action, body)?),
        "slots" => Ok(handle_slots(data, wallet, minimum, action, body)?),
        "coinflip" => Ok(handle_coinflip(data, wallet, minimum, action, body)?),
        _ => Ok(Err("Неизвестная игра.".into())),
    }
}

#[derive(Clone, Copy)]
enum MinesResult {
    Lost { hit: u8 },
    Paid { payout: i64, timed_out: bool },
    Refunded,
}

struct MinesRound {
    id: u64,
    stake: i64,
    mine_count: u8,
    mines: u32,
    revealed: u32,
    expires_at: Instant,
    result: Option<MinesResult>,
}

fn choose(n: u8, k: u8) -> u64 {
    (1..=k).fold(1, |total, i| total * u64::from(n + 1 - i) / u64::from(i))
}

fn mines_multiplier(mine_count: u8, revealed: u32) -> i64 {
    let count = revealed.count_ones() as u8;
    if count == 0 {
        return 100;
    }
    // Survival after k picks is C(25 - mines, k) / C(25, k).
    // Use 99% of the fair multiplier, rounded down to hundredths.
    ((10_000 - MINES_EDGE_BPS) * choose(CELLS, count) / (100 * choose(CELLS - mine_count, count)))
        as i64
}

fn hidden_mines(count: u8) -> u32 {
    let mut cells: [u8; CELLS as usize] = std::array::from_fn(|i| i as u8);
    cells.shuffle(&mut rand::rng());
    cells[..usize::from(count)]
        .iter()
        .fold(0, |bits, &cell| bits | (1 << cell))
}

impl MinesRound {
    fn payout(&self, revealed: u32) -> i64 {
        self.stake * mines_multiplier(self.mine_count, revealed) / 100
    }

    fn finish(&mut self, db: &Database, result: MinesResult) -> rusqlite::Result<()> {
        let payout = match result {
            MinesResult::Lost { .. } => 0,
            MinesResult::Paid { payout, .. } => payout,
            MinesResult::Refunded => self.stake,
        };
        wagers::settle(db, self.id, &[(self.id, payout)])?;
        self.result = Some(result);
        Ok(())
    }

    fn expire(&mut self, db: &Database, now: Instant) -> rusqlite::Result<()> {
        if self.result.is_none() && now >= self.expires_at {
            let result = if self.revealed == 0 {
                MinesResult::Refunded
            } else {
                MinesResult::Paid {
                    payout: self.payout(self.revealed),
                    timed_out: true,
                }
            };
            match self.finish(db, result) {
                // A wallet that grew while the round was open may only have room for the stake.
                Err(rusqlite::Error::IntegralValueOutOfRange(..)) if self.revealed != 0 => {
                    self.finish(db, MinesResult::Refunded)?;
                }
                result => result?,
            }
        }
        Ok(())
    }

    fn snapshot(&self, now: Instant) -> Value {
        let (status, payout, timed_out) = match self.result {
            None => ("active", None, false),
            Some(MinesResult::Lost { .. }) => ("lost", Some(0), false),
            Some(MinesResult::Paid { payout, timed_out }) => ("paid", Some(payout), timed_out),
            Some(MinesResult::Refunded) => ("refunded", Some(self.stake), true),
        };
        let cells: [&str; CELLS as usize] = std::array::from_fn(|index| {
            let bit = 1 << index;
            if matches!(self.result, Some(MinesResult::Lost { hit }) if usize::from(hit) == index) {
                "hit"
            } else if self.revealed & bit != 0 {
                "safe"
            } else if self.result.is_some() && self.mines & bit != 0 {
                "mine"
            } else {
                "hidden"
            }
        });
        json!({
            "id": self.id.to_string(),
            "status": status,
            "stake": self.stake,
            "mine_count": self.mine_count,
            "safe_count": self.revealed.count_ones(),
            "cells": cells,
            "multiplier": mines_multiplier(self.mine_count, self.revealed),
            "cashout": (self.result.is_none() && self.revealed != 0).then(|| self.payout(self.revealed)),
            "payout": payout,
            "timed_out": timed_out,
            "remaining_ms": self.result.is_none().then(|| self.expires_at.saturating_duration_since(now).as_millis() as u64),
        })
    }
}

fn handle_mines(
    data: &Data,
    wallet: Wallet<'_>,
    minimum: i64,
    action: &str,
    body: &Value,
) -> rusqlite::Result<Reply> {
    let player = (wallet.guild, wallet.user);
    let mut rounds = data.casino_solo.mines();
    let now = Instant::now();
    if let Some(round) = rounds.get_mut(&player) {
        round.expire(&data.db, now)?;
    }
    let initial_revision = *data.casino_solo.initial_revision.get_or_init(activity_id);
    let revision = rounds
        .get(&player)
        .map_or(initial_revision, |round| round.id);
    match action {
        "state" => {}
        "start" => {
            if body.get("expected_round").and_then(round_id) != Some(revision) {
                return Ok(Err(STALE_ROUND.into()));
            }
            if rounds
                .get(&player)
                .is_some_and(|round| round.result.is_none())
            {
                return Ok(Err("У тебя уже есть открытое поле.".into()));
            }
            let Some(mine_count) = body["mine_count"]
                .as_u64()
                .filter(|count| (1..25).contains(count))
            else {
                return Ok(Err("Выбери от 1 до 24 мин.".into()));
            };
            let Some(bet) = body["bet"].as_str() else {
                return Ok(Err("Укажи ставку в монетах.".into()));
            };
            // rand::rng is the OS-seeded cryptographic ThreadRng. Keep the permutation on the server.
            let mines = hidden_mines(mine_count as u8);
            let placed = data.db.transaction(|tx| {
                wallet.open(tx)?;
                let id = available_wager_id(tx, Some(revision))?;
                Ok(
                    wagers::place_on(tx, id, id, wallet.guild, wallet.user, bet, minimum)?
                        .map(|stake| (id, stake)),
                )
            })?;
            let (id, stake) = match placed {
                Ok(placed) => placed,
                Err(error) => return Ok(Err(error)),
            };
            rounds.insert(
                player,
                MinesRound {
                    id,
                    stake,
                    mine_count: mine_count as u8,
                    mines,
                    revealed: 0,
                    expires_at: Instant::now() + MINES_IDLE,
                    result: None,
                },
            );
        }
        "reveal" | "cashout" => {
            let expected = body.get("round_id").and_then(round_id);
            let Some(round) = rounds
                .get_mut(&player)
                .filter(|round| expected == Some(round.id))
            else {
                return Ok(Err(STALE_ROUND.into()));
            };
            if round.result.is_some() {
                return Ok(Err("Этот раунд завершён.".into()));
            }
            if action == "cashout" {
                if round.revealed == 0 {
                    return Ok(Err("Сначала открой хотя бы одну безопасную клетку.".into()));
                }
                round.finish(
                    &data.db,
                    MinesResult::Paid {
                        payout: round.payout(round.revealed),
                        timed_out: false,
                    },
                )?;
            } else {
                let Some(cell) = body["cell"]
                    .as_u64()
                    .filter(|cell| *cell < u64::from(CELLS))
                else {
                    return Ok(Err("Выбери клетку на поле.".into()));
                };
                let bit = 1 << cell;
                if round.revealed & bit != 0 {
                    return Ok(Err("Эта клетка уже открыта.".into()));
                }
                if round.mines & bit != 0 {
                    round.finish(&data.db, MinesResult::Lost { hit: cell as u8 })?;
                } else {
                    let revealed = round.revealed | bit;
                    if revealed.count_ones() == u32::from(CELLS - round.mine_count) {
                        round.finish(
                            &data.db,
                            MinesResult::Paid {
                                payout: round.payout(revealed),
                                timed_out: false,
                            },
                        )?;
                    }
                    round.revealed = revealed;
                    round.expires_at = Instant::now() + MINES_IDLE;
                }
            }
        }
        _ => return Ok(Err("Неизвестное действие.".into())),
    }
    let round = rounds.get(&player);
    let (balance, _) = wallet.open(&data.db.lock())?;
    Ok(Ok(json!({
        "balance": balance,
        "min_bet": minimum,
        "max_bet": MAX_BET,
        "revision": round.map_or(initial_revision, |round| round.id).to_string(),
        "idle_seconds": MINES_IDLE.as_secs(),
        "house_edge_bps": MINES_EDGE_BPS,
        "round": round.map(|round| round.snapshot(Instant::now())),
    })))
}

/// Open stakes are also covered by the shared startup recovery in wagers::refund.
pub fn sweep(data: &Data) {
    let mut rounds = data.casino_solo.mines();
    let now = Instant::now();
    rounds.retain(|_, round| {
        if let Err(error) = round.expire(&data.db, now) {
            eprintln!("Мины {}: не удалось завершить раунд: {error}", round.id);
        }
        round.result.is_none() || now.saturating_duration_since(round.expires_at) < MINES_RESULT_RETENTION
    });
}

// With six equally likely symbols and five paylines, these multipliers return 100.63% on average.
const SLOT_SYMBOLS: [(&str, &str, [i64; 3]); 6] = [
    ("🍒", "Вишня", [2, 5, 15]),
    ("🍋", "Лимон", [3, 7, 25]),
    ("🍇", "Виноград", [4, 9, 35]),
    ("🥝", "Киви", [6, 13, 50]),
    ("portrait", "Даня", [7, 17, 75]),
    ("💎", "Алмаз", [7, 24, 120]),
];
const SLOT_PAYLINES: [(&str, [usize; 5]); 5] = [
    ("Верхняя", [0; 5]),
    ("Средняя", [1; 5]),
    ("Нижняя", [2; 5]),
    ("V-образная", [0, 1, 2, 1, 0]),
    ("Перевёрнутая V", [2, 1, 0, 1, 2]),
];
type SlotGrid = [[usize; 5]; 3];

#[derive(Debug, PartialEq, Eq)]
struct SlotWin {
    line: usize,
    symbol: usize,
    count: usize,
    multiplier: i64,
}

impl SlotWin {
    fn snapshot(&self) -> Value {
        json!({
            "line": self.line,
            "symbol": self.symbol,
            "count": self.count,
            "multiplier": self.multiplier,
        })
    }
}

fn slot_wins(grid: &SlotGrid) -> Vec<SlotWin> {
    SLOT_PAYLINES
        .iter()
        .enumerate()
        .filter_map(|(line, (_, rows))| {
            let symbol = grid[rows[0]][0];
            let count = rows
                .iter()
                .enumerate()
                .take_while(|&(column, &row)| grid[row][column] == symbol)
                .count();
            let multiplier = SLOT_SYMBOLS[symbol].2.get(count.checked_sub(3)?).copied()?;
            Some(SlotWin {
                line,
                symbol,
                count,
                multiplier,
            })
        })
        .collect()
}

fn load_receipt(tx: &Transaction<'_>, query: &str, wallet: &Wallet<'_>) -> rusqlite::Result<Option<Value>> {
    tx.query_row(query, params![wallet.guild as i64, wallet.user as i64], |row| {
        let raw: String = row.get(0)?;
        serde_json::from_str(&raw).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
        })
    }).optional()
}

fn is_current_round(previous: Option<u64>, expected: Option<&Value>) -> bool {
    match expected {
        Some(Value::Null) => previous.is_none(),
        Some(value) => round_id(value).is_some_and(|id| previous == Some(id)),
        None => false,
    }
}

fn handle_coinflip(
    data: &Data,
    wallet: Wallet<'_>,
    minimum: i64,
    action: &str,
    body: &Value,
) -> rusqlite::Result<Reply> {
    if !matches!(action, "state" | "flip") {
        return Ok(Err("Неизвестное действие.".into()));
    }
    data.casino_solo.prepare_receipts(&data.db)?;
    data.db.transaction(|tx| {
        let mut flip = load_receipt(tx,
            "SELECT receipt FROM miniapp_coinflip_results WHERE guild_id = ?1 AND user_id = ?2", &wallet)?;
        let balance;
        if action == "flip" {
            let previous = flip.as_ref().and_then(|receipt| round_id(&receipt["id"]));
            if !is_current_round(previous, body.get("expected_round")) {
                return Ok(Err(STALE_ROUND.into()));
            }
            let Some(side) = body["side"].as_str().and_then(CoinSide::from_key) else {
                return Ok(Err("Выбери орла или решку.".into()));
            };
            let Some(bet) = body["bet"].as_str() else {
                return Ok(Err("Укажи ставку в монетах.".into()));
            };
            let result = CoinSide::roll();
            let multiplier = side.payout_multiplier(result);
            let (stake, new_balance) = match settle_bet_on(tx, wallet, bet, minimum, multiplier)? {
                Ok(result) => result,
                Err(error) => return Ok(Err(error)),
            };
            let id = available_wager_id(tx, previous)?;
            let receipt = json!({
                "id": id.to_string(), "side": side.key(), "result": result.key(),
                "stake": stake, "payout": stake * multiplier,
            });
            tx.execute(
                "INSERT INTO miniapp_coinflip_results (guild_id, user_id, receipt) VALUES (?1, ?2, ?3)
                 ON CONFLICT (guild_id, user_id) DO UPDATE SET receipt = excluded.receipt",
                params![wallet.guild as i64, wallet.user as i64, receipt.to_string()],
            )?;
            flip = Some(receipt);
            balance = new_balance;
        } else {
            (balance, _) = wallet.open(tx)?;
        }
        Ok(Ok(json!({"balance": balance, "min_bet": minimum, "max_bet": MAX_BET, "flip": flip})))
    })
}

fn handle_slots(
    data: &Data,
    wallet: Wallet<'_>,
    minimum: i64,
    action: &str,
    body: &Value,
) -> rusqlite::Result<Reply> {
    if !matches!(action, "state" | "spin") {
        return Ok(Err("Неизвестное действие.".into()));
    }
    data.casino_solo.prepare_receipts(&data.db)?;
    data.db.transaction(|tx| {
        let mut spin = load_receipt(tx,
            "SELECT receipt FROM miniapp_slots_results WHERE guild_id = ?1 AND user_id = ?2", &wallet)?;
        let (mut balance, _) = wallet.open(tx)?;
        if action == "spin" {
            // The previous receipt acts as a compare-and-swap token, including after process restart.
            let previous = spin.as_ref().and_then(|receipt| round_id(&receipt["id"]));
            if !is_current_round(previous, body.get("expected_round")) {
                return Ok(Err(STALE_ROUND.into()));
            }
            let Some(bet) = body["bet"].as_str() else {
                return Ok(Err("Укажи ставку в монетах.".into()));
            };
            let count = match body.get("boards") {
                None => 1,
                Some(value) => match value.as_u64() {
                    Some(count @ 1..=3) => count as usize,
                    _ => return Ok(Err("Выбери от одного до трёх наборов барабанов.".into())),
                },
            };
            let stake = match parse_bet(bet, balance / count as i64, minimum.max(1)) {
                Ok(stake) => stake,
                Err(error) => return Ok(Err(error)),
            };
            let total_stake = stake * count as i64;
            if total_stake > balance {
                return Ok(Err(format!(
                    "Для ставки нужно {}, у тебя {}.",
                    money_text(total_stake),
                    money_text(balance)
                )));
            }

            let id = available_wager_id(tx, previous)?;
            let bet = stake.to_string();
            let mut rng = rand::rng();
            let mut boards = Vec::with_capacity(count);
            let mut payouts = Vec::with_capacity(count);
            for index in 0..count {
                let ticket = if index == 0 { id } else { available_wager_id(tx, previous)? };
                // The full stake was checked before placing any tickets. A failed placement rolls back the round.
                wagers::place_on(tx, id, ticket, wallet.guild, wallet.user, &bet, minimum)?
                    .map_err(|_| rusqlite::Error::InvalidQuery)?;
                let grid: SlotGrid = std::array::from_fn(|_| {
                    std::array::from_fn(|_| rng.random_range(0..SLOT_SYMBOLS.len()))
                });
                let wins = slot_wins(&grid);
                let payout = stake * wins.iter().map(|win| win.multiplier).sum::<i64>();
                boards.push(json!({
                    "grid": grid,
                    "wins": wins.iter().map(SlotWin::snapshot).collect::<Vec<_>>(),
                    "stake": stake,
                    "payout": payout,
                }));
                payouts.push((ticket, payout));
            }
            let total_payout = payouts.iter().map(|&(_, payout)| payout).sum::<i64>();
            wagers::settle_on(tx, id, &payouts)?;
            let mut receipt = if count == 1 {
                boards.remove(0)
            } else {
                json!({"boards": boards, "stake": total_stake, "payout": total_payout})
            };
            receipt["id"] = json!(id.to_string());
            tx.execute(
                "INSERT INTO miniapp_slots_results (guild_id, user_id, receipt) VALUES (?1, ?2, ?3)
                 ON CONFLICT (guild_id, user_id) DO UPDATE SET receipt = excluded.receipt",
                params![wallet.guild as i64, wallet.user as i64, receipt.to_string()],
            )?;
            spin = Some(receipt);
            balance = balance - total_stake + total_payout;
        }
        let symbols: Vec<_> = SLOT_SYMBOLS
            .iter()
            .map(|(icon, name, payouts)| json!({ "icon": icon, "name": name, "payouts": payouts }))
            .collect();
        let paylines: Vec<_> = SLOT_PAYLINES
            .iter()
            .map(|(name, rows)| json!({ "name": name, "rows": rows }))
            .collect();
        Ok(Ok(json!({
            "balance": balance,
            "min_bet": minimum,
            "max_bet": MAX_BET,
            "symbols": symbols,
            "paylines": paylines,
            "spin": spin,
        })))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUILD: u64 = 10_000_000_000_000_001;
    const USER: u64 = 10_000_000_000_000_002;

    fn data() -> Data {
        Data::new(
            Database::in_memory().unwrap(),
            &crate::config::Settings::default(),
            Default::default(),
            false,
        )
    }

    fn call(data: &Data, game: &str, action: &str, fields: Value) -> Result<Value, String> {
        handle(data, GUILD, USER, "<player>", game, action, &fields).unwrap()
    }

    fn start(data: &Data, count: u8) -> Value {
        let initial = call(data, "mines", "state", json!({})).unwrap();
        call(
            data,
            "mines",
            "start",
            json!({"expected_round": initial["revision"], "bet": "100", "mine_count": count}),
        )
        .unwrap()
    }

    #[test]
    fn mines_hides_layout_and_guards_round_and_cell() {
        let data = data();
        let start = start(&data, 1);
        assert_eq!(start["balance"], 400);
        assert!(start["round"]["id"].is_string());
        assert!(start["round"].get("mines").is_none());
        assert_eq!(start["round"]["cells"], json!(vec!["hidden"; 25]));
        let id = &start["round"]["id"];
        assert!(call(&data, "mines", "reveal", json!({"round_id": 1, "cell": 0})).is_err());
        assert!(call(
            &data,
            "mines",
            "reveal",
            json!({"round_id": id, "cell": 25})
        )
        .is_err());
        assert!(call(&data, "mines", "cashout", json!({"round_id": id})).is_err());
        assert!(handle(
            &data,
            GUILD,
            USER + 1,
            "other",
            "mines",
            "reveal",
            &json!({
                "round_id": id, "cell": 1
            })
        )
        .unwrap()
        .is_err());

        data.casino_solo
            .mines()
            .get_mut(&(GUILD, USER))
            .unwrap()
            .mines = 1;
        let safe = call(&data, "mines", "reveal", json!({"round_id": id, "cell": 1})).unwrap();
        assert_eq!(safe["round"]["cells"][1], "safe");
        assert_eq!(safe["round"]["cells"][0], "hidden");
        assert_eq!(safe["round"]["multiplier"], 103);
        assert!(call(&data, "mines", "reveal", json!({"round_id": id, "cell": 1})).is_err());
        let lost = call(&data, "mines", "reveal", json!({"round_id": id, "cell": 0})).unwrap();
        assert_eq!(lost["round"]["status"], "lost");
        assert_eq!(lost["round"]["cells"][0], "hit");
        assert_eq!(lost["round"]["payout"], 0);
        assert_eq!(lost["balance"], 400);
        assert!(call(&data, "mines", "cashout", json!({"round_id": id})).is_err());
    }

    #[test]
    fn mines_sweep_settles_expired_wagers_once() {
        for (reveal, status, balance) in [(false, "refunded", 500), (true, "paid", 503)] {
            let data = data();
            let state = start(&data, 1);
            let id = &state["round"]["id"];
            data.casino_solo
                .mines()
                .get_mut(&(GUILD, USER))
                .unwrap()
                .mines = 1;
            if reveal {
                call(&data, "mines", "reveal", json!({"round_id": id, "cell": 1})).unwrap();
            }
            data.casino_solo
                .mines()
                .get_mut(&(GUILD, USER))
                .unwrap()
                .expires_at = Instant::now();
            sweep(&data);
            sweep(&data);
            let expired = call(&data, "mines", "state", json!({})).unwrap();
            assert_eq!(expired["round"]["status"], status);
            assert_eq!(expired["round"]["timed_out"], true);
            assert_eq!(expired["balance"], balance);
            assert_eq!(wagers::refund(&data.db, None).unwrap(), 0);
        }
    }

    #[test]
    fn mines_last_safe_cell_auto_pays_and_stale_round_cannot_replay() {
        let data = data();
        let state = start(&data, 24);
        let id = &state["round"]["id"];
        data.casino_solo
            .mines()
            .get_mut(&(GUILD, USER))
            .unwrap()
            .mines = (1 << 24) - 1;
        let won = call(
            &data,
            "mines",
            "reveal",
            json!({"round_id": id, "cell": 24}),
        )
        .unwrap();
        assert_eq!(won["round"]["status"], "paid");
        assert_eq!(won["round"]["payout"], 2475);
        assert_eq!(won["balance"], 2875);
        let next = start(&data, 1);
        assert_ne!(&next["round"]["id"], id);
        assert!(call(
            &data,
            "mines",
            "reveal",
            json!({"round_id": id, "cell": 24})
        )
        .is_err());
        assert!(call(
            &data,
            "mines",
            "start",
            json!({
                "expected_round": id, "bet": "100", "mine_count": 1
            })
        )
        .is_err());
    }

    #[test]
    fn mines_uses_wager_startup_recovery() {
        let data = data();
        assert_eq!(start(&data, 4)["balance"], 400);
        assert_eq!(wagers::refund(&data.db, None).unwrap(), 1);
        assert_eq!(wagers::refund(&data.db, None).unwrap(), 0);
        assert_eq!(data.db.wallet(GUILD, USER, "<player>", 500).unwrap().0, 500);
    }

    #[test]
    fn mines_manual_cashout_pays_the_current_multiplier_once() {
        let data = data();
        let state = start(&data, 1);
        let id = &state["round"]["id"];
        data.casino_solo
            .mines()
            .get_mut(&(GUILD, USER))
            .unwrap()
            .mines = 1;
        call(&data, "mines", "reveal", json!({"round_id": id, "cell": 1})).unwrap();
        call(&data, "mines", "reveal", json!({"round_id": id, "cell": 2})).unwrap();
        let cashed = call(&data, "mines", "cashout", json!({"round_id": id})).unwrap();
        assert_eq!(cashed["round"]["status"], "paid");
        assert_eq!(cashed["round"]["multiplier"], 107);
        assert_eq!(cashed["round"]["payout"], 107);
        assert_eq!(cashed["balance"], 507);
        assert!(call(&data, "mines", "cashout", json!({"round_id": id})).is_err());
        assert_eq!(call(&data, "mines", "state", json!({})).unwrap(), cashed);
    }

    #[test]
    fn slots_uses_left_runs_on_each_line_and_totals_multiple_wins() {
        let mut grid = [[0; 5]; 3];
        let wins = slot_wins(&grid);
        assert_eq!(wins.iter().map(|win| win.multiplier).sum::<i64>(), 75);
        assert_eq!(wins.len(), 5);
        grid = [[5, 5, 5, 5, 4], [4, 5, 5, 5, 5], [2, 1, 0, 1, 2]];
        let wins = slot_wins(&grid);
        assert_eq!(
            wins,
            vec![SlotWin {
                line: 0,
                symbol: 5,
                count: 4,
                multiplier: 24
            }]
        );
    }

    #[test]
    fn slots_paytable_raises_expected_return_by_about_five_percent() {
        // For a given symbol on a five-cell line, there are 30, 5, and 1 ways to end a run of 3, 4, or 5.
        let weighted_payout: i64 = SLOT_SYMBOLS
            .iter()
            .map(|(_, _, [three, four, five])| 30 * three + 5 * four + five)
            .sum();
        assert_eq!(weighted_payout, 1_565);
        assert_eq!(SLOT_PAYLINES.len(), 5);
    }

    #[test]
    fn slots_persists_receipt_and_rejects_replayed_initial_spin() {
        let mut data = data();
        let body = json!({"bet": "100", "expected_round": null});
        let first = call(&data, "slots", "spin", body.clone()).unwrap();
        let payout = first["spin"]["payout"].as_i64().unwrap();
        assert_eq!(first["balance"], 400 + payout);
        assert!(first["spin"]["id"].as_str().is_some());
        data.casino_solo = State::default();
        assert!(call(&data, "slots", "spin", body).is_err());
        assert_eq!(call(&data, "slots", "state", json!({})).unwrap(), first);
        assert_eq!(wagers::refund(&data.db, None).unwrap(), 0);
    }

    #[test]
    fn slots_charges_and_records_all_boards_as_one_round() {
        let data = data();
        let fields = json!({"bet": "100", "boards": 3, "expected_round": null});
        let state = call(&data, "slots", "spin", fields.clone()).unwrap();
        let spin = &state["spin"];
        let boards = spin["boards"].as_array().unwrap();
        assert_eq!(boards.len(), 3);
        for board in boards {
            assert_eq!(board["stake"], 100);
            let grid: SlotGrid = serde_json::from_value(board["grid"].clone()).unwrap();
            let wins = slot_wins(&grid);
            assert_eq!(board["wins"], json!(wins.iter().map(SlotWin::snapshot).collect::<Vec<_>>()));
            assert_eq!(board["payout"], 100 * wins.iter().map(|win| win.multiplier).sum::<i64>());
        }
        let payout: i64 = boards.iter().map(|board| board["payout"].as_i64().unwrap()).sum();
        assert_eq!(spin["stake"], 300);
        assert_eq!(spin["payout"], payout);
        assert_eq!(state["balance"], 200 + payout);
        assert!(call(&data, "slots", "spin", fields).is_err());
        assert_eq!(call(&data, "slots", "state", json!({})).unwrap(), state);
        assert_eq!(wagers::refund(&data.db, None).unwrap(), 0);
    }

    #[test]
    fn slots_rejects_invalid_counts_and_requires_funds_for_every_board() {
        let data = data();
        for count in [json!(0), json!(4), json!(2.5), json!("2"), json!(null)] {
            let fields = json!({"bet": "100", "boards": count, "expected_round": null});
            assert!(call(&data, "slots", "spin", fields).is_err());
        }
        let fields = json!({"bet": "200", "boards": 3, "expected_round": null});
        assert!(call(&data, "slots", "spin", fields).is_err());
        let state = call(&data, "slots", "state", json!({})).unwrap();
        assert_eq!(state["balance"], 500);
        assert!(state["spin"].is_null());
        assert_eq!(wagers::refund(&data.db, None).unwrap(), 0);
    }

    #[test]
    fn slots_accepts_only_one_concurrent_spin_from_the_same_receipt() {
        let data = data();
        let previous = call(
            &data,
            "slots",
            "spin",
            json!({"bet": "100", "expected_round": null}),
        )
        .unwrap();
        let fields = json!({"bet": "100", "boards": 2, "expected_round": previous["spin"]["id"]});
        let replies = std::thread::scope(|scope| {
            let first = scope.spawn(|| call(&data, "slots", "spin", fields.clone()));
            let second = scope.spawn(|| call(&data, "slots", "spin", fields.clone()));
            [first.join().unwrap(), second.join().unwrap()]
        });
        assert_eq!(replies.iter().filter(|reply| reply.is_ok()).count(), 1);
        let latest = call(&data, "slots", "state", json!({})).unwrap();
        assert_eq!(
            latest,
            *replies
                .iter()
                .find_map(|reply| reply.as_ref().ok())
                .unwrap()
        );
        assert_eq!(
            latest["balance"].as_i64().unwrap(),
            previous["balance"].as_i64().unwrap() - 200
                + latest["spin"]["payout"].as_i64().unwrap()
        );
    }

    #[test]
    fn slots_rolls_back_debit_and_payout_when_receipt_cannot_be_saved() {
        let data = data();
        call(&data, "slots", "state", json!({})).unwrap();
        data.db
            .lock()
            .execute_batch(
                "CREATE TRIGGER reject_receipt BEFORE INSERT ON miniapp_slots_results
             BEGIN SELECT RAISE(ABORT, 'test write failure'); END;",
            )
            .unwrap();
        assert!(handle(
            &data,
            GUILD,
            USER,
            "<player>",
            "slots",
            "spin",
            &json!({"bet": "100", "boards": 3, "expected_round": null})
        )
        .is_err());
        let state = call(&data, "slots", "state", json!({})).unwrap();
        assert_eq!(state["balance"], 500);
        assert!(state["spin"].is_null());
        assert_eq!(wagers::refund(&data.db, None).unwrap(), 0);
    }

    #[test]
    fn coinflip_saves_the_result_and_charges_a_receipt_only_once() {
        let mut data = data();
        let initial = call(&data, "coinflip", "state", json!({})).unwrap();
        assert_eq!(initial["balance"], 500);
        assert!(initial["flip"].is_null());
        let fields = json!({"bet": "100", "side": "heads", "expected_round": null});
        let played = call(&data, "coinflip", "flip", fields.clone()).unwrap();
        let flip = &played["flip"];
        assert_eq!(flip["side"], "heads");
        assert_eq!(flip["stake"], 100);
        let head = flip["result"] == "heads";
        assert!(head || flip["result"] == "tails");
        assert_eq!(flip["payout"], if head { 200 } else { 0 });
        assert_eq!(played["balance"], if head { 600 } else { 400 });

        data.casino_solo = State::default();
        assert!(call(&data, "coinflip", "flip", fields).is_err());
        assert!(call(&data, "coinflip", "flip", json!({
            "bet": "100", "side": "edge", "expected_round": flip["id"],
        })).is_err());
        assert_eq!(call(&data, "coinflip", "state", json!({})).unwrap(), played);
    }
}
