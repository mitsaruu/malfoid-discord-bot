//! Блэкджек в Discord Activity. Каждый стол играет против одного дилера.

use crate::{
    blackjack::{card_value, hand_value, new_shoe, Card},
    db::{change_balance, Database, Wallet},
    economy::MAX_BET,
    wagers, Data, Error,
};
use rusqlite::{params, Transaction};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

const LOBBY_TIME: Duration = Duration::from_secs(90);
const TURN_TIME: Duration = Duration::from_secs(30);
const RESULT_TIME: Duration = Duration::from_secs(20);
const REFUND_RETRY: Duration = Duration::from_secs(5);
const MAX_PLAYERS: usize = 7;
const MAX_HANDS: usize = 4;
const RANKS: [&str; 13] = [
    "2", "3", "4", "5", "6", "7", "8", "9", "10", "J", "Q", "K", "A",
];
const SUITS: [&str; 4] = ["♠", "♥", "♦", "♣"];

#[derive(Default)]
pub struct State {
    tables: Mutex<HashMap<(u64, Mode), Table>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Mode {
    Online,
    Solo(u64),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Lobby,
    Playing,
    Result,
    Refunding,
    Refunded,
}

#[derive(Clone)]
struct Table {
    id: u64,
    players: Vec<Player>,
    dealer: Vec<Card>,
    shoe: Vec<Card>,
    current: Option<(usize, usize)>,
    turn: u64,
    deadline: Instant,
    phase: Phase,
}

#[derive(Clone)]
struct Player {
    id: u64,
    name: String,
    bet: i64,
    hands: Vec<Hand>,
}

#[derive(Clone)]
struct Hand {
    cards: Vec<Card>,
    ticket: u64,
    double_ticket: Option<u64>,
    done: bool,
    outcome: Option<(&'static str, i64)>,
}

impl Hand {
    fn new(ticket: u64) -> Self {
        Self {
            cards: Vec::new(),
            ticket,
            double_ticket: None,
            done: false,
            outcome: None,
        }
    }

    fn tickets(&self) -> impl Iterator<Item = u64> {
        std::iter::once(self.ticket).chain(self.double_ticket)
    }
}

impl Table {
    fn new(now: Instant) -> Self {
        Self {
            id: rand::random(),
            players: Vec::new(),
            dealer: Vec::new(),
            shoe: Vec::new(),
            current: None,
            turn: 0,
            deadline: now + LOBBY_TIME,
            phase: Phase::Lobby,
        }
    }

    fn draw(&mut self) -> Card {
        if self.shoe.is_empty() {
            self.shoe = new_shoe();
        }
        self.shoe.pop().unwrap()
    }

    fn start(&mut self, now: Instant) {
        for _ in 0..2 {
            for seat in 0..self.players.len() {
                let card = self.draw();
                self.players[seat].hands[0].cards.push(card);
            }
            let card = self.draw();
            self.dealer.push(card);
        }
        let dealer_blackjack = natural(&self.dealer);
        for player in &mut self.players {
            let hand = &mut player.hands[0];
            hand.done = dealer_blackjack || natural(&hand.cards);
        }
        self.phase = Phase::Playing;
        self.next_turn(now);
    }

    fn next_turn(&mut self, now: Instant) {
        self.current = self.players.iter().enumerate().find_map(|(seat, player)| {
            player
                .hands
                .iter()
                .position(|hand| !hand.done)
                .map(|hand| (seat, hand))
        });
        self.turn += 1;
        self.deadline = now + TURN_TIME;
    }

