//! Серверный техасский холдем. Деньги находятся в журнале ставок до расчёта раздачи.

use crate::{db::Database, economy::MAX_BET, wagers, Data, Error};
use rand::{seq::SliceRandom, Rng};
use rusqlite::OptionalExtension;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

const SMALL_BLIND: i64 = 5;
const BIG_BLIND: i64 = 10;
const TRAINING_STACK: i64 = 200;
const MAX_SEATS: usize = 10;
const LOBBY_TIME: Duration = Duration::from_secs(180);
const TURN_TIME: Duration = Duration::from_secs(30);
const BOT_TIME: Duration = Duration::from_millis(650);
const RESULT_TIME: Duration = Duration::from_secs(600);

#[derive(Default)]
pub struct State {
    tables: Mutex<Tables>,
}

#[derive(Default)]
struct Tables {
    online: HashMap<u64, Room>,
    solo: HashMap<(u64, u64), Table>,
}

#[derive(Default)]
struct Room {
    table: Option<Table>,
    last: Option<Table>,
}

impl State {
    fn lock(&self) -> MutexGuard<'_, Tables> {
        self.tables
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

#[derive(Clone, Copy)]
struct Card(u8);

impl Card {
    fn rank(self) -> u8 {
        self.0 % 13 + 2
    }

    fn suit(self) -> u8 {
        self.0 / 13
    }

    fn label(self) -> String {
        let rank = [
            "2", "3", "4", "5", "6", "7", "8", "9", "10", "J", "Q", "K", "A",
        ];
        let suit = ["♣", "♦", "♥", "♠"];
        format!(
            "{}{}",
            rank[(self.rank() - 2) as usize],
            suit[self.suit() as usize]
        )
    }
}

// Сначала категория, затем отличающие руки ранги по убыванию значимости.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Rank([u8; 6]);

fn rank_five(cards: [Card; 5]) -> Rank {
    let mut counts = [0; 15];
    for card in cards {
        counts[card.rank() as usize] += 1;
    }
    counts[1] = counts[14]; // Туз также замыкает младший стрит.
    let straight = (5..=14)
        .rev()
        .find(|&high| (high - 4..=high).all(|r| counts[r] != 0));
    let flush = cards.iter().all(|c| c.suit() == cards[0].suit());
    if let Some(high) = straight.filter(|_| flush) {
        return Rank([8, high as u8, 0, 0, 0, 0]);
    }
    let mut groups = [(0, 0); 5];
    for (group, r) in groups.iter_mut().zip((2..=14).filter(|&r| counts[r] > 0)) {
        *group = (counts[r], r as u8);
    }
    groups.sort_unstable_by(|a, b| b.cmp(a));
    let [a, b, c, d, e] = groups.map(|(_, rank)| rank);
    Rank(match (groups[0].0, groups[1].0) {
        (4, _) => [7, a, b, 0, 0, 0],
        (3, 2) => [6, a, b, 0, 0, 0],
        _ if flush => [5, a, b, c, d, e],
        _ if straight.is_some() => [4, straight.unwrap() as u8, 0, 0, 0, 0],
        (3, _) => [3, a, b, c, 0, 0],
        (2, 2) => [2, a, b, c, 0, 0],
        (2, _) => [1, a, b, c, d, 0],
        _ => [0, a, b, c, d, e],
    })
}

fn best_rank(hole: [Card; 2], board: &[Card]) -> Rank {
    let mut cards = [Card(0); 7];
    cards[..2].copy_from_slice(&hole);
    let n = 2 + board.len();
    cards[2..n].copy_from_slice(board);
    let mut best = Rank([0; 6]);
    for a in 0..n - 4 {
        for b in a + 1..n - 3 {
            for c in b + 1..n - 2 {
                for d in c + 1..n - 1 {
                    for e in d + 1..n {
                        best = best.max(rank_five([
                            cards[a], cards[b], cards[c], cards[d], cards[e],
                        ]));
                    }
                }
            }
        }
    }
    best
}

#[derive(Clone)]
struct Seat {
    user: Option<u64>,
    name: String,
    ticket: Option<u64>,
    stack: i64,
    street: i64,
    committed: i64,
    acted_at: Option<i64>,
    hole: Option<[Card; 2]>,
    folded: bool,
}

impl Seat {
    fn new(user: Option<u64>, name: String, stack: i64, ticket: Option<u64>) -> Self {
        Self {
            user,
            name,
            ticket,
            stack,
            street: 0,
            committed: 0,
            acted_at: None,
            hole: None,
            folded: false,
        }
    }

    fn wager(&mut self, amount: i64) {
        self.stack -= amount;
        self.street += amount;
        self.committed += amount;
    }

    fn can_act(&self) -> bool {
        !self.folded && self.stack > 0
    }
}

#[derive(Clone, Copy)]
enum Move {
    Fold,
    Check,
    Call,
    Raise(i64),
    AllIn,
}

#[derive(Clone, Copy)]
enum Street {
    Preflop,
    Flop,
    Turn,
    River,
}

impl Street {
    fn label(self) -> &'static str {
        match self {
            Self::Preflop => "preflop",
            Self::Flop => "flop",
            Self::Turn => "turn",
            Self::River => "river",
        }
    }
}

#[derive(Clone)]
struct Pot {
    amount: i64,
    winners: Vec<usize>,
}

#[derive(Clone)]
struct Outcome {
    at: Instant,
    ranks: Vec<Option<Rank>>,
    awards: Vec<i64>,
    returns: Vec<i64>,
    pots: Vec<Pot>,
}

