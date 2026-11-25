//! Европейская рулетка с одним общим раундом на сервер.

use crate::{
    db::{Database, Wallet},
    economy::MAX_BET,
    wagers, Data,
};
use poise::serenity_prelude as s;
use rand::Rng;
use rusqlite::params;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(15);
const MAX_ROUND: Duration = Duration::from_secs(60);
const MIN_BET: i64 = 10;
const MAX_BETS: usize = 50;
const HISTORY_LIMIT: i64 = 10;
const HELP_BUTTON: &str = "roulette:help";
const RED: [u8; 18] = [
    1, 3, 5, 7, 9, 12, 14, 16, 18, 19, 21, 23, 25, 27, 30, 32, 34, 36,
];
const GROUPS: &[(&str, &str, Space)] = &[
    ("red", "Красное", Space::Color(true)),
    ("black", "Чёрное", Space::Color(false)),
    ("even", "Чётное", Space::Parity(true)),
    ("odd", "Нечётное", Space::Parity(false)),
    ("1-18", "1–18", Space::Range(1, 18)),
    ("19-36", "19–36", Space::Range(19, 36)),
    ("1st", "1-я колонка", Space::Column(1)),
    ("2nd", "2-я колонка", Space::Column(2)),
    ("3rd", "3-я колонка", Space::Column(3)),
    ("1-12", "1–12", Space::Range(1, 12)),
    ("13-24", "13–24", Space::Range(13, 24)),
    ("25-36", "25–36", Space::Range(25, 36)),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Space {
    Color(bool),
    Parity(bool),
    Range(u8, u8),
    Column(u8),
    Number(u8),
}

impl Space {
    fn parse(text: &str) -> Option<(Self, String)> {
        let text = text.trim().to_lowercase().replace('–', "-");
        if let Some(&(key, _, space)) = GROUPS
            .iter()
            .find(|(key, label, _)| *key == text || label.to_lowercase().replace('–', "-") == text)
        {
            return Some((space, key.into()));
        }
        let number = text.parse::<u8>().ok()?;
        (number <= 36 && number.to_string() == text).then_some((Self::Number(number), text))
    }

    /// Множитель полной выплаты, включая поставленные монеты.
    fn multiplier(self, number: u8) -> i64 {
        if let Self::Number(wanted) = self {
            return if number == wanted { 36 } else { 0 };
        }
        if number == 0 {
            return 0;
        }
        match self {
            Self::Color(red) if RED.contains(&number) == red => 2,
            Self::Parity(even) if (number % 2 == 0) == even => 2,
            Self::Range(first, last) if (first..=last).contains(&number) => {
                if last - first == 11 {
                    3
                } else {
                    2
                }
            }
            Self::Column(column) if (number - 1) % 3 + 1 == column => 3,
            _ => 0,
        }
    }
}

struct Bet {
    ticket: u64,
    user: u64,
    name: String,
    amount: i64,
    key: String,
    space: Space,
    request_id: String,
}

impl Bet {
    fn payout(&self, number: Option<u8>) -> i64 {
        number.map_or(self.amount, |number| self.amount * self.space.multiplier(number))
    }

    fn public_snapshot(&self, payout: Option<i64>) -> Value {
        let mut row = json!({
            "user_id": self.user.to_string(),
            "name": self.name,
            "amount": self.amount,
            "space": self.key,
        });
        if let Some(payout) = payout {
            row["payout"] = json!(payout);
        }
        row
    }
}

struct Round {
    started: Instant,
    deadline: Instant,
    bets: Vec<Bet>,
    refund_pending: bool,
}

struct Completed {
    id: u64,
    number: Option<u8>,
    bets: Vec<Bet>,
}

struct Table {
    // Shared by clients who saw an idle table and clients who saw it open.
    round_id: u64,
    round: Option<Round>,
    last_result: Option<Completed>,
}

impl Default for Table {
    fn default() -> Self {
        Self {
            round_id: new_id(),
            round: None,
            last_result: None,
        }
    }
}

fn new_id() -> u64 {
    rand::random::<u64>() | (1 << 63)
}

fn read_history(db: &Database, guild: u64) -> rusqlite::Result<Vec<Value>> {
    let connection = db.lock();
    let mut query = connection.prepare(
        "SELECT round_id, number FROM miniapp_roulette_history
         WHERE guild_id = ?1 ORDER BY id DESC LIMIT ?2",
    )?;
    let rows = query.query_map(params![guild as i64, HISTORY_LIMIT], |row| {
        let round_id = row.get::<_, i64>(0)? as u64;
        let number = row.get::<_, u8>(1)?;
        Ok(json!({"id": round_id.to_string(), "number": number}))
    })?;
    rows.collect()
}

fn own_bets(bets: &[Bet], user: u64) -> Vec<Value> {
    bets.iter()
        .filter(|bet| bet.user == user)
        .map(|bet| json!({"request_id": bet.request_id, "space": bet.key, "amount": bet.amount}))
        .collect()
}

impl Completed {
    fn snapshot(&self, user: u64) -> Value {
        let mut totals = BTreeMap::new();
        let mut bets = Vec::with_capacity(self.bets.len());
        for bet in &self.bets {
            let payout = bet.payout(self.number);
            bets.push(bet.public_snapshot(Some(payout)));
            let (name, total) = totals.entry(bet.user).or_insert((bet.name.as_str(), 0));
            *name = &bet.name;
            *total += payout - bet.amount;
        }
        let mut players: Vec<_> = totals.into_iter().collect();
        players.sort_by_key(|&(user, (_, net))| (std::cmp::Reverse(net), user));
        json!({
            "id": self.id.to_string(),
            "number": self.number,
            "refunded": self.number.is_none(),
            "bets": bets,
            "your_bets": own_bets(&self.bets, user),
            "players": players.into_iter().map(|(id, (name, net))| json!({
                "id": id.to_string(), "name": name, "net": net,
            })).collect::<Vec<_>>(),
        })
    }
}

impl Table {
    fn maintain(&mut self, db: &Database, guild: u64, now: Instant) {
        let Some(round) = self.round.as_mut().filter(|round| now >= round.deadline) else {
            return;
        };
        // Choose the result only once betting has closed, under the same lock as placement.
        let mut number = None;
        if !round.refund_pending {
            let drawn = rand::rng().random_range(0..=36);
            let payouts: Vec<_> = round
                .bets
                .iter()
                .map(|bet| (bet.ticket, bet.payout(Some(drawn))))
                .collect();
            let settled = db.transaction(|tx| {
                wagers::settle_on(tx, self.round_id, &payouts)?;
                tx.execute(
                    "INSERT INTO miniapp_roulette_history (guild_id, round_id, number) VALUES (?1, ?2, ?3)",
                    params![guild as i64, self.round_id as i64, drawn],
                )?;
                tx.execute(
                    "DELETE FROM miniapp_roulette_history WHERE id IN (
                         SELECT id FROM miniapp_roulette_history
                         WHERE guild_id = ?1 ORDER BY id DESC LIMIT -1 OFFSET ?2
                     )",
                    params![guild as i64, HISTORY_LIMIT],
                )?;
                Ok(())
            });
            match settled {
                Ok(()) => number = Some(drawn),
                Err(error) => {
                    eprintln!(
                        "Mini App roulette {}: settlement failed: {error}",
                        self.round_id
                    );
                    round.refund_pending = true;
                }
            }
        }
        if round.refund_pending {
            if let Err(error) = wagers::refund(db, Some(self.round_id)) {
                eprintln!(
                    "Mini App roulette {}: refund failed: {error}",
                    self.round_id
                );
                return;
            }
        }
        let round = self.round.take().unwrap();
        self.last_result = Some(Completed {
            id: self.round_id,
            number,
            bets: round.bets,
        });
        self.round_id = new_id();
    }

    fn snapshot(
        &self,
        user: u64,
        balance: i64,
        now: Instant,
        bet_error: Option<String>,
        history: Vec<Value>,
    ) -> Value {
        let round = self.round.as_ref().map(|round| {
            json!({
                "id": self.round_id.to_string(),
                "remaining_ms": round.deadline.saturating_duration_since(now).as_millis() as u64,
                "refunding": round.refund_pending,
                "bet_count": round.bets.len(),
                "player_count": round.bets.iter().map(|bet| bet.user).collect::<HashSet<_>>().len(),
                "bets": round.bets.iter().map(|bet| bet.public_snapshot(None)).collect::<Vec<_>>(),
                "your_bets": own_bets(&round.bets, user),
            })
        });
        json!({
            "user_id": user.to_string(),
            "balance": balance,
            "min_bet": MIN_BET,
            "max_bet": MAX_BET,
            "max_bets": MAX_BETS,
            "round_id": self.round_id.to_string(),
            "round": round,
            "last_result": self.last_result.as_ref().map(|last| last.snapshot(user)),
            "history": history,
            "bet_error": bet_error,
        })
    }

    fn accepted(&self, wager: &Wager<'_>, user: u64) -> bool {
        let bets = if wager.round_id == self.round_id {
            self.round.as_ref().map(|round| &round.bets)
        } else {
            self.last_result
                .as_ref()
                .filter(|last| last.id == wager.round_id)
                .map(|last| &last.bets)
        };
        bets.is_some_and(|bets| {
            bets.iter()
                .any(|bet| bet.user == user && bet.request_id == wager.request_id)
        })
    }
}