    fn finish(&mut self, tx: &Transaction<'_>, now: Instant) -> rusqlite::Result<()> {
        if self.phase != Phase::Playing || self.current.is_some() {
            return Ok(());
        }
        let dealer_blackjack = natural(&self.dealer);
        let needs_draw = self.players.iter().any(|player| {
            player.hands.iter().any(|hand| {
                hand_value(&hand.cards) <= 21 && !(player.hands.len() == 1 && natural(&hand.cards))
            })
        });
        if needs_draw {
            while hand_value(&self.dealer) < 17 {
                let card = self.draw();
                self.dealer.push(card);
            }
        }
        let dealer_total = hand_value(&self.dealer);
        let mut payouts = Vec::new();
        for player in &mut self.players {
            let split = player.hands.len() > 1;
            for hand in &mut player.hands {
                let result = outcome(
                    &hand.cards,
                    player.bet,
                    split,
                    dealer_total,
                    dealer_blackjack,
                );
                hand.outcome = Some(result);
                payouts.extend(hand.tickets().map(|ticket| (ticket, result.1)));
            }
        }
        wagers::settle_on(tx, self.id, &payouts)?;
        self.phase = Phase::Result;
        self.deadline = now + RESULT_TIME;
        Ok(())
    }

    fn allowed(&self, user: u64, mode: Mode, balance: i64, minimum: i64) -> Vec<&'static str> {
        match self.phase {
            Phase::Refunded => vec!["join"],
            Phase::Result if matches!(mode, Mode::Solo(_)) => vec!["join"],
            Phase::Lobby => {
                if self.players.first().is_some_and(|host| host.id == user)
                    && self.players.len() >= 2
                {
                    vec!["start", "leave"]
                } else if self.players.iter().any(|player| player.id == user) {
                    vec!["leave"]
                } else if self.players.len() < MAX_PLAYERS {
                    vec!["join"]
                } else {
                    vec![]
                }
            }
            Phase::Playing => {
                let Some((seat, index)) = self.current else {
                    return vec![];
                };
                let player = &self.players[seat];
                if player.id != user {
                    return vec![];
                }
                let mut actions = vec!["hit", "stand"];
                let hand = &player.hands[index];
                if hand.cards.len() == 2 && balance >= player.bet {
                    actions.push("double");
                    if player.hands.len() < MAX_HANDS
                        && card_value(hand.cards[0]) == card_value(hand.cards[1])
                    {
                        actions.push("split");
                    }
                }
                actions
            }
            Phase::Result | Phase::Refunding => vec![],
        }
        .into_iter()
        .filter(|action| *action != "join" || balance >= minimum)
        .collect()
    }

    fn play(&mut self, action: &str, ticket: Option<u64>, now: Instant) {
        let (seat, index) = self.current.unwrap();
        match action {
            "hit" | "double" => {
                let card = self.draw();
                let hand = &mut self.players[seat].hands[index];
                hand.cards.push(card);
                hand.double_ticket = ticket;
                hand.done = ticket.is_some() || hand_value(&hand.cards) >= 21;
            }
            "stand" => self.players[seat].hands[index].done = true,
            "split" => {
                let first_draw = self.draw();
                let second_draw = self.draw();
                let hands = &mut self.players[seat].hands;
                let first = &mut hands[index];
                let second_card = first.cards.pop().unwrap();
                first.cards.push(first_draw);
                first.done = card_value(first.cards[0]) == 11 || hand_value(&first.cards) == 21;
                let mut second = Hand::new(ticket.unwrap());
                second.cards = vec![second_card, second_draw];
                second.done = card_value(second_card) == 11 || hand_value(&second.cards) == 21;
                hands.insert(index + 1, second);
            }
            _ => unreachable!(),
        }
        self.next_turn(now);
    }