#[derive(Clone, Copy)]
struct Action {
    seat: usize,
    kind: &'static str,
    amount: i64,
    timed_out: bool,
}

struct Options {
    owed: i64,
    call: i64,
    may_raise: bool,
    min_to: i64,
    max_to: i64,
    all_in: bool,
}

#[derive(Clone)]
struct Table {
    id: u64,
    seats: Vec<Seat>,
    deck: Vec<Card>,
    board: Vec<Card>,
    street: Option<Street>,
    dealer: usize,
    small_blind: usize,
    big_blind: usize,
    target: i64,
    raise_by: i64,
    actor: Option<usize>,
    turn: u32,
    deadline: Instant,
    last_action: Option<Action>,
    outcome: Option<Outcome>,
}

impl Table {
    fn new(id: u64, seats: Vec<Seat>, now: Instant) -> Self {
        Self {
            id,
            seats,
            deck: Vec::new(),
            board: Vec::new(),
            street: None,
            dealer: 0,
            small_blind: 0,
            big_blind: 0,
            target: 0,
            raise_by: BIG_BLIND,
            actor: None,
            turn: 0,
            deadline: now + LOBBY_TIME,
            last_action: None,
            outcome: None,
        }
    }

    fn seat_of(&self, user: u64) -> Option<usize> {
        self.seats.iter().position(|seat| seat.user == Some(user))
    }

    fn clockwise(&self, after: usize) -> impl Iterator<Item = usize> {
        let n = self.seats.len();
        (1..=n).map(move |offset| (after + offset) % n)
    }

    fn deal(&mut self, dealer: usize, now: Instant) {
        self.dealer = dealer;
        let n = self.seats.len();
        self.small_blind = if n == 2 { dealer } else { (dealer + 1) % n };
        self.big_blind = (self.small_blind + 1) % n;
        self.deck = (0..52).map(Card).collect();
        self.deck.shuffle(&mut rand::rng());
        for offset in 1..=n {
            self.seats[(dealer + offset) % n].hole = Some([self.deck.pop().unwrap(), Card(0)]);
        }
        for offset in 1..=n {
            self.seats[(dealer + offset) % n].hole.as_mut().unwrap()[1] = self.deck.pop().unwrap();
        }
        for (seat, blind) in [(self.small_blind, SMALL_BLIND), (self.big_blind, BIG_BLIND)] {
            let seat = &mut self.seats[seat];
            seat.wager(blind.min(seat.stack));
        }
        self.street = Some(Street::Preflop);
        self.target = BIG_BLIND;
        self.progress(self.big_blind, now);
    }

    fn options(&self, actor: usize) -> Options {
        let seat = &self.seats[actor];
        let others_can_bet = self
            .seats
            .iter()
            .enumerate()
            .any(|(i, s)| i != actor && s.can_act());
        let target = if others_can_bet {
            self.target
        } else {
            // Последний игрок с фишками может только ответить на фактическую ставку.
            self.seats
                .iter()
                .enumerate()
                .filter(|(i, s)| *i != actor && !s.folded)
                .map(|(_, s)| s.street)
                .max()
                .unwrap_or(0)
        };
        let owed = (target - seat.street).max(0);
        let may_raise = others_can_bet
            && seat
                .acted_at
                .is_none_or(|acted| self.target - acted >= self.raise_by);
        let max_to = seat.street + seat.stack;
        Options {
            owed,
            call: owed.min(seat.stack),
            may_raise,
            min_to: self.target + self.raise_by,
            max_to,
            all_in: seat.stack <= owed || (may_raise && max_to > self.target),
        }
    }

    fn play(&mut self, actor: usize, action: Move, now: Instant) -> Result<(), String> {
        self.apply_move(actor, action, now, false)
    }

    fn apply_move(
        &mut self,
        actor: usize,
        action: Move,
        now: Instant,
        timed_out: bool,
    ) -> Result<(), String> {
        if self.actor != Some(actor) {
            return Err("Сейчас ход другого игрока.".into());
        }
        let options = self.options(actor);
        let seat = &mut self.seats[actor];
        let (kind, amount) = match action {
            Move::Fold => {
                seat.folded = true;
                ("fold", 0)
            }
            Move::Check if options.owed == 0 => ("check", 0),
            Move::Call if options.owed > 0 => ("call", options.call),
            Move::AllIn if options.all_in => ("all_in", seat.stack),
            Move::Raise(to)
                if options.may_raise && (options.min_to..=options.max_to).contains(&to) =>
            {
                (
                    if self.target == 0 { "bet" } else { "raise" },
                    to - seat.street,
                )
            }
            _ => {
                return Err(
                    "Этот ход сейчас недоступен. Проверьте сумму и доступные действия.".into(),
                )
            }
        };
        seat.wager(amount);
        // Только полный рейз меняет минимальный шаг. У каждого игрока своя точка отсчёта.
        if amount > 0 && seat.street > self.target {
            self.raise_by = self.raise_by.max(seat.street - self.target);
            self.target = seat.street;
        }
        seat.acted_at = Some(self.target);
        self.last_action = Some(Action {
            seat: actor,
            kind,
            amount,
            timed_out,
        });
        self.progress(actor, now);
        Ok(())
    }