struct Wager<'a> {
    round_id: u64,
    request_id: &'a str,
    bet: &'a str,
    key: String,
    space: Space,
}

impl<'a> Wager<'a> {
    fn parse(body: &'a Value) -> Result<Self, String> {
        let round_id = body["round_id"]
            .as_str()
            .and_then(|id| id.parse().ok())
            .ok_or("Нужен номер раунда.")?;
        let request_id = body["request_id"]
            .as_str()
            .filter(|id| {
                (1..=64).contains(&id.len())
                    && id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            })
            .ok_or("Неверный номер ставки.")?;
        let bet = body["bet"]
            .as_str()
            .filter(|bet| !bet.is_empty() && bet.len() <= 64)
            .ok_or("Укажите сумму ставки.")?;
        let (space, key) = body["space"]
            .as_str()
            .filter(|space| space.len() <= 64)
            .and_then(Space::parse)
            .ok_or("Выберите клетку на столе.")?;
        Ok(Self {
            round_id,
            request_id,
            bet,
            key,
            space,
        })
    }
}

#[derive(Default)]
pub struct State(Mutex<HashMap<u64, Table>>);

impl State {
    fn act(
        &self,
        db: &Database,
        wallet: Wallet<'_>,
        action: &str,
        body: &Value,
        clock: impl Fn() -> Instant,
    ) -> rusqlite::Result<Result<Value, String>> {
        let wager = match action {
            "state" => None,
            "bet" => match Wager::parse(body) {
                Ok(wager) => Some(wager),
                Err(error) => return Ok(Err(error)),
            },
            _ => return Ok(Err("Неизвестное действие рулетки.".into())),
        };
        let mut tables = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let table = tables.entry(wallet.guild).or_default();
        table.maintain(db, wallet.guild, clock());

        let mut bet_error = None;
        let balance = if let Some(wager) = wager.filter(|wager| !table.accepted(wager, wallet.user))
        {
            let (balance, placed) = db.transaction(|tx| {
                let (balance, _) = wallet.open(tx)?;
                // Check the deadline after acquiring SQLite, which can block behind other games.
                let now = clock();
                let round_id = table.round_id;
                if wager.round_id != round_id || table.round.as_ref().is_some_and(|round| now >= round.deadline) {
                    return Ok((balance, Err("Раунд уже закрыт." .into())));
                }
                if table.round.as_ref().is_some_and(|round| round.bets.len() >= MAX_BETS) {
                    return Ok((balance, Err("В этом раунде уже 50 ставок.".into())));
                }
                let ticket = if table.round.is_none() {
                    let in_use = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM game_wagers WHERE round_id = ?1 OR ticket = ?1)",
                        [round_id as i64],
                        |row| row.get::<_, bool>(0),
                    )?;
                    if in_use {
                        return Ok((balance, Err("Не удалось открыть раунд. Обновите игру.".into())));
                    }
                    round_id
                } else {
                    new_id()
                };
                match wagers::place_on(tx, round_id, ticket, wallet.guild, wallet.user, wager.bet, MIN_BET)? {
                    Ok(amount) => Ok((balance - amount, Ok((now, Bet {
                        ticket,
                        user: wallet.user,
                        name: wallet.name.to_owned(),
                        amount,
                        key: wager.key,
                        space: wager.space,
                        request_id: wager.request_id.to_owned(),
                    })))),
                    Err(error) => Ok((balance, Err(error))),
                }
            })?;
            match placed {
                Ok((now, bet)) => {
                    let round = table.round.get_or_insert_with(|| Round {
                        started: now,
                        deadline: now + WAIT,
                        bets: Vec::new(),
                        refund_pending: false,
                    });
                    round.deadline = (now + WAIT).min(round.started + MAX_ROUND);
                    round.bets.push(bet);
                }
                Err(error) => bet_error = Some(error),
            }
            balance
        } else {
            wallet.open(&db.lock())?.0
        };

        // A wager rejection carries an authoritative snapshot. Transport failures can
        // then be retried with the same request ID without guessing whether it was debited.
        Ok(Ok(table.snapshot(
            wallet.user,
            balance,
            clock(),
            bet_error,
            read_history(db, wallet.guild)?,
        )))
    }

    fn sweep(&self, db: &Database, now: Instant) {
        let mut tables = self.0.lock().unwrap_or_else(|error| error.into_inner());
        for (&guild, table) in tables.iter_mut() {
            table.maintain(db, guild, now);
        }
    }
}