    fn snapshot(&self, now: Instant) -> Value {
        let phase = match self.phase {
            Phase::Lobby => "lobby",
            Phase::Playing => "playing",
            Phase::Refunding => "refunding",
            Phase::Result | Phase::Refunded => "result",
        };
        let dealer = match self.phase {
            Phase::Playing => {
                let up = &self.dealer[..1];
                json!({"cards": [card_text(up[0]), Value::Null], "total": hand_value(up)})
            }
            Phase::Result => json!({
                "cards": self.dealer.iter().copied().map(card_text).collect::<Vec<_>>(),
                "total": hand_value(&self.dealer),
            }),
            _ => Value::Null,
        };
        let players: Vec<_> = self.players.iter().enumerate().map(|(seat, player)| {
            let hands: Vec<_> = player.hands.iter().enumerate().filter(|(_, hand)| !hand.cards.is_empty())
                .map(|(index, hand)| {
                    let tickets = hand.tickets().count() as i64;
                    json!({
                        "cards": hand.cards.iter().copied().map(card_text).collect::<Vec<_>>(),
                        "total": hand_value(&hand.cards),
                        "bet": player.bet * tickets,
                        "active": self.current == Some((seat, index)),
                        "outcome": hand.outcome.map(|(kind, _)| kind),
                        "payout": hand.outcome.map(|(_, amount)| amount * tickets),
                    })
                }).collect();
            json!({"id": player.id.to_string(), "name": player.name, "bet": player.bet, "hands": hands})
        }).collect();
        json!({
            "id": self.id.to_string(),
            "phase": phase,
            "refunded": self.phase == Phase::Refunded,
            "host_id": self.players.first().map(|player| player.id.to_string()),
            "current_user": self.current.map(|(seat, _)| self.players[seat].id.to_string()),
            "turn": self.turn,
            "remaining_ms": self.deadline.saturating_duration_since(now).as_millis() as u64,
            "dealer": dealer,
            "players": players,
        })
    }
}

fn natural(cards: &[Card]) -> bool {
    cards.len() == 2 && hand_value(cards) == 21
}

fn outcome(
    cards: &[Card],
    bet: i64,
    split: bool,
    dealer: i64,
    dealer_blackjack: bool,
) -> (&'static str, i64) {
    let total = hand_value(cards);
    let blackjack = !split && natural(cards);
    match (blackjack, dealer_blackjack) {
        (true, true) => ("push", bet),
        (true, false) => ("blackjack", bet + bet * 3 / 2),
        _ if total > 21 => ("bust", 0),
        (false, true) => ("loss", 0),
        _ if dealer > 21 || total > dealer => ("win", bet * 2),
        _ if total == dealer => ("push", bet),
        _ => ("loss", 0),
    }
}

fn card_text((rank, suit): Card) -> String {
    format!("{}{}", RANKS[rank as usize], SUITS[suit as usize])
}

fn refund(db: &Database, table: &mut Table, now: Instant) {
    table.current = None;
    match wagers::refund(db, Some(table.id)) {
        Ok(_) => {
            table.phase = Phase::Refunded;
            table.deadline = now + RESULT_TIME;
        }
        Err(error) => {
            eprintln!("Mini App blackjack: refund {}: {error}", table.id);
            table.phase = Phase::Refunding;
            table.deadline = now + REFUND_RETRY;
        }
    }
}

fn maintain(db: &Database, table: &mut Table, now: Instant) -> bool {
    if now < table.deadline {
        return true;
    }
    match table.phase {
        Phase::Lobby | Phase::Refunding => refund(db, table, now),
        Phase::Result | Phase::Refunded => return false,
        Phase::Playing => {
            let mut next = table.clone();
            let (seat, _) = next.current.unwrap();
            for hand in &mut next.players[seat].hands {
                hand.done = true;
            }
            next.next_turn(now);
            match db.transaction(|tx| next.finish(tx, now)) {
                Ok(()) => *table = next,
                Err(error) => {
                    eprintln!(
                        "Mini App blackjack: timeout settlement {}: {error}",
                        table.id
                    );
                    refund(db, table, now);
                }
            }
        }
    }
    true
}

impl State {
    fn act(
        &self,
        db: &Database,
        wallet: Wallet<'_>,
        minimum: i64,
        action: &str,
        body: &Value,
        clock: impl FnOnce() -> Instant,
    ) -> rusqlite::Result<Result<Value, String>> {
        let mode = match body.get("mode") {
            None => Mode::Online,
            Some(mode) if mode == "online" => Mode::Online,
            Some(mode) if mode == "solo" => Mode::Solo(wallet.user),
            _ => return Ok(Err("Неизвестный режим блэкджека.".into())),
        };
        let key = (wallet.guild, mode);
        let mut tables = self.tables.lock().unwrap();
        let now = clock();
        if tables
            .get_mut(&key)
            .is_some_and(|table| !maintain(db, table, now))
        {
            tables.remove(&key);
        }
        let (balance, _) = wallet.open(&db.lock())?;
        if action != "state" {
            let expected = match body.get("round") {
                None | Some(Value::Null) => None,
                Some(Value::String(round)) => Some(round.as_str()),
                _ => return Ok(Err("Неверный номер стола.".into())),
            };
            let old = tables.get(&key);
            if old.map(|table| table.id.to_string()).as_deref() != expected {
                return Ok(Err("Стол уже изменился. Обновите игру.".into()));
            }
            if action == "join" && balance < minimum {
                return Ok(Err("В кошельке меньше минимальной ставки.".into()));
            }
            let allowed = old.map_or_else(
                || vec!["join"],
                |table| table.allowed(wallet.user, mode, balance, minimum),
            );
            if !allowed.contains(&action) {
                return Ok(Err("Это действие сейчас недоступно.".into()));
            }
            let mut next = match old {
                Some(table) if matches!(table.phase, Phase::Lobby | Phase::Playing) => {
                    table.clone()
                }
                _ => Table::new(now),
            };
            if next.phase == Phase::Playing && body["turn"].as_u64() != Some(next.turn) {
                return Ok(Err("Ход уже изменился.".into()));
            }

            let updated = db.transaction(|tx| {
                match action {
                    "join" => {
                        let Some(bet) = body["bet"].as_str().filter(|bet| bet.len() <= 64) else {
                            return Ok(Err("Укажите ставку в монетах.".into()));
                        };
                        let ticket = if next.players.is_empty() { next.id } else { rand::random() };
                        let amount = match wagers::place_on(tx, next.id, ticket, wallet.guild, wallet.user, bet, minimum)? {
                            Ok(amount) => amount,
                            Err(error) => return Ok(Err(error)),
                        };
                        next.players.push(Player {
                            id: wallet.user,
                            name: wallet.name.to_owned(),
                            bet: amount,
                            hands: vec![Hand::new(ticket)],
                        });
                        if matches!(mode, Mode::Solo(_)) {
                            next.start(now);
                        }
                    }
                    "leave" => {
                        let index = next.players.iter().position(|player| player.id == wallet.user).unwrap();
                        let player = next.players.remove(index);
                        let removed = tx.execute(
                            "DELETE FROM game_wagers WHERE ticket = ?1 AND round_id = ?2 AND guild_id = ?3 AND user_id = ?4 AND amount = ?5",
                            params![player.hands[0].ticket as i64, next.id as i64, wallet.guild as i64, wallet.user as i64, player.bet],
                        )?;
                        if removed != 1 {
                            return Err(rusqlite::Error::InvalidQuery);
                        }
                        change_balance(tx, wallet.guild, wallet.user, player.bet)?;
                    }
                    "start" => next.start(now),
                    "hit" | "stand" => next.play(action, None, now),
                    "split" | "double" => {
                        let ticket = rand::random();
                        let (seat, _) = next.current.unwrap();
                        let bet = next.players[seat].bet.to_string();
                        if let Err(error) = wagers::place_on(tx, next.id, ticket, wallet.guild, wallet.user, &bet, 1)? {
                            return Ok(Err(error));
                        }
                        next.play(action, Some(ticket), now);
                    }
                    _ => unreachable!(),
                }
                next.finish(tx, now)?;
                Ok(Ok(()))
            });
            match updated {
                Ok(Ok(())) if next.players.is_empty() => {
                    tables.remove(&key);
                }
                Ok(Ok(())) => {
                    tables.insert(key, next);
                }
                Ok(Err(error)) => return Ok(Err(error)),
                Err(error) => {
                    if let Some(table) = tables
                        .get_mut(&key)
                        .filter(|table| matches!(table.phase, Phase::Lobby | Phase::Playing))
                    {
                        eprintln!(
                            "Mini App blackjack: action {action} for {}: {error}",
                            table.id
                        );
                        refund(db, table, now);
                    } else {
                        return Err(error);
                    }
                }
            }
        }
        let balance = if action == "state" {
            balance
        } else {
            wallet.open(&db.lock())?.0
        };
        let table = tables.get(&key);
        let actions = match table {
            Some(table) => table.allowed(wallet.user, mode, balance, minimum),
            None if balance >= minimum => vec!["join"],
            None => vec![],
        };
        Ok(Ok(json!({
            "user_id": wallet.user.to_string(),
            "balance": balance,
            "min_bet": minimum,
            "max_bet": MAX_BET,
            "max_players": MAX_PLAYERS,
            "actions": actions,
            "table": table.map(|table| table.snapshot(now)),
        })))
    }