    fn progress(&mut self, mut after: usize, now: Instant) {
        loop {
            if self.seats.iter().filter(|s| !s.folded).count() <= 1 {
                self.finish(now);
                return;
            }
            let able = self.seats.iter().filter(|s| s.can_act()).count();
            let next = self.clockwise(after).find(|&i| {
                let seat = &self.seats[i];
                seat.can_act()
                    && (self.options(i).owed > 0 || (able > 1 && seat.acted_at.is_none()))
            });
            if let Some(i) = next {
                self.actor = Some(i);
                self.turn += 1;
                self.deadline = now
                    + if self.seats[i].user.is_none() {
                        BOT_TIME
                    } else {
                        TURN_TIME
                    };
                return;
            }
            let (street, count) = match self.street.unwrap() {
                Street::Preflop => (Street::Flop, 3),
                Street::Flop => (Street::Turn, 1),
                Street::Turn => (Street::River, 1),
                Street::River => {
                    self.finish(now);
                    return;
                }
            };
            self.deck.pop(); // Сжигаем карту перед каждой улицей.
            for _ in 0..count {
                self.board.push(self.deck.pop().unwrap());
            }
            self.street = Some(street);
            self.target = 0;
            self.raise_by = BIG_BLIND;
            for seat in &mut self.seats {
                seat.street = 0;
                seat.acted_at = None;
            }
            after = self.dealer;
        }
    }

    fn finish(&mut self, now: Instant) {
        self.actor = None;
        let showdown = self.board.len() == 5 && self.seats.iter().filter(|s| !s.folded).count() > 1;
        let ranks: Vec<_> = self
            .seats
            .iter()
            .map(|s| (showdown && !s.folded).then(|| best_rank(s.hole.unwrap(), &self.board)))
            .collect();
        let mut outcome = Outcome {
            at: now,
            ranks,
            awards: vec![0; self.seats.len()],
            returns: vec![0; self.seats.len()],
            pots: Vec::new(),
        };
        let mut levels: Vec<_> = self
            .seats
            .iter()
            .filter_map(|s| (s.committed > 0).then_some(s.committed))
            .collect();
        levels.sort_unstable();
        levels.dedup();
        let mut contested: Vec<(Vec<usize>, i64)> = Vec::new();
        let mut previous = 0;
        for level in levels {
            let unit = level - previous;
            previous = level;
            let contributors: Vec<_> = self
                .clockwise(self.dealer)
                .filter(|&i| self.seats[i].committed >= level)
                .collect();
            let eligible: Vec<_> = contributors
                .iter()
                .copied()
                .filter(|&i| !self.seats[i].folded)
                .collect();
            if contributors.len() == 1 || eligible.is_empty() {
                for i in contributors {
                    outcome.returns[i] += unit;
                }
                continue;
            }
            let amount = unit * contributors.len() as i64;
            // Сбросивший игрок добавляет фишки в банк, но не создаёт новый побочный банк.
            if let Some((_, total)) = contested.last_mut().filter(|(seats, _)| *seats == eligible) {
                *total += amount;
            } else {
                contested.push((eligible, amount));
            }
        }
        for (eligible, amount) in contested {
            let best = eligible.iter().map(|&i| outcome.ranks[i]).max().unwrap();
            let winners: Vec<_> = eligible
                .into_iter()
                .filter(|&i| outcome.ranks[i] == best)
                .collect();
            let count = winners.len() as i64;
            for (position, &i) in winners.iter().enumerate() {
                outcome.awards[i] += amount / count + i64::from((position as i64) < amount % count);
            }
            outcome.pots.push(Pot { amount, winners });
        }
        self.outcome = Some(outcome);
    }

    fn cashouts(&self) -> Vec<i64> {
        let outcome = self.outcome.as_ref().unwrap();
        self.seats
            .iter()
            .enumerate()
            .map(|(i, s)| s.stack + outcome.awards[i] + outcome.returns[i])
            .collect()
    }

    fn bot_move(&self, i: usize, options: &Options) -> Move {
        let [a, b] = self.seats[i].hole.unwrap();
        let strength = if self.board.is_empty() {
            let suited = a.suit() == b.suit();
            match (a.rank(), b.rank()) {
                (a, b) if a == b => {
                    if a >= 10 {
                        4
                    } else {
                        3
                    }
                }
                (a, b) if a.max(b) >= 13 && a.min(b) >= 9 => 3,
                (a, b) if a.max(b) >= 11 || (a.abs_diff(b) <= 2 && suited) => 2,
                _ => 1,
            }
        } else {
            let rank = best_rank([a, b], &self.board);
            match rank.0[0] {
                0 => 1,
                1 => 2,
                2 => 3,
                _ => 4,
            }
        };
        let mut rng = rand::rng();
        if options.may_raise
            && options.min_to <= options.max_to
            && (strength >= 3 && rng.random_bool(0.55)
                || options.owed == 0 && rng.random_bool(0.08))
        {
            return Move::Raise(
                options
                    .max_to
                    .min(options.min_to + (strength as i64 - 1) * SMALL_BLIND),
            );
        }
        if options.owed == 0 {
            Move::Check
        } else if strength >= 3
            || options.call <= BIG_BLIND
            || (strength == 2 && options.call <= self.seats[i].stack / 4)
        {
            Move::Call
        } else {
            Move::Fold
        }
    }

    fn advance(&mut self, now: Instant) {
        while let Some(i) = self.actor.filter(|_| now >= self.deadline) {
            let options = self.options(i);
            let bot = self.seats[i].user.is_none();
            let action = if bot {
                self.bot_move(i, &options)
            } else if options.owed == 0 {
                Move::Check
            } else {
                Move::Fold
            };
            self.apply_move(i, action, self.deadline, !bot)
                .expect("automatic poker move must be legal");
        }
    }