pub fn handle(
    data: &Data,
    guild: u64,
    user: u64,
    name: &str,
    action: &str,
    body: &Value,
) -> rusqlite::Result<Result<Value, String>> {
    data.roulette.act(
        &data.db,
        Wallet {
            guild,
            user,
            name,
            start: data.economy.start_balance,
        },
        action,
        body,
        Instant::now,
    )
}

pub fn sweep(data: &Data) {
    data.roulette.sweep(&data.db, Instant::now());
}

/// Route the rules button on historical Discord messages to the Activity.
pub async fn pressed(ctx: &s::Context, press: &s::ComponentInteraction) -> Result<bool, s::Error> {
    if press.data.custom_id != HELP_BUTTON {
        return Ok(false);
    }
    press
        .create_response(
            ctx,
            s::CreateInteractionResponse::Message(
                s::CreateInteractionResponseMessage::new()
                    .content(
                        "Рулетка теперь в мини-приложении. Откройте /games и выберите «Рулетка».",
                    )
                    .ephemeral(true),
            ),
        )
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        state: State,
        db: crate::db::Database,
        now: Instant,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                state: State::default(),
                db: crate::db::Database::in_memory().unwrap(),
                now: Instant::now(),
            }
        }

        fn call(&self, user: u64, seconds: u64, action: &str, body: Value) -> Value {
            self.state
                .act(
                    &self.db,
                    crate::db::Wallet {
                        guild: 10,
                        user,
                        name: "Игрок",
                        start: 500,
                    },
                    action,
                    &body,
                    || self.now + Duration::from_secs(seconds),
                )
                .unwrap()
                .unwrap()
        }
    }

    #[test]
    fn minimum_bet_opens_a_round_and_settles_once() {
        let f = Fixture::new();
        let idle = f.call(1, 0, "state", json!({}));
        assert_eq!(idle["min_bet"], 10);
        let body = json!({
            "round_id": idle["round_id"], "request_id": "minimum", "bet": "10", "space": "0",
        });
        let placed = f.call(1, 0, "bet", body.clone());
        assert!(placed["bet_error"].is_null());
        assert_eq!(placed["min_bet"], 10);
        assert_eq!(placed["balance"], 490);
        assert_eq!(placed["round"]["remaining_ms"], 15000);
        assert_eq!(placed["round"]["your_bets"][0]["amount"], 10);
        let retry = f.call(1, 1, "bet", body.clone());
        assert_eq!(retry["round"]["bet_count"], 1);
        assert_eq!(retry["balance"], 490);
        assert_eq!(retry["round"]["remaining_ms"], 14000);

        let settled = f.call(1, 15, "bet", body);
        let number = settled["last_result"]["number"].as_u64().unwrap();
        assert!(settled["bet_error"].is_null());
        assert!(settled["round"].is_null());
        assert_eq!(settled["balance"], if number == 0 { 850 } else { 490 });
        assert_eq!(
            settled["last_result"]["players"][0]["net"],
            if number == 0 { 350 } else { -10 },
        );
    }

    #[test]
    fn public_bets_keep_each_stake_and_report_exact_round_payouts() {
        let f = Fixture::new();
        let idle = f.call(3, 0, "state", json!({}));
        let stakes = [(1, "red", 10), (1, "red", 13), (2, "black", 17), (2, "0", 19)];
        for (i, (user, space, amount)) in stakes.into_iter().enumerate() {
            let placed = f.call(user, 0, "bet", json!({
                "round_id": idle["round_id"], "request_id": format!("private-{i}"),
                "bet": amount.to_string(), "space": space,
            }));
            assert!(placed["bet_error"].is_null());
        }
        let open = f.call(3, 0, "state", json!({}));
        assert_eq!(open["round"]["your_bets"], json!([]));
        let public = open["round"]["bets"].as_array().unwrap();
        assert_eq!(public.len(), stakes.len());
        for (row, (user, space, amount)) in public.iter().zip(stakes) {
            assert_eq!(row, &json!({
                "user_id": user.to_string(), "name": "Игрок", "space": space, "amount": amount,
            }));
        }

        let settled = f.call(3, 15, "state", json!({}));
        let result = &settled["last_result"];
        assert_eq!(result["id"], open["round"]["id"]);
        assert!(settled["round"].is_null());
        let number = result["number"].as_u64().unwrap() as u8;
        let mut totals = [0; 2];
        let paid = result["bets"].as_array().unwrap();
        assert_eq!(paid.len(), public.len());
        for ((row, before), (user, space, amount)) in paid.iter().zip(public).zip(stakes) {
            let multiplier = match space {
                "red" if RED.contains(&number) => 2,
                "black" if number != 0 && !RED.contains(&number) => 2,
                "0" if number == 0 => 36,
                _ => 0,
            };
            let mut expected = before.clone();
            expected["payout"] = json!(amount * multiplier);
            assert_eq!(row, &expected);
            totals[user as usize - 1] += amount * (multiplier - 1);
        }
        for (index, net) in totals.into_iter().enumerate() {
            let player = f.call(index as u64 + 1, 15, "state", json!({}));
            assert_eq!(player["balance"], 500 + net);
            assert_eq!(player["last_result"]["bets"], result["bets"]);
        }
        let next = f.call(3, 16, "bet", json!({
            "round_id": settled["round_id"], "request_id": "next", "bet": "20", "space": "even",
        }));
        assert_ne!(next["round"]["id"], result["id"]);
        assert_eq!(next["round"]["bets"], json!([
            {"user_id": "3", "name": "Игрок", "space": "even", "amount": 20}
        ]));
        assert_eq!(next["last_result"]["bets"], result["bets"]);
    }

    #[test]
    fn spin_history_keeps_only_the_latest_ten_results_for_each_server() {
        let f = Fixture::new();
        let mut expected = Vec::new();
        for step in 0..12_u64 {
            let now = step * 20;
            let idle = f.call(1, now, "state", json!({}));
            let placed = f.call(1, now, "bet", json!({
                "round_id": idle["round_id"], "request_id": "test", "bet": "10", "space": "0",
            }));
            assert!(placed["bet_error"].is_null());
            assert_eq!(placed["history"], json!(expected));
            let settled = f.call(1, now + 15, "state", json!({}));
            expected.insert(0, json!({
                "id": idle["round_id"], "number": settled["last_result"]["number"],
            }));
            expected.truncate(HISTORY_LIMIT as usize);
            assert_eq!(settled["history"], json!(expected));
        }
        let restarted = State::default();
        let after_restart = |guild| restarted.act(
            &f.db, Wallet { guild, user: 1, name: "Игрок", start: 500 },
            "state", &json!({}), Instant::now,
        ).unwrap().unwrap();
        assert_eq!(after_restart(10)["history"], json!(expected));
        assert_eq!(after_restart(11)["history"], json!([]));
        let stored: i64 = f.db.lock().query_row(
            "SELECT COUNT(*) FROM miniapp_roulette_history", [], |row| row.get(0),
        ).unwrap();
        assert_eq!(stored, HISTORY_LIMIT);
    }

    #[test]
    fn wagers_below_ten_do_not_open_a_round() {
        let f = Fixture::new();
        let idle = f.call(1, 0, "state", json!({}));
        for bet in ["0", "-1", "1", "9"] {
            let rejected = f.call(1, 0, "bet", json!({
                "round_id": idle["round_id"], "request_id": bet, "bet": bet, "space": "red",
            }));
            assert!(rejected["bet_error"].is_string());
            assert_eq!(rejected["balance"], 500);
            assert_eq!(rejected["round_id"], idle["round_id"]);
            assert!(rejected["round"].is_null());
        }
    }

    #[test]
    fn shared_round_extends_to_hard_cap_and_rejects_stale_wagers() {
        let f = Fixture::new();
        let idle = f.call(1, 0, "state", serde_json::json!({}));
        for (index, seconds) in [0, 14, 28, 42, 56].into_iter().enumerate() {
            let body = serde_json::json!({
                "round_id": idle["round_id"], "request_id": index.to_string(),
                "bet": "10", "space": "red",
            });
            let state = f.call((index % 2 + 1) as u64, seconds, "bet", body);
            assert!(state["bet_error"].is_null());
            assert!(state["last_result"].is_null());
            assert!(state["round"].get("number").is_none());
            assert_eq!(state["round"]["bet_count"], index + 1);
            assert_eq!(
                state["round"]["remaining_ms"],
                if seconds == 56 { 4000 } else { 15000 }
            );
        }
        let late = f.call(
            2,
            60,
            "bet",
            serde_json::json!({
                "round_id": idle["round_id"], "request_id": "late", "bet": "10", "space": "red",
            }),
        );
        assert!(late["bet_error"].is_string());
        assert_ne!(late["round_id"], idle["round_id"]);
        assert!(late["round"].is_null());
        assert!(late["last_result"]["number"].as_u64().unwrap() <= 36);
        assert_eq!(late["last_result"]["players"].as_array().unwrap().len(), 2);
        assert_eq!(wagers::refund(&f.db, None).unwrap(), 0);
    }

    #[test]
    fn concurrent_retry_debits_once_and_replays_after_settlement() {
        let f = Fixture::new();
        let idle = f.call(1, 0, "state", serde_json::json!({}));
        let body = serde_json::json!({
            "round_id": idle["round_id"], "request_id": "retry", "bet": "all", "space": "0",
        });
        std::thread::scope(|scope| {
            let first = scope.spawn(|| f.call(1, 0, "bet", body.clone()));
            let second = f.call(1, 0, "bet", body.clone());
            for state in [first.join().unwrap(), second] {
                assert_eq!(state["balance"], 0);
                assert_eq!(state["round"]["bet_count"], 1);
                assert_eq!(state["round"]["your_bets"][0]["request_id"], "retry");
            }
        });
        f.state.sweep(&f.db, f.now + Duration::from_secs(15));
        let settled = f.call(1, 15, "bet", body);
        let number = settled["last_result"]["number"].as_u64().unwrap();
        let balance = if number == 0 { 18000 } else { 0 };
        assert!(settled["bet_error"].is_null());
        assert_eq!(settled["balance"], balance);
        assert_eq!(settled["last_result"]["players"][0]["net"], balance - 500);
        assert_eq!(
            settled["last_result"]["your_bets"][0]["request_id"],
            "retry"
        );
        assert!(settled["round"].is_null());
        assert_eq!(wagers::refund(&f.db, None).unwrap(), 0);
    }

    #[test]
    fn failed_settlement_locks_the_round_until_every_wager_is_refunded() {
        let f = Fixture::new();
        let idle = f.call(1, 0, "state", json!({}));
        for space in ["0", "red", "black"] {
            let state = f.call(
                1,
                0,
                "bet",
                json!({"round_id": idle["round_id"], "request_id": space, "bet": "10", "space": space}),
            );
            assert!(state["bet_error"].is_null());
        }
        let set_balance = |balance: i64| {
            f.db.lock()
                .execute(
                    "UPDATE economy SET balance = ?1 WHERE guild_id = 10 AND user_id = 1",
                    [balance],
                )
                .unwrap();
        };
        // Every possible outcome owes this wallet a payout, which cannot fit.
        set_balance(i64::MAX);
        let pending = f.call(1, 15, "state", json!({}));
        assert_eq!(pending["round"]["refunding"], true);
        assert_eq!(pending["round"]["remaining_ms"], 0);
        assert!(pending["last_result"].is_null());
        let rejected = f.call(
            2,
            16,
            "bet",
            json!({"round_id": idle["round_id"], "request_id": "closed", "bet": "10", "space": "0"}),
        );
        assert!(rejected["bet_error"].is_string());
        assert_eq!(rejected["balance"], 500);

        set_balance(470);
        let now = f.now + Duration::from_secs(17);
        f.state.sweep(&f.db, now);
        f.state.sweep(&f.db, now);
        let settled = f.call(1, 17, "state", json!({}));
        assert_eq!(settled["balance"], 500);
        assert!(settled["round"].is_null());
        assert_eq!(settled["last_result"]["refunded"], true);
        assert!(settled["last_result"]["number"].is_null());
        assert_eq!(settled["history"], json!([]));
        assert_eq!(settled["last_result"]["players"][0]["net"], 0);
        for bet in settled["last_result"]["bets"].as_array().unwrap() {
            assert_eq!(bet["payout"], bet["amount"]);
        }
        assert_eq!(wagers::refund(&f.db, None).unwrap(), 0);
    }

    #[test]
    fn rejected_wagers_do_not_extend_the_window() {
        let f = Fixture::new();
        let idle = f.call(1, 0, "state", json!({}));
        let wager = |request_id: &str, bet: &str| json!({"round_id": idle["round_id"], "request_id": request_id, "bet": bet, "space": "0"});
        let opened = f.call(1, 0, "bet", wager("first", "10"));
        assert_eq!(opened["round"]["remaining_ms"], 15000);
        let failed = f.call(2, 14, "bet", wager("insufficient", "510"));
        assert!(failed["bet_error"].is_string());
        assert_eq!(failed["balance"], 500);
        assert_eq!(failed["round"]["your_bets"], json!([]));
        assert_eq!(failed["round"]["bet_count"], 1);
        assert_eq!(failed["round"]["remaining_ms"], 1000);
    }

    #[test]
    fn european_wheel_payouts_and_zero() {
        for &(key, _, space) in GROUPS {
            assert_eq!(space.multiplier(0), 0, "{key}");
            let payouts: i64 = (0..=36).map(|number| space.multiplier(number)).sum();
            assert_eq!(payouts, 36, "{key}");
        }
        assert_eq!(Space::parse("0").unwrap().0.multiplier(0), 36);
        assert_eq!(Space::parse("black").unwrap().0.multiplier(22), 2);
        assert_eq!(Space::parse("1–18").unwrap().0.multiplier(22), 0);
        assert!(Space::parse("00").is_none());
        assert!(Space::parse("37").is_none());
    }
}