    fn sweep(&self, db: &Database, now: Instant) {
        self.tables
            .lock()
            .unwrap()
            .retain(|_, table| maintain(db, table, now));
    }
}

pub fn handle(
    data: &Data,
    guild: u64,
    user: u64,
    name: &str,
    action: &str,
    body: &Value,
) -> Result<Result<Value, String>, Error> {
    let wallet = Wallet {
        guild,
        user,
        name,
        start: data.economy.start_balance,
    };
    Ok(data.table_blackjack.act(
        &data.db,
        wallet,
        data.economy.min_bet,
        action,
        body,
        Instant::now,
    )?)
}

pub fn sweep(data: &Data) {
    data.table_blackjack.sweep(&data.db, Instant::now());
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUILD: u64 = 123_456_789_012_345_678;
    const ALICE: u64 = 223_456_789_012_345_678;
    const BOB: u64 = 323_456_789_012_345_678;

    struct Fixture {
        db: Database,
        state: State,
        now: Instant,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                db: Database::in_memory().unwrap(),
                state: State::default(),
                now: Instant::now(),
            }
        }

        fn act(&self, user: u64, action: &str, body: Value) -> Result<Value, String> {
            self.state
                .act(
                    &self.db,
                    Wallet {
                        guild: GUILD,
                        user,
                        name: "<player>",
                        start: 500,
                    },
                    10,
                    action,
                    &body,
                    || self.now,
                )
                .unwrap()
        }