    fn snapshot(&self, user: u64, now: Instant) -> Value {
        let viewer = self.seat_of(user);
        let dealt = self.street.is_some();
        let finished = self.outcome.is_some();
        let cashouts = self.outcome.as_ref().map(|_| self.cashouts());
        let seats: Vec<_> = self
            .seats
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let rank = self.outcome.as_ref().and_then(|o| o.ranks[i]);
                let cards = if viewer == Some(i) || rank.is_some() {
                    s.hole.map(|hole| hole.map(Card::label))
                } else {
                    None
                };
                json!({
                    "id": s.user.map(|id| id.to_string()),
                    "name": s.name, "bot": s.user.is_none(), "cards": cards,
                    "stack": cashouts.as_ref().map_or(s.stack, |chips| chips[i]),
                    "street_bet": s.street, "committed": s.committed,
                    "folded": s.folded, "all_in": dealt && s.stack == 0 && !s.folded,
                    "rank": rank.map(|r| r.0[0]),
                    "won": self.outcome.as_ref().map(|o| o.awards[i]),
                    "returned": self.outcome.as_ref().map(|o| o.returns[i]),
                })
            })
            .collect();
        let actions = self.actor.filter(|&i| viewer == Some(i)).map(|i| {
            let options = self.options(i);
            let raise = (options.may_raise && options.min_to <= options.max_to)
                .then(|| json!({ "min": options.min_to, "max": options.max_to, "opening": self.target == 0 }));
            json!({ "fold": true, "check": options.owed == 0, "call": options.call, "all_in": options.all_in, "raise": raise })
        });
        let pots = self.outcome.as_ref().map(|o| {
            o.pots
                .iter()
                .map(|p| json!({ "amount": p.amount, "winners": p.winners }))
                .collect::<Vec<_>>()
        });
        let pot: i64 = if let Some(outcome) = &self.outcome {
            outcome.pots.iter().map(|p| p.amount).sum()
        } else {
            self.seats.iter().map(|s| s.committed).sum()
        };
        json!({
            "id": self.id.to_string(),
            "status": if finished { "finished" } else if dealt { "playing" } else { "lobby" },
            "street": self.street.map(Street::label),
            "board": self.board.iter().map(|c| c.label()).collect::<Vec<_>>(),
            "pot": pot,
            "seats": seats, "you": viewer, "actor": self.actor, "turn": self.turn,
            "remaining_ms": (!finished).then(|| self.deadline.saturating_duration_since(now).as_millis() as u64),
            "dealer": dealt.then_some(self.dealer),
            "small_blind_seat": dealt.then_some(self.small_blind),
            "big_blind_seat": dealt.then_some(self.big_blind),
            "actions": actions, "pots": pots,
            "last_action": self.last_action.map(|a| json!({
                "seat": a.seat, "kind": a.kind, "amount": a.amount, "timed_out": a.timed_out
            }))
        })
    }

    fn expires_at(&self, now: Instant) -> bool {
        self.outcome
            .as_ref()
            .is_some_and(|o| now >= o.at + RESULT_TIME)
    }
}

fn settle_seats(db: &Database, seats: &[Seat], payouts: &[i64]) -> rusqlite::Result<()> {
    // У каждого места свой билет: можно вернуть один взнос до старта.
    // Все выплаты за раздачу при этом фиксируются одной транзакцией.
    db.transaction(|tx| {
        for (seat, &payout) in seats.iter().zip(payouts) {
            let ticket = seat.ticket.ok_or(rusqlite::Error::InvalidQuery)?;
            wagers::settle_on(tx, ticket, &[(ticket, payout)])?;
        }
        Ok(())
    })
}

fn save_hand(db: &Database, original: &mut Table, updated: Table) -> rusqlite::Result<()> {
    if original.outcome.is_none() && updated.outcome.is_some() {
        settle_seats(db, &updated.seats, &updated.cashouts())?;
    }
    *original = updated;
    Ok(())
}

fn maintain_online(db: &Database, table: &mut Table, now: Instant) -> rusqlite::Result<bool> {
    if table.street.is_none() && now >= table.deadline {
        let refunds: Vec<_> = table.seats.iter().map(|seat| seat.stack).collect();
        settle_seats(db, &table.seats, &refunds)?;
        return Ok(true);
    }
    if table.actor.is_some() && now >= table.deadline {
        let mut updated = table.clone();
        updated.advance(now);
        save_hand(db, table, updated)?;
    }
    Ok(table.expires_at(now))
}

impl Room {
    fn advance(&mut self, db: &Database, now: Instant) -> rusqlite::Result<()> {
        if let Some(table) = self.table.as_mut() {
            if maintain_online(db, table, now)? {
                self.table = None;
            }
        }
        if self
            .last
            .as_ref()
            .is_some_and(|table| table.expires_at(now))
        {
            self.last = None;
        }
        Ok(())
    }
}

pub fn sweep(data: &Data) {
    let mut tables = data.poker.lock();
    let now = Instant::now();
    tables.online.retain(|guild, room| {
        if let Err(error) = room.advance(&data.db, now) {
            eprintln!("Не удалось обновить покер на сервере {guild}: {error}");
        }
        room.table.is_some() || room.last.is_some()
    });
    tables.solo.retain(|_, table| {
        table.advance(now);
        !table.expires_at(now)
    });
}

fn is_current(table: &Table, body: &Value) -> bool {
    body["table_id"]
        .as_str()
        .and_then(|value| value.parse::<u64>().ok())
        == Some(table.id)
}