        fn join(&self, user: u64, round: Option<&Value>) -> Value {
            self.act(user, "join", json!({"round": round, "bet": "101"}))
                .unwrap()
        }

        fn shoe(&self, cards: &[u8]) {
            self.state
                .tables
                .lock()
                .unwrap()
                .get_mut(&(GUILD, Mode::Online))
                .unwrap()
                .shoe = cards.iter().rev().map(|rank| (*rank, 0)).collect();
        }

        fn balance(&self, user: u64) -> i64 {
            self.db.wallet(GUILD, user, "<player>", 500).unwrap().0
        }
    }

    #[test]
    fn natural_pays_three_to_two_and_dealer_hole_card_is_private_until_settlement() {
        let f = Fixture::new();
        let lobby = f.join(ALICE, None);
        let round = &lobby["table"]["id"];
        f.join(BOB, Some(round));
        // Alice A,K; Bob 10,7; dealer 6,10,2 (cards dealt clockwise).
        f.shoe(&[12, 8, 4, 11, 5, 8, 0]);
        let playing = f.act(ALICE, "start", json!({"round": round})).unwrap();
        assert_eq!(playing["table"]["players"][0]["id"], ALICE.to_string());
        assert_eq!(playing["table"]["current_user"], BOB.to_string());
        assert_eq!(
            playing["table"]["dealer"],
            json!({"cards":["6♠", null], "total":6})
        );
        assert_eq!(f.balance(ALICE), 399);
        let turn = &playing["table"]["turn"];
        let result = f
            .act(BOB, "stand", json!({"round":round, "turn":turn}))
            .unwrap();
        assert_eq!(
            result["table"]["dealer"]["cards"],
            json!(["6♠", "10♠", "2♠"])
        );
        assert_eq!(f.balance(ALICE), 651);
        assert_eq!(f.balance(BOB), 399);
        assert!(f
            .act(BOB, "stand", json!({"round":round, "turn":turn}))
            .is_err());
        assert_eq!(f.balance(ALICE), 651);
    }

    #[test]
    fn leaving_and_lobby_expiry_refund_each_ticket_once() {
        let f = Fixture::new();
        let lobby = f.join(ALICE, None);
        let round = &lobby["table"]["id"];
        f.join(BOB, Some(round));
        assert!(f.act(BOB, "start", json!({"round":round})).is_err());
        let remains = f.act(ALICE, "leave", json!({"round":round})).unwrap();
        assert_eq!(remains["table"]["host_id"], BOB.to_string());
        assert_eq!(f.balance(ALICE), 500);
        assert_eq!(f.balance(BOB), 399);
        assert!(f.act(BOB, "start", json!({"round":round})).is_err());
        f.state.sweep(&f.db, f.now + LOBBY_TIME);
        f.state.sweep(&f.db, f.now + LOBBY_TIME);
        assert_eq!(f.balance(BOB), 500);
    }

    #[test]
    fn split_double_and_timeout_settle_their_escrow_tickets() {
        let f = Fixture::new();
        let lobby = f.join(ALICE, None);
        let round = &lobby["table"]["id"];
        f.join(BOB, Some(round));
        // Alice 8,8; Bob 10,8; dealer 10,7. Split draws 3,10; double draws 10.
        f.shoe(&[6, 8, 8, 6, 6, 5, 1, 8, 8]);
        let initial = f.act(ALICE, "start", json!({"round":round})).unwrap();
        let turn = &initial["table"]["turn"];
        let split = f
            .act(ALICE, "split", json!({"round":round,"turn":turn}))
            .unwrap();
        assert_eq!(f.balance(ALICE), 298);
        assert!(f
            .act(ALICE, "double", json!({"round":round,"turn":turn}))
            .is_err());
        let turn = &split["table"]["turn"];
        let doubled = f
            .act(ALICE, "double", json!({"round":round,"turn":turn}))
            .unwrap();
        assert_eq!(f.balance(ALICE), 197);
        assert_eq!(doubled["table"]["players"][0]["hands"][0]["bet"], 202);
        f.state.sweep(&f.db, f.now + TURN_TIME);
        assert_eq!(f.balance(ALICE), 197);
        f.state.sweep(&f.db, f.now + TURN_TIME * 2);
        assert_eq!(f.balance(ALICE), 803);
        assert_eq!(f.balance(BOB), 601);
    }

    #[test]
    fn database_failure_rolls_back_partial_payout_and_retries_refund() {
        let f = Fixture::new();
        let lobby = f.join(ALICE, None);
        let round = &lobby["table"]["id"];
        f.join(BOB, Some(round));
        f.shoe(&[12, 12, 8, 8, 8, 5]); // Both have blackjack against dealer 17.
        f.db.lock()
            .execute_batch(
                "CREATE TABLE credit_count (credits INTEGER);
                 INSERT INTO credit_count VALUES (0);
                 CREATE TRIGGER reject_one_credit BEFORE UPDATE OF balance ON economy
                 WHEN NEW.balance > OLD.balance
                 BEGIN
                     UPDATE credit_count SET credits = credits + 1;
                     SELECT RAISE(ABORT, 'credit blocked') FROM credit_count WHERE credits > 1;
                 END;",
            )
            .unwrap();
        let pending = f.act(ALICE, "start", json!({"round":round})).unwrap();
        assert_eq!(pending["table"]["phase"], "refunding");
        assert_eq!(f.balance(ALICE), 399);
        assert_eq!(f.balance(BOB), 399);
        f.db.lock()
            .execute_batch("DROP TRIGGER reject_one_credit;")
            .unwrap();
        f.state.sweep(&f.db, f.now + REFUND_RETRY);
        f.state.sweep(&f.db, f.now + REFUND_RETRY);
        assert_eq!(f.balance(ALICE), 500);
        assert_eq!(f.balance(BOB), 500);
        assert_eq!(
            f.act(ALICE, "state", json!({})).unwrap()["table"]["refunded"],
            true
        );
    }

    #[test]
    fn solo_round_is_private_and_a_replayed_join_cannot_charge_again() {
        let f = Fixture::new();
        let bet = json!({"mode":"solo", "bet":"101", "round":null});
        let solo = f.act(ALICE, "join", bet.clone()).unwrap();
        let table = &solo["table"];
        assert!(matches!(
            table["phase"].as_str(),
            Some("playing" | "result")
        ));
        assert_eq!(table["players"].as_array().unwrap().len(), 1);
        assert_eq!(table["players"][0]["id"], ALICE.to_string());
        assert!(f.act(ALICE, "join", bet).is_err());
        assert_eq!(f.balance(ALICE), solo["balance"].as_i64().unwrap());
        assert!(f.act(BOB, "state", json!({"mode":"solo"})).unwrap()["table"].is_null());
        assert!(f.act(ALICE, "state", json!({"mode":"online"})).unwrap()["table"].is_null());
        assert_eq!(f.balance(BOB), 500);
    }

    #[test]
    fn full_lobby_cannot_reserve_an_eighth_seat() {
        let f = Fixture::new();
        let lobby = f.join(ALICE, None);
        let round = &lobby["table"]["id"];
        for user in BOB..BOB + 6 {
            f.join(user, Some(round));
        }
        let spectator = BOB + 6;
        assert!(f
            .act(spectator, "join", json!({"round":round, "bet":"101"}))
            .is_err());
        assert_eq!(f.balance(spectator), 500);
        let view = f.act(spectator, "state", json!({})).unwrap();
        assert_eq!(view["table"]["players"].as_array().unwrap().len(), 7);
    }

    #[test]
    fn split_twenty_one_is_ordinary_and_a_bust_always_loses() {
        let ace_and_king = [(12, 0), (11, 0)];
        assert_eq!(outcome(&ace_and_king, 100, false, 21, true), ("push", 100));
        assert_eq!(outcome(&ace_and_king, 100, true, 21, true), ("loss", 0));
        assert_eq!(outcome(&ace_and_king, 100, true, 20, false), ("win", 200));
        let busted = [(11, 0), (11, 1), (0, 0)];
        assert_eq!(outcome(&busted, 100, false, 22, false), ("bust", 0));
    }

    #[test]
    fn split_aces_end_both_hands_with_exactly_one_card_each() {
        let f = Fixture::new();
        let lobby = f.join(ALICE, None);
        let round = &lobby["table"]["id"];
        f.join(BOB, Some(round));
        // Alice A,A; Bob 10,7; dealer 10,7. Split draws 10,9.
        f.shoe(&[12, 8, 8, 12, 5, 5, 8, 7]);
        let playing = f.act(ALICE, "start", json!({"round":round})).unwrap();
        let split = f
            .act(
                ALICE,
                "split",
                json!({"round":round,"turn":playing["table"]["turn"]}),
            )
            .unwrap();
        assert_eq!(split["table"]["current_user"], BOB.to_string());
        assert_eq!(
            split["table"]["players"][0]["hands"][0]["cards"],
            json!(["A♠", "10♠"])
        );
        assert_eq!(
            split["table"]["players"][0]["hands"][1]["cards"],
            json!(["A♠", "9♠"])
        );
    }
}