fn player_move(
    table: &mut Table,
    user: u64,
    action: &str,
    body: &Value,
    now: Instant,
) -> Result<(), String> {
    if !is_current(table, body) || body["turn"].as_u64() != Some(u64::from(table.turn)) {
        return Err("Ход уже изменился. Обновите стол.".into());
    }
    let i = table
        .seat_of(user)
        .ok_or("Вы не участвуете в этой раздаче.")?;
    let action = match action {
        "fold" => Move::Fold,
        "check" => Move::Check,
        "call" => Move::Call,
        "raise" => Move::Raise(
            body["amount"]
                .as_i64()
                .ok_or("Укажите общую сумму ставки на этой улице.")?,
        ),
        "all_in" => Move::AllIn,
        _ => return Err("Неизвестное действие в покере.".into()),
    };
    table.play(i, action, now)
}

fn training(
    tables: &mut Tables,
    guild: u64,
    user: u64,
    name: &str,
    action: &str,
    body: &Value,
    now: Instant,
) -> Result<Value, String> {
    let key = (guild, user);
    if let Some(table) = tables.solo.get_mut(&key) {
        table.advance(now);
    }
    if tables
        .solo
        .get(&key)
        .is_some_and(|table| table.expires_at(now))
    {
        tables.solo.remove(&key);
    }
    if action == "solo_start" {
        let mut stacks = vec![TRAINING_STACK; 2];
        let mut dealer = rand::rng().random_range(0..2);
        if let Some(previous) = tables.solo.get(&key) {
            if previous.outcome.is_none() {
                return Err("Сначала завершите текущую раздачу.".into());
            }
            if !is_current(previous, body) {
                return Err("Раздача уже изменилась.".into());
            }
            let previous_stacks = previous.cashouts();
            if previous_stacks.iter().all(|&chips| chips > 0) {
                stacks = previous_stacks;
            }
            dealer = 1 - previous.dealer;
        }
        let seats = vec![
            Seat::new(Some(user), name.into(), stacks[0], None),
            Seat::new(None, "Бот".into(), stacks[1], None),
        ];
        let mut table = Table::new(rand::random(), seats, now);
        table.deal(dealer, now);
        tables.solo.insert(key, table);
    } else if action != "state" {
        let table = tables
            .solo
            .get_mut(&key)
            .ok_or("Тренировочная раздача ещё не началась.")?;
        player_move(table, user, action, body, now)?;
    }
    Ok(json!({
        "mode": "solo", "small_blind": SMALL_BLIND, "big_blind": BIG_BLIND,
        "training_stack": TRAINING_STACK, "table": tables.solo.get(&key).map(|table| table.snapshot(user, now)),
    }))
}

struct Online<'a> {
    data: &'a Data,
    guild: u64,
    user: u64,
    name: &'a str,
    now: Instant,
}

impl Online<'_> {
    fn minimum(&self) -> i64 {
        self.data.economy.min_bet.max(BIG_BLIND)
    }

    fn join(&self, room: &mut Room, body: &Value) -> rusqlite::Result<Result<(), String>> {
        if let Some(table) = &room.table {
            if table.street.is_some() && table.outcome.is_none() {
                return Ok(Err("За столом уже идёт раздача.".into()));
            }
            if table.street.is_none() {
                if table.seat_of(self.user).is_some() {
                    return Ok(Err("Ваше место уже занято.".into()));
                }
                if table.seats.len() >= MAX_SEATS {
                    return Ok(Err("За столом нет свободных мест.".into()));
                }
            }
        }
        if !body["table_id"].is_null()
            && !room
                .table
                .as_ref()
                .is_some_and(|table| is_current(table, body))
        {
            return Ok(Err("Стол уже изменился. Попробуйте снова.".into()));
        }
        let Some(bet) = body["bet"]
            .as_str()
            .filter(|bet| !bet.is_empty() && bet.len() <= 32)
        else {
            return Ok(Err("Укажите взнос за место.".into()));
        };
        let db = &self.data.db;
        db.wallet(
            self.guild,
            self.user,
            self.name,
            self.data.economy.start_balance,
        )?;
        let ticket = rand::random();
        let stack = match db.transaction(|tx| {
            wagers::place_on(
                tx,
                ticket,
                ticket,
                self.guild,
                self.user,
                bet,
                self.minimum(),
            )
        })? {
            Ok(stack) => stack,
            Err(error) => return Ok(Err(error)),
        };
        if room.table.as_ref().is_some_and(|t| t.outcome.is_some()) {
            room.last = room.table.take();
        }
        let table = room
            .table
            .get_or_insert_with(|| Table::new(rand::random(), Vec::new(), self.now));
        table.seats.push(Seat::new(
            Some(self.user),
            self.name.into(),
            stack,
            Some(ticket),
        ));
        Ok(Ok(()))
    }

    fn update(
        &self,
        room: &mut Room,
        action: &str,
        body: &Value,
    ) -> rusqlite::Result<Result<(), String>> {
        if action == "state" {
            return Ok(Ok(()));
        }
        if action == "join" {
            return self.join(room, body);
        }
        let Some(table) = room.table.as_mut().filter(|table| is_current(table, body)) else {
            return Ok(Err("Стол уже изменился. Попробуйте снова.".into()));
        };
        match action {
            "start" => {
                if table.street.is_some()
                    || table.seat_of(self.user) != Some(0)
                    || table.seats.len() < 2
                {
                    return Ok(Err(
                        "Раздачу начинает создатель стола, когда есть хотя бы два игрока.".into(),
                    ));
                }
                let mut next = table.clone();
                next.deal(rand::rng().random_range(0..next.seats.len()), self.now);
                save_hand(&self.data.db, table, next)?;
            }
            "leave" => {
                if table.street.is_some() {
                    return Ok(Err(
                        "Во время раздачи можно сбросить карты в свой ход.".into()
                    ));
                }
                let Some(i) = table.seat_of(self.user) else {
                    return Ok(Err("У вас нет места за этим столом.".into()));
                };
                let seat = &table.seats[i];
                settle_seats(&self.data.db, std::slice::from_ref(seat), &[seat.stack])?;
                table.seats.remove(i);
                if table.seats.is_empty() {
                    room.table = None;
                }
            }
            _ => {
                let mut next = table.clone();
                if let Err(error) = player_move(&mut next, self.user, action, body, self.now) {
                    return Ok(Err(error));
                }
                save_hand(&self.data.db, table, next)?;
            }
        }
        Ok(Ok(()))
    }

    fn snapshot(&self, room: &Room) -> rusqlite::Result<Value> {
        let balance: i64 = self
            .data
            .db
            .lock()
            .query_row(
                "SELECT balance FROM economy WHERE guild_id = ?1 AND user_id = ?2",
                [self.guild as i64, self.user as i64],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(self.data.economy.start_balance);
        Ok(json!({
            "mode": "online", "balance": balance, "small_blind": SMALL_BLIND, "big_blind": BIG_BLIND,
            "min_buy_in": self.minimum(), "max_buy_in": MAX_BET, "max_seats": MAX_SEATS,
            "table": room.table.as_ref().map(|t| t.snapshot(self.user, self.now)),
            "previous": room.last.as_ref().filter(|t| t.seat_of(self.user).is_some()).map(|t| t.snapshot(self.user, self.now))
        }))
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
    let mut tables = data.poker.lock();
    let now = Instant::now();
    match body["mode"].as_str() {
        Some("solo") => Ok(training(&mut tables, guild, user, name, action, body, now)),
        None | Some("online") => {
            let online = Online {
                data,
                guild,
                user,
                name,
                now,
            };
            let mut empty = Room::default();
            let room = if action == "join" {
                tables.online.entry(guild).or_default()
            } else {
                tables.online.get_mut(&guild).unwrap_or(&mut empty)
            };
            room.advance(&data.db, now)?;
            if let Err(error) = online.update(room, action, body)? {
                return Ok(Err(error));
            }
            Ok(Ok(online.snapshot(room)?))
        }
        Some(_) => Ok(Err("Неизвестный режим покера.".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> Data {
        Data::new(
            Database::in_memory().unwrap(),
            &crate::config::Settings::default(),
            Default::default(),
            false,
        )
    }

    fn card(rank: u8, suit: u8) -> Card {
        Card(suit * 13 + rank - 2)
    }

    fn seat(user: u64, stack: i64) -> Seat {
        Seat::new(Some(user), format!("Игрок {user}"), stack, None)
    }

    fn table(stacks: &[i64]) -> Table {
        let now = Instant::now();
        let seats = stacks
            .iter()
            .enumerate()
            .map(|(i, &stack)| seat(i as u64 + 1, stack))
            .collect();
        let mut table = Table::new(1, seats, now);
        table.deal(0, now);
        table
    }

    #[test]
    fn five_card_categories_and_wheel_rank_in_order() {
        let hands = [
            [(14, 0), (13, 1), (10, 2), (6, 3), (2, 0)],
            [(7, 0), (7, 1), (14, 2), (6, 3), (2, 0)],
            [(7, 0), (7, 1), (6, 2), (6, 3), (2, 0)],
            [(7, 0), (7, 1), (7, 2), (6, 3), (2, 0)],
            [(14, 0), (5, 1), (4, 2), (3, 3), (2, 0)],
            [(14, 0), (13, 0), (10, 0), (6, 0), (2, 0)],
            [(7, 0), (7, 1), (7, 2), (6, 3), (6, 0)],
            [(7, 0), (7, 1), (7, 2), (7, 3), (2, 0)],
            [(14, 0), (5, 0), (4, 0), (3, 0), (2, 0)],
        ];
        let ranks = hands.map(|hand| rank_five(hand.map(|(rank, suit)| card(rank, suit))));
        assert_eq!(ranks.map(|rank| rank.0[0]), [0, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(ranks[4].0[1], 5);
        assert_eq!(ranks[8].0[1], 5);
    }

    #[test]
    fn best_of_seven_handles_two_trips_and_pair_kickers() {
        let board = [card(4, 0), card(4, 1), card(4, 2), card(9, 0), card(9, 1)];
        assert_eq!(
            best_rank([card(9, 2), card(2, 0)], &board),
            Rank([6, 9, 4, 0, 0, 0])
        );
        let board = [card(4, 0), card(4, 1), card(8, 2), card(9, 0), card(11, 1)];
        assert!(
            best_rank([card(14, 0), card(2, 1)], &board)
                > best_rank([card(13, 0), card(12, 1)], &board)
        );
    }

    #[test]
    fn heads_up_blinds_and_all_four_betting_streets() {
        let now = Instant::now();
        let mut game = table(&[100, 100]);
        assert_eq!(game.actor, Some(0));
        assert_eq!(
            game.seats.iter().map(|s| s.street).collect::<Vec<_>>(),
            [5, 10]
        );
        assert!(game.play(1, Move::Check, now).is_err());
        game.play(0, Move::Call, now).unwrap();
        assert_eq!(game.actor, Some(1));
        game.play(1, Move::Check, now).unwrap();
        for cards in 3..=5 {
            assert_eq!(game.board.len(), cards);
            assert_eq!(game.actor, Some(1));
            game.play(1, Move::Check, now).unwrap();
            game.play(0, Move::Check, now).unwrap();
        }
        assert!(game.outcome.is_some());
        assert_eq!(game.cashouts().iter().sum::<i64>(), 200);
    }

    #[test]
    fn short_all_ins_reopen_only_after_a_full_cumulative_raise() {
        let now = Instant::now();
        let mut short = table(&[100, 15, 100, 100]);
        short.play(3, Move::Call, now).unwrap();
        short.play(0, Move::Call, now).unwrap();
        short.play(1, Move::AllIn, now).unwrap();
        short.play(2, Move::Call, now).unwrap();
        assert!(short.play(3, Move::Raise(25), now).is_err());
        assert!(short.play(3, Move::AllIn, now).is_err());
        short.play(3, Move::Call, now).unwrap();

        let mut cumulative = table(&[100, 15, 20, 100]);
        cumulative.play(3, Move::Call, now).unwrap();
        cumulative.play(0, Move::Call, now).unwrap();
        cumulative.play(1, Move::AllIn, now).unwrap();
        cumulative.play(2, Move::AllIn, now).unwrap();
        assert!(cumulative.play(3, Move::Raise(29), now).is_err());
        cumulative.play(3, Move::Raise(30), now).unwrap();
    }

    #[test]
    fn all_ins_run_out_and_award_main_side_and_uncalled_chips_separately() {
        let now = Instant::now();
        let mut game = table(&[100, 40, 60]);
        game.seats[0].hole = Some([card(9, 0), card(9, 1)]);
        game.seats[1].hole = Some([card(14, 0), card(14, 1)]);
        game.seats[2].hole = Some([card(13, 0), card(13, 1)]);
        game.deck = [
            card(10, 1),
            card(6, 2),
            card(11, 1),
            card(8, 3),
            card(7, 2),
            card(4, 3),
            card(2, 2),
            card(12, 1),
        ]
        .into_iter()
        .collect();
        game.play(0, Move::AllIn, now).unwrap();
        game.play(1, Move::AllIn, now).unwrap();
        game.play(2, Move::Call, now).unwrap();
        let outcome = game.outcome.as_ref().unwrap();
        assert_eq!(game.board.len(), 5);
        assert_eq!(outcome.awards, [0, 120, 40]);
        assert_eq!(outcome.returns, [40, 0, 0]);
        assert_eq!(game.cashouts(), [40, 120, 40]);
    }

    #[test]
    fn ties_split_separate_pots_and_odd_chip_starts_left_of_the_button() {
        let mut game = table(&[100, 100, 100, 100]);
        game.board = (10..=14).map(|rank| card(rank, 0)).collect();
        for (seat, put) in game.seats.iter_mut().zip([11, 11, 5, 11]) {
            seat.stack = 100 - put;
            seat.committed = put;
        }
        game.seats[3].folded = true;
        game.finish(Instant::now());
        assert_eq!(game.outcome.as_ref().unwrap().awards, [15, 16, 7, 0]);
        assert_eq!(game.cashouts().iter().sum::<i64>(), 400);
    }

    #[test]
    fn folded_chips_do_not_create_extra_split_pots() {
        let mut game = table(&[100; 5]);
        game.board = (10..=14).map(|rank| card(rank, 0)).collect();
        for (seat, put) in game.seats.iter_mut().zip([10, 10, 10, 1, 2]) {
            seat.stack = 100 - put;
            seat.committed = put;
        }
        game.seats[3].folded = true;
        game.seats[4].folded = true;
        game.finish(Instant::now());
        let outcome = game.outcome.unwrap();
        assert_eq!(outcome.awards, [11, 11, 11, 0, 0]);
        assert_eq!(outcome.pots.len(), 1);
        assert_eq!(outcome.pots[0].amount, 33);
    }

    #[test]
    fn ten_seats_use_unique_cards_and_force_preflop_action_after_the_big_blind() {
        let now = Instant::now();
        let mut game = table(&[100; 10]);
        assert_eq!(
            (game.small_blind, game.big_blind, game.actor),
            (1, 2, Some(3))
        );
        while let Some(actor) = game.actor {
            let action = if game.options(actor).owed > 0 {
                Move::Call
            } else {
                Move::Check
            };
            game.play(actor, action, now).unwrap();
        }
        let dealt = game
            .seats
            .iter()
            .flat_map(|seat| seat.hole.unwrap())
            .chain(game.board.iter().copied())
            .map(|card| card.0)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(dealt.len(), 25);
        assert_eq!(game.deck.len(), 24);
        assert_eq!(game.cashouts().iter().sum::<i64>(), 1000);
    }

    #[test]
    fn snapshot_keeps_discord_ids_as_strings_and_hides_folded_hole_cards() {
        let now = Instant::now();
        let mut game = table(&[100, 100, 100]);
        let user = 9_234_567_890_123_456_789;
        game.seats[0].user = Some(user);
        let own = game.snapshot(user, now);
        assert_eq!(own["seats"][0]["id"], user.to_string());
        assert_eq!(own["seats"][0]["cards"].as_array().unwrap().len(), 2);
        assert!(own["seats"][1]["cards"].is_null());
        game.play(0, Move::Fold, now).unwrap();
        game.play(1, Move::AllIn, now).unwrap();
        game.play(2, Move::Call, now).unwrap();
        let spectator = game.snapshot(999, now);
        assert!(spectator["seats"][0]["cards"].is_null());
        assert_eq!(spectator["seats"][1]["cards"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn expired_lobby_refunds_and_timed_out_hand_settles_exactly_once() {
        let db = Database::in_memory().unwrap();
        let now = Instant::now();
        for user in [11, 12] {
            db.wallet(42, user, "Игрок", 100).unwrap();
        }
        let mut lobby = Table::new(8, Vec::new(), now);
        for user in [11, 12] {
            let ticket = user + 100;
            db.transaction(|tx| wagers::place_on(tx, ticket, ticket, 42, user, "100", 10))
                .unwrap()
                .unwrap();
            lobby
                .seats
                .push(Seat::new(Some(user), "Игрок".into(), 100, Some(ticket)));
        }
        let balance = |user| db.wallet(42, user, "Игрок", 100).unwrap().0;
        assert_eq!(balance(11), 0);
        assert!(maintain_online(&db, &mut lobby, now + LOBBY_TIME).unwrap());
        assert_eq!([balance(11), balance(12)], [100, 100]);

        for seat in &lobby.seats {
            let ticket = seat.ticket.unwrap();
            db.transaction(|tx| {
                wagers::place_on(tx, ticket, ticket, 42, seat.user.unwrap(), "100", 10)
            })
            .unwrap()
            .unwrap();
        }
        lobby.deal(0, now);
        assert!(!maintain_online(&db, &mut lobby, now + TURN_TIME).unwrap());
        assert!(!maintain_online(&db, &mut lobby, now + TURN_TIME).unwrap());
        assert_eq!([balance(11), balance(12)], [95, 105]);
        assert_eq!(wagers::refund(&db, None).unwrap(), 0);
    }

    #[test]
    fn online_api_is_guild_scoped_and_rejects_replayed_turns() {
        let data = data();
        let guild = 11;
        let first = 9_234_567_890_123_456_789;
        let second = first + 1;
        let call =
            |guild, user, action, body| handle(&data, guild, user, "Игрок", action, &body).unwrap();
        let lobby = call(guild, first, "join", json!({"bet": "100"})).unwrap();
        assert_eq!(lobby["balance"], 400);
        assert!(call(guild + 1, first, "state", json!({})).unwrap()["table"].is_null());
        assert!(call(guild, first, "join", json!({"bet": "100"})).is_err());
        let table_id = lobby["table"]["id"].as_str().unwrap();
        let with_id = json!({"table_id": table_id});
        assert!(call(guild, first, "start", with_id.clone()).is_err());
        call(
            guild,
            second,
            "join",
            json!({"table_id": table_id, "bet": "100"}),
        )
        .unwrap();
        assert!(call(guild, second, "start", with_id.clone()).is_err());
        let started = call(guild, first, "start", with_id).unwrap();
        let actor = started["table"]["actor"].as_u64().unwrap() as usize;
        let users = [first, second];
        let actor_view = call(guild, users[actor], "state", json!({})).unwrap();
        let hand = &actor_view["table"];
        assert_eq!(hand["id"], table_id);
        assert_eq!(hand["seats"][actor]["id"], users[actor].to_string());
        assert_eq!(hand["seats"][actor]["cards"].as_array().unwrap().len(), 2);
        assert!(hand["seats"][1 - actor]["cards"].is_null());
        assert!(call(
            guild,
            users[actor],
            "fold",
            json!({"table_id": table_id, "turn":0})
        )
        .is_err());
        let legal = json!({"table_id": table_id, "turn":hand["turn"]});
        let final_state = call(guild, users[actor], "fold", legal.clone()).unwrap();
        assert_eq!(final_state["table"]["status"], "finished");
        assert_eq!(final_state["balance"], 495);
        assert!(call(guild, users[actor], "fold", legal).is_err());
        assert_eq!(
            call(guild, users[1 - actor], "state", json!({})).unwrap()["balance"],
            505
        );
        assert_eq!(wagers::refund(&data.db, None).unwrap(), 0);
    }

    #[test]
    fn private_training_advances_bot_and_keeps_wallet_untouched() {
        let data = data();
        let (guild, user) = (18, 19);
        let solo = |user, action, body| {
            handle(&data, guild, user, "Игрок", action, &body)
                .unwrap()
                .unwrap()
        };
        let started = solo(user, "solo_start", json!({"mode":"solo"}));
        assert_eq!(started["table"]["seats"][1]["bot"], true);
        assert!(solo(user + 1, "state", json!({"mode":"solo"}))["table"].is_null());
        {
            let mut games = data.poker.lock();
            let table = games.solo.get_mut(&(guild, user)).unwrap();
            if table.actor == Some(1) {
                table.advance(table.deadline);
            }
        }
        let state = solo(user, "state", json!({"mode":"solo"}));
        let hand = &state["table"];
        assert_eq!(hand["actor"], 0);
        let id = hand["id"].as_str().unwrap();
        let result = solo(
            user,
            "fold",
            json!({"mode":"solo", "table_id":id, "turn":hand["turn"]}),
        );
        assert_eq!(result["table"]["status"], "finished");
        let next = solo(user, "solo_start", json!({"mode":"solo", "table_id":id}));
        for (i, seat) in next["table"]["seats"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            assert_eq!(
                seat["stack"].as_i64().unwrap() + seat["committed"].as_i64().unwrap(),
                result["table"]["seats"][i]["stack"].as_i64().unwrap()
            );
        }
        assert_ne!(next["table"]["dealer"], result["table"]["dealer"]);
        let wallets: i64 = data
            .db
            .lock()
            .query_row("SELECT COUNT(*) FROM economy", [], |row| row.get(0))
            .unwrap();
        assert_eq!(wallets, 0);
        assert_eq!(wagers::refund(&data.db, None).unwrap(), 0);
    }
}
