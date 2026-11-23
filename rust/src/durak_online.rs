//! Guild-local multiplayer Durak. Room mutations and snapshots share one lock.

use crate::durak::{Card, DECK_SIZE, FULL_DECK_SIZE, HAND_SIZE};
use rand::{Rng, seq::SliceRandom};
use serde_json::{Value, json};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

const MAX_ROOMS: usize = 256;
const INACTIVE_TTL: Duration = Duration::from_secs(30 * 60);
const FINISHED_TTL: Duration = Duration::from_secs(10 * 60);
const REACTION_TTL: Duration = Duration::from_secs(5);
const REACTION_COOLDOWN: Duration = Duration::from_secs(2);
const REACTIONS: [&str; 6] = ["👍", "😂", "😮", "👏", "😎", "😤"];
const ROOM_MISSING: &str = "Комната не найдена.";
const ILLEGAL: &str = "Этот ход сейчас недоступен.";

/// Each callback commits atomically. An error leaves the round available for retry.
pub(crate) trait Bank {
    fn reserve(
        &self,
        round: u64,
        guild: u64,
        players: &[(u64, &str)],
        stake: i64,
    ) -> Result<(), String>;
    /// With no loser, return each player's stake.
    fn settle(&self, round: u64, loser: Option<u64>) -> Result<(), String>;
}

pub struct State {
    store: Mutex<Store>,
    epoch: u64,
}

#[derive(Default)]
struct Store {
    sequence: u64,
    rooms: Vec<Room>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            store: Mutex::default(),
            epoch: rand::random(),
        }
    }
}

impl State {
    pub(crate) fn handle_with_bank(
        &self,
        bank: &impl Bank,
        guild: u64,
        user: u64,
        name: &str,
        action: &str,
        body: &Value,
    ) -> Result<Value, String> {
        let mut store = self.store.lock().map_err(|_| "Игра недоступна.")?;
        let now = Instant::now();
        store
            .rooms
            .retain(|room| room.is_playing() || !room.expired(now));
        let mut current = store
            .rooms
            .iter()
            .position(|room| room.guild == guild && room.member(user).is_some());
        if let Some(index) = current {
            match store.rooms[index].maintain(bank, now) {
                Ok(false) => {
                    store.rooms.remove(index);
                    current = None;
                }
                Ok(true) if store.rooms[index].member(user).is_none() => current = None,
                Err(error) if action != "online_state" => return Err(error),
                _ => {}
            }
        }

        match action {
            "online_state" => {}
            "online_create" => {
                if current.is_some() {
                    return Err("Сначала выйдите из текущей комнаты.".into());
                }
                let mode = Mode::parse(&body["mode"])?;
                let options = Options::parse(body)?;
                let max_players = if options.deck == 24 { 4 } else { 6 };
                let capacity = body["capacity"]
                    .as_u64()
                    .filter(|capacity| (2..=max_players).contains(capacity))
                    .ok_or_else(|| format!("В комнате может быть от 2 до {max_players} игроков."))?
                    as usize;
                if store.rooms.len() >= MAX_ROOMS {
                    return Err("Достигнут лимит комнат. Попробуйте позже.".into());
                }
                store.sequence = store.sequence.checked_add(1).ok_or("Игра недоступна.")?;
                let id = format!("{:016x}-{:016x}", self.epoch, store.sequence);
                current = Some(store.rooms.len());
                store.rooms.push(Room {
                    id,
                    guild,
                    round: rand::random(),
                    mode,
                    options,
                    capacity,
                    players: vec![Player::new(user, name)],
                    game: None,
                    changed: now,
                    pending: None,
                });
            }
            "online_join" => {
                let id = room_id(body)?;
                let index = store
                    .rooms
                    .iter()
                    .position(|room| room.guild == guild && room.id == id)
                    .ok_or(ROOM_MISSING)?;
                if current != Some(index) {
                    if current.is_some() {
                        return Err("Сначала выйдите из текущей комнаты.".into());
                    }
                    let room = &mut store.rooms[index];
                    if !room.joinable() {
                        return Err("В эту комнату уже нельзя войти.".into());
                    }
                    room.players.push(Player::new(user, name));
                    room.changed = now;
                    current = Some(index);
                }
            }
            "online_start" | "online_leave" | "online_move" | "online_react" => {
                let id = room_id(body)?;
                let index = current
                    .filter(|&index| store.rooms[index].id == id)
                    .ok_or(ROOM_MISSING)?;
                let room = &mut store.rooms[index];
                let seat = room.member(user).expect("current room member");
                match action {
                    "online_start" => room.start(bank, seat)?,
                    "online_move" => {
                        let mut next = room.clone();
                        let played = Instant::now();
                        next.play(seat, body, played)?;
                        next.changed = played;
                        room.commit(bank, next)?;
                    }
                    "online_leave" => {
                        let mut next = room.clone();
                        next.leave(seat);
                        next.changed = Instant::now();
                        if !room.commit(bank, next)? {
                            store.rooms.remove(index);
                        }
                        current = None;
                    }
                    "online_react" => room.players[seat].react(body, now)?,
                    _ => unreachable!(),
                }
            }
            _ => return Err("Неизвестный запрос игры.".into()),
        }

        let rooms: Vec<_> = store
            .rooms
            .iter()
            .filter(|room| room.guild == guild && room.joinable())
            .map(Room::summary)
            .collect();
        let now = Instant::now();
        Ok(json!({
            "you": user.to_string(),
            "rooms": rooms,
            "room": current.map(|index| store.rooms[index].detail(user, now)),
        }))
    }

    pub(crate) fn sweep(&self, bank: &impl Bank) -> Result<(), String> {
        let mut store = self.store.lock().map_err(|_| "Игра недоступна.")?;
        let now = Instant::now();
        let mut result = Ok(());
        store
            .rooms
            .retain_mut(|room| match room.maintain(bank, now) {
                Ok(keep) => keep,
                Err(error) => {
                    if result.is_ok() {
                        result = Err(error);
                    }
                    true
                }
            });
        result
    }

    pub(crate) fn shared_room(&self, guild: u64, viewer: u64, target: u64, room_id: &str) -> bool {
        let Ok(store) = self.store.lock() else {
            return false;
        };
        let now = Instant::now();
        store.rooms.iter().any(|room| {
            room.guild == guild
                && room.id == room_id
                && !room.expired(now)
                && room.member(viewer).is_some()
                && room.member(target).is_some()
        })
    }

    #[cfg(test)]
    fn handle(
        &self,
        guild: u64,
        user: u64,
        name: &str,
        action: &str,
        body: &Value,
    ) -> Result<Value, String> {
        struct FreeBank;
        impl Bank for FreeBank {
            fn reserve(&self, _: u64, _: u64, _: &[(u64, &str)], _: i64) -> Result<(), String> {
                panic!("unexpected reservation in free game");
            }

            fn settle(&self, _: u64, _: Option<u64>) -> Result<(), String> {
                panic!("unexpected settlement in free game");
            }
        }
        self.handle_with_bank(&FreeBank, guild, user, name, action, body)
    }
}

fn room_id(body: &Value) -> Result<&str, &str> {
    body["room_id"].as_str().ok_or(ROOM_MISSING)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Throw,
    Transfer,
}

impl Mode {
    fn parse(value: &Value) -> Result<Self, &'static str> {
        match value.as_str() {
            Some("throw") => Ok(Self::Throw),
            Some("transfer") => Ok(Self::Transfer),
            _ => Err("Выберите подкидного или переводного дурака."),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Throw => "throw",
            Self::Transfer => "transfer",
        }
    }
}

#[derive(Clone, Copy)]
struct Options {
    deck: u8,
    cheating: bool,
    throwers: Throwers,
    speed: Speed,
    stake: i64,
}

impl Options {
    fn parse(body: &Value) -> Result<Self, &'static str> {
        let deck = match body.get("deck").map(Value::as_u64) {
            None => DECK_SIZE,
            Some(Some(deck @ (24 | 36 | 52))) => deck as u8,
            _ => return Err("Выберите колоду на 24, 36 или 52 карты."),
        };
        let cheating = body
            .get("cheating")
            .map_or(Some(false), Value::as_bool)
            .ok_or("Неверное значение режима обмана.")?;
        let throwers = match body.get("throwers").map(Value::as_str) {
            None | Some(Some("all")) => Throwers::All,
            Some(Some("neighbors")) => Throwers::Neighbors,
            _ => return Err("Выберите, кто может подкидывать карты."),
        };
        let speed = match body.get("speed").map(Value::as_str) {
            None | Some(Some("slow")) => Speed::Slow,
            Some(Some("fast")) => Speed::Fast,
            _ => return Err("Выберите темп игры."),
        };
        let stake = body
            .get("stake")
            .map_or(Some(0), Value::as_i64)
            .filter(|stake| matches!(stake, 0 | 10..=1_000_000))
            .ok_or("Ставка должна быть 0 или от 10 до 1 000 000.")?;
        Ok(Self {
            deck,
            cheating,
            throwers,
            speed,
            stake,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Throwers {
    All,
    Neighbors,
}

impl Throwers {
    fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Neighbors => "neighbors",
        }
    }
}

#[derive(Clone, Copy)]
enum Speed {
    Slow,
    Fast,
}

impl Speed {
    fn name(self) -> &'static str {
        match self {
            Self::Slow => "slow",
            Self::Fast => "fast",
        }
    }

    fn turn_time(self) -> Duration {
        Duration::from_secs(match self {
            Self::Slow => 45,
            Self::Fast => 15,
        })
    }
}

#[derive(Clone)]
struct Player {
    id: u64,
    name: String,
    departed: bool,
    reaction: Option<(&'static str, Instant)>,
}

impl Player {
    fn new(id: u64, name: &str) -> Self {
        Self {
            id,
            name: name.to_owned(),
            departed: false,
            reaction: None,
        }
    }

    fn react(&mut self, body: &Value, now: Instant) -> Result<(), &'static str> {
        let emoji = REACTIONS
            .into_iter()
            .find(|emoji| body["emoji"].as_str() == Some(*emoji))
            .ok_or("Неизвестная реакция.")?;
        if self
            .reaction
            .is_some_and(|(_, sent)| now.duration_since(sent) < REACTION_COOLDOWN)
        {
            return Err("Подождите перед следующей реакцией.");
        }
        self.reaction = Some((emoji, now));
        Ok(())
    }
}

#[derive(Clone)]
struct Room {
    id: String,
    guild: u64,
    round: u64,
    mode: Mode,
    options: Options,
    capacity: usize,
    players: Vec<Player>,
    game: Option<Game>,
    changed: Instant,
    pending: Option<Settlement>,
}

#[derive(Clone)]
struct Settlement {
    loser: Option<u64>,
    next: Option<Box<Room>>,
}

impl Room {
    fn is_playing(&self) -> bool {
        self.game.as_ref().and_then(Game::turn).is_some()
    }

    fn expired(&self, now: Instant) -> bool {
        let ttl = match self.game.as_ref().map(|game| &game.phase) {
            Some(Phase::Finished { .. }) => FINISHED_TTL,
            _ => INACTIVE_TTL,
        };
        now.duration_since(self.changed) >= ttl
    }

    fn member(&self, user: u64) -> Option<usize> {
        self.players
            .iter()
            .position(|player| player.id == user && !player.departed)
    }

    fn host(&self) -> Option<&Player> {
        self.players.iter().find(|player| !player.departed)
    }

    fn joinable(&self) -> bool {
        self.game.is_none() && self.players.len() < self.capacity
    }

    fn start(&mut self, bank: &impl Bank, seat: usize) -> Result<(), String> {
        if seat != 0 || self.players.len() < 2 || self.game.is_some() {
            return Err("Только хозяин комнаты может начать игру с двумя и более игроками.".into());
        }
        if self.options.stake > 0 {
            let players: Vec<_> = self
                .players
                .iter()
                .map(|player| (player.id, player.name.as_str()))
                .collect();
            bank.reserve(self.round, self.guild, &players, self.options.stake)?;
        }
        self.changed = Instant::now();
        self.game = Some(Game::new(self.players.len(), self.options, self.changed));
        Ok(())
    }

    fn maintain(&mut self, bank: &impl Bank, now: Instant) -> Result<bool, String> {
        if self.pending.is_some() {
            return self.settle_pending(bank);
        }
        if self.expired(now) {
            if self.options.stake > 0 && self.is_playing() {
                self.pending = Some(Settlement {
                    loser: None,
                    next: None,
                });
                return self.settle_pending(bank);
            }
            return Ok(false);
        }
        if let Some(seat) = self
            .game
            .as_ref()
            .filter(|game| now >= game.deadline)
            .and_then(Game::turn)
        {
            let mut next = self.clone();
            next.forfeit(seat, "не успел сделать ход и проиграл.");
            next.changed = now;
            return self.commit(bank, next);
        }
        Ok(true)
    }

    fn commit(&mut self, bank: &impl Bank, next: Self) -> Result<bool, String> {
        if self.options.stake > 0 && self.is_playing() {
            if let Some(Phase::Finished { loser, .. }) = next.game.as_ref().map(|game| &game.phase)
            {
                let loser = loser.map(|seat| next.players[seat].id);
                // Preserve both the live state and the original outcome across bank failures.
                self.pending = Some(Settlement {
                    loser,
                    next: Some(Box::new(next)),
                });
                return self.settle_pending(bank);
            }
        }
        *self = next;
        Ok(self.host().is_some())
    }

    fn settle_pending(&mut self, bank: &impl Bank) -> Result<bool, String> {
        let pending = self.pending.as_ref().expect("pending settlement");
        bank.settle(self.round, pending.loser)?;
        let pending = self.pending.take().expect("settled outcome");
        let Some(mut next) = pending.next else {
            return Ok(false);
        };
        next.changed = Instant::now();
        *self = *next;
        Ok(self.host().is_some())
    }

    fn summary(&self) -> Value {
        self.view(None)
    }

    fn view(&self, reaction_time: Option<Instant>) -> Value {
        let players: Vec<_> = self
            .players
            .iter()
            .enumerate()
            .filter(|(_, player)| !player.departed)
            .map(|(seat, player)| {
                let mut view = json!({"id": player.id.to_string(), "name": player.name});
                if let Some(game) = &self.game {
                    view["cards"] = json!(game.hands[seat].len());
                }
                if let (Some(now), Some((emoji, sent))) = (reaction_time, player.reaction) {
                    if now.duration_since(sent) < REACTION_TTL {
                        view["reaction"] = json!(emoji);
                    }
                }
                view
            })
            .collect();
        json!({
            "id": self.id,
            "mode": self.mode.name(),
            "deck": self.options.deck,
            "cheating": self.options.cheating,
            "throwers": self.options.throwers.name(),
            "speed": self.options.speed.name(),
            "stake": self.options.stake,
            "capacity": self.capacity,
            "host": self.host().expect("occupied room").id.to_string(),
            "players": players,
        })
    }

    fn detail(&self, user: u64, now: Instant) -> Value {
        let seat = self.member(user).expect("current room member");
        let mut detail = self.view(Some(now));
        detail["status"] = json!(match self.game.as_ref().map(|game| &game.phase) {
            None => "waiting",
            Some(Phase::Finished { .. }) => "finished",
            _ => "playing",
        });
        detail["game"] = self
            .game
            .as_ref()
            .map_or(Value::Null, |game| game.view(self, seat, now));
        detail
    }

    fn revision(&self, game: &Game) -> String {
        format!("{}-{}", self.id, game.version)
    }

    fn play(&mut self, seat: usize, body: &Value, now: Instant) -> Result<(), String> {
        let game = self.game.as_ref().ok_or(ILLEGAL)?;
        if body["revision"].as_str() != Some(self.revision(game).as_str()) {
            return Err("Игра уже изменилась. Обновите её состояние.".into());
        }
        let movement = Move::parse(body).ok_or(ILLEGAL)?;
        if !game
            .legal_moves(self.mode, self.options.cheating, seat)
            .contains(&movement)
        {
            return Err(ILLEGAL.into());
        }
        let game = self.game.as_mut().expect("game checked");
        let version = game.version.checked_add(1).ok_or("Игра недоступна.")?;
        game.play(seat, movement, &self.players, self.options.throwers);
        game.version = version;
        game.deadline = now + self.options.speed.turn_time();
        Ok(())
    }

    fn forfeit(&mut self, seat: usize, reason: &str) {
        if let Some(game) = &mut self.game {
            if game.turn().is_some() {
                game.phase = Phase::Finished {
                    message: format!("{} {reason}", self.players[seat].name),
                    loser: Some(seat),
                };
                game.version = game.version.saturating_add(1);
            }
        }
    }

    fn leave(&mut self, seat: usize) {
        if self.game.is_some() {
            self.players[seat].departed = true;
            self.forfeit(seat, "покинул игру и проиграл.");
        } else {
            self.players.remove(seat);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Move {
    Attack(Card),
    Defend(Card),
    Transfer(Card),
    Cheat(Card),
    Take,
    Pass,
}

impl Move {
    fn parse(body: &Value) -> Option<Self> {
        let card = || {
            body["card"]
                .as_u64()
                .filter(|id| (1..=u64::from(FULL_DECK_SIZE)).contains(id))
                .map(|id| Card(id as u8))
        };
        match body["action"].as_str()? {
            "attack" => Some(Self::Attack(card()?)),
            "defend" => Some(Self::Defend(card()?)),
            "transfer" => Some(Self::Transfer(card()?)),
            "cheat" => Some(Self::Cheat(card()?)),
            "take" => Some(Self::Take),
            "pass" => Some(Self::Pass),
            _ => None,
        }
    }

    fn view(self) -> (&'static str, Option<Card>) {
        match self {
            Self::Attack(card) => ("attack", Some(card)),
            Self::Defend(card) => ("defend", Some(card)),
            Self::Transfer(card) => ("transfer", Some(card)),
            Self::Cheat(card) => ("cheat", Some(card)),
            Self::Take => ("take", None),
            Self::Pass => ("pass", None),
        }
    }
}

#[derive(Clone)]
enum Phase {
    Open,
    Defend,
    Throw {
        offset: usize,
        taking: bool,
    },
    Finished {
        message: String,
        loser: Option<usize>,
    },
}

#[derive(Clone)]
struct Pair {
    attack: Card,
    defense: Option<Card>,
}

#[derive(Clone)]
struct Game {
    version: u64,
    hands: Vec<Vec<Card>>,
    deck: Vec<Card>,
    trump: Card,
    table: Vec<Pair>,
    opener: usize,
    attacker: usize,
    defender: usize,
    limit: usize,
    phase: Phase,
    deadline: Instant,
    cheated: Vec<bool>,
}

impl Game {
    fn new(count: usize, options: Options, now: Instant) -> Self {
        let mut rng = rand::rng();
        let lowest_rank = 13 - usize::from(options.deck) / 4;
        let mut deck: Vec<_> = (1..=FULL_DECK_SIZE)
            .map(Card)
            .filter(|card| card.rank() >= lowest_rank)
            .collect();
        deck.shuffle(&mut rng);
        Self::deal(
            deck,
            count,
            rng.random_range(0..count),
            now + options.speed.turn_time(),
        )
    }

    fn deal(mut deck: Vec<Card>, count: usize, first: usize, deadline: Instant) -> Self {
        let trump = deck[0];
        let mut hands = vec![Vec::with_capacity(HAND_SIZE); count];
        for _ in 0..HAND_SIZE {
            for offset in 0..count {
                hands[(first + offset) % count].push(deck.pop().expect("full deck"));
            }
        }
        let opener = hands
            .iter()
            .enumerate()
            .flat_map(|(seat, hand)| hand.iter().map(move |&card| (seat, card)))
            .filter(|&(_, card)| card.suit() == trump.suit())
            .min_by_key(|&(_, card)| card.rank())
            .map_or(first, |(seat, _)| seat);
        Self {
            version: 0,
            hands,
            deck,
            trump,
            table: Vec::new(),
            opener,
            attacker: opener,
            defender: (opener + 1) % count,
            limit: HAND_SIZE,
            phase: Phase::Open,
            deadline,
            cheated: vec![false; count],
        }
    }

    fn seat_after(&self, seat: usize, offset: usize) -> usize {
        (seat + offset) % self.hands.len()
    }

    fn next_with_cards(&self, seat: usize) -> Option<usize> {
        (1..self.hands.len())
            .map(|offset| self.seat_after(seat, offset))
            .find(|&next| !self.hands[next].is_empty())
    }

    fn turn(&self) -> Option<usize> {
        match self.phase {
            Phase::Open => Some(self.attacker),
            Phase::Defend => Some(self.defender),
            Phase::Throw { offset, .. } => Some(self.seat_after(self.attacker, offset)),
            Phase::Finished { .. } => None,
        }
    }

    fn matches_table(&self, card: Card) -> bool {
        self.table.iter().any(|pair| {
            pair.attack.rank() == card.rank()
                || pair
                    .defense
                    .is_some_and(|defense| defense.rank() == card.rank())
        })
    }

    fn legal_moves(&self, mode: Mode, cheating: bool, seat: usize) -> Vec<Move> {
        if self.turn() != Some(seat) {
            return Vec::new();
        }
        let hand = self.hands[seat].iter().copied();
        let mut moves = match self.phase {
            Phase::Open => hand.map(Move::Attack).collect(),
            Phase::Defend => {
                let attack = self
                    .table
                    .iter()
                    .find(|pair| pair.defense.is_none())
                    .expect("uncovered attack")
                    .attack;
                let mut moves: Vec<_> = hand
                    .clone()
                    .filter(|&card| card.beats(attack, self.trump.suit()))
                    .map(Move::Defend)
                    .collect();
                if mode == Mode::Transfer
                    && self.table.iter().all(|pair| pair.defense.is_none())
                    && self.table.len() < HAND_SIZE
                    && self
                        .next_with_cards(seat)
                        .is_some_and(|next| self.hands[next].len() > self.table.len())
                {
                    moves.extend(
                        hand.filter(|card| card.rank() == attack.rank())
                            .map(Move::Transfer),
                    );
                }
                moves.push(Move::Take);
                moves
            }
            Phase::Throw { .. } => {
                let mut moves: Vec<_> = hand
                    .filter(|&card| self.matches_table(card))
                    .map(Move::Attack)
                    .collect();
                moves.push(Move::Pass);
                moves
            }
            Phase::Finished { .. } => Vec::new(),
        };
        if cheating && !self.cheated[seat] && self.deck.len() >= 2 {
            moves.extend(self.hands[seat].iter().copied().map(Move::Cheat));
        }
        moves
    }

    fn play(&mut self, seat: usize, movement: Move, players: &[Player], throwers: Throwers) {
        if let Some(card) = movement.view().1 {
            let hand = &mut self.hands[seat];
            let index = hand
                .iter()
                .position(|&held| held == card)
                .expect("legal card");
            if matches!(movement, Move::Cheat(_)) {
                let top = self.deck.last_mut().expect("face-down stock");
                std::mem::swap(&mut hand[index], top);
                self.cheated[seat] = true;
                return;
            }
            hand.remove(index);
        }
        match movement {
            Move::Attack(card) => {
                self.table.push(Pair {
                    attack: card,
                    defense: None,
                });
                if let Phase::Throw {
                    offset,
                    taking: true,
                } = self.phase
                {
                    self.offer_throws(offset, true, players, throwers);
                } else {
                    self.phase = Phase::Defend;
                }
            }
            Move::Defend(card) => {
                self.table
                    .iter_mut()
                    .find(|pair| pair.defense.is_none())
                    .expect("uncovered attack")
                    .defense = Some(card);
                if self.table.iter().all(|pair| pair.defense.is_some()) {
                    self.offer_throws(0, false, players, throwers);
                }
            }
            Move::Transfer(card) => {
                self.defender = self.next_with_cards(seat).expect("eligible next defender");
                self.attacker = seat;
                self.limit = self.hands[self.defender].len().min(HAND_SIZE);
                self.table.push(Pair {
                    attack: card,
                    defense: None,
                });
            }
            Move::Take => self.offer_throws(0, true, players, throwers),
            Move::Pass => {
                let Phase::Throw { offset, taking } = self.phase else {
                    unreachable!("only attackers can pass");
                };
                self.offer_throws(offset + 1, taking, players, throwers);
            }
            Move::Cheat(_) => unreachable!("swap handled above"),
        }
    }

    fn offer_throws(&mut self, start: usize, taking: bool, players: &[Player], throwers: Throwers) {
        if self.table.len() < self.limit {
            let next = (start..self.hands.len()).find(|&offset| {
                let seat = self.seat_after(self.attacker, offset);
                seat != self.defender
                    && !self.hands[seat].is_empty()
                    && (throwers == Throwers::All
                        || seat == self.attacker
                        || self.neighbors_defender(seat))
            });
            if let Some(offset) = next {
                self.phase = Phase::Throw { offset, taking };
                return;
            }
        }
        self.finish_bout(taking, players);
    }

    fn neighbors_defender(&self, seat: usize) -> bool {
        let active = |&seat: &usize| !self.deck.is_empty() || !self.hands[seat].is_empty();
        let clockwise =
            || (1..self.hands.len()).map(|offset| self.seat_after(self.defender, offset));
        clockwise().find(active) == Some(seat) || clockwise().rev().find(active) == Some(seat)
    }

    fn finish_bout(&mut self, taking: bool, players: &[Player]) {
        for pair in self.table.drain(..) {
            if taking {
                self.hands[self.defender].push(pair.attack);
                self.hands[self.defender].extend(pair.defense);
            }
        }

        // Refill in clockwise order from the original opener, with the defender last.
        for offset in 0..self.hands.len() {
            let seat = self.seat_after(self.opener, offset);
            if seat != self.defender {
                self.refill(seat);
            }
        }
        self.refill(self.defender);

        if self.deck.is_empty() {
            let mut active = self
                .hands
                .iter()
                .enumerate()
                .filter(|(_, hand)| !hand.is_empty());
            match (active.next(), active.next()) {
                (None, _) => {
                    self.phase = Phase::Finished {
                        message: "Ничья.".into(),
                        loser: None,
                    };
                    return;
                }
                (Some((seat, _)), None) => {
                    self.phase = Phase::Finished {
                        message: format!("Проиграл игрок {}.", players[seat].name),
                        loser: Some(seat),
                    };
                    return;
                }
                _ => {}
            }
        }

        self.attacker = if taking || self.hands[self.defender].is_empty() {
            self.next_with_cards(self.defender)
                .expect("another player remains")
        } else {
            self.defender
        };
        self.opener = self.attacker;
        self.defender = self
            .next_with_cards(self.attacker)
            .expect("another player remains");
        self.limit = self.hands[self.defender].len().min(HAND_SIZE);
        self.phase = Phase::Open;
    }

    fn refill(&mut self, seat: usize) {
        let hand = &mut self.hands[seat];
        while hand.len() < HAND_SIZE {
            let Some(card) = self.deck.pop() else { break };
            hand.push(card);
        }
    }

    fn view(&self, room: &Room, seat: usize, now: Instant) -> Value {
        let hand: Vec<_> = self.hands[seat].iter().map(|card| card.view()).collect();
        let table: Vec<_> = self
            .table
            .iter()
            .map(
                |pair| json!({"attack":pair.attack.view(), "defense":pair.defense.map(Card::view)}),
            )
            .collect();

        let mut actions = Vec::new();
        let mut legal_cards = serde_json::Map::new();
        for movement in self.legal_moves(room.mode, room.options.cheating, seat) {
            let (action, card) = movement.view();
            if actions.last() != Some(&action) {
                actions.push(action);
            }
            if let Some(card) = card {
                legal_cards
                    .entry(action)
                    .or_insert_with(|| json!([]))
                    .as_array_mut()
                    .expect("card list")
                    .push(json!(card.0));
            }
        }

        let turn = self.turn().map(|seat| &room.players[seat]);
        let name = turn.map_or("", |player| player.name.as_str());
        let message = match &self.phase {
            Phase::Open => format!("Ходит {name}."),
            Phase::Defend => format!("Отбивается {name}."),
            Phase::Throw { taking: true, .. } => {
                format!(
                    "{} берёт. Может подкинуть {name}.",
                    room.players[self.defender].name
                )
            }
            Phase::Throw { taking: false, .. } => format!("Может подкинуть {name}."),
            Phase::Finished { message, .. } => message.clone(),
        };
        json!({
            "revision": room.revision(self),
            "hand": hand,
            "trump": self.trump.view(),
            "deck_count": self.deck.len(),
            "table": table,
            "actions": actions,
            "legal_cards": legal_cards,
            "turn": turn.map(|player| player.id.to_string()).unwrap_or_default(),
            "remaining_ms": turn.map_or(0, |_| self.deadline.saturating_duration_since(now).as_millis()),
            "message": message,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug, PartialEq, Eq)]
    enum BankCall {
        Reserve(u64, u64, Vec<(u64, String)>, i64),
        Settle(u64, Option<u64>),
    }

    #[derive(Default)]
    struct RecordingBank {
        calls: Mutex<Vec<BankCall>>,
        fail_reserve: AtomicBool,
        fail_settle: AtomicBool,
    }

    impl Bank for RecordingBank {
        fn reserve(
            &self,
            round: u64,
            guild: u64,
            players: &[(u64, &str)],
            stake: i64,
        ) -> Result<(), String> {
            let players = players
                .iter()
                .map(|&(id, name)| (id, name.to_owned()))
                .collect();
            self.calls
                .lock()
                .unwrap()
                .push(BankCall::Reserve(round, guild, players, stake));
            if self.fail_reserve.load(Ordering::Relaxed) {
                return Err("reserve failed".into());
            }
            Ok(())
        }

        fn settle(&self, round: u64, loser: Option<u64>) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(BankCall::Settle(round, loser));
            if self.fail_settle.load(Ordering::Relaxed) {
                return Err("settle failed".into());
            }
            Ok(())
        }
    }

    fn bank_request(
        state: &State,
        bank: &RecordingBank,
        user: u64,
        action: &str,
        body: &Value,
    ) -> Result<Value, String> {
        let name = if user == 10 { "Host" } else { "Guest" };
        state.handle_with_bank(bank, 1, user, name, action, body)
    }

    fn bank_lobby(state: &State, bank: &RecordingBank, stake: i64) -> Value {
        let created = bank_request(
            state,
            bank,
            10,
            "online_create",
            &json!({
                "mode":"throw", "capacity":2, "stake":stake
            }),
        )
        .unwrap();
        assert_eq!(created["room"]["stake"], stake);
        assert_eq!(created["rooms"][0]["stake"], stake);
        let body = json!({"room_id":created["room"]["id"]});
        bank_request(state, bank, 20, "online_join", &body).unwrap();
        bank_request(state, bank, 20, "online_join", &body).unwrap();
        body
    }

    #[test]
    fn paid_start_reserves_the_unique_seats_once_and_forfeit_settles_once() {
        let state = State::default();
        let bank = RecordingBank::default();
        let body = bank_lobby(&state, &bank, 1_000_000);
        state.sweep(&bank).unwrap();
        assert!(bank.calls.lock().unwrap().is_empty());
        assert!(bank_request(&state, &bank, 20, "online_start", &body).is_err());
        assert!(bank.calls.lock().unwrap().is_empty());

        bank.fail_reserve.store(true, Ordering::Relaxed);
        assert_eq!(
            bank_request(&state, &bank, 10, "online_start", &body),
            Err("reserve failed".into())
        );
        assert_eq!(
            bank_request(&state, &bank, 10, "online_state", &json!({})).unwrap()["room"]["status"],
            "waiting"
        );
        bank.fail_reserve.store(false, Ordering::Relaxed);
        let successful = std::thread::scope(|scope| {
            let start = || bank_request(&state, &bank, 10, "online_start", &body);
            let a = scope.spawn(start);
            let b = scope.spawn(start);
            [a.join().unwrap(), b.join().unwrap()]
                .into_iter()
                .filter(Result::is_ok)
                .count()
        });
        assert_eq!(successful, 1);
        assert!(bank_request(&state, &bank, 20, "online_leave", &body).unwrap()["room"].is_null());
        bank_request(&state, &bank, 10, "online_leave", &body).unwrap();
        state.sweep(&bank).unwrap();
        let calls = bank.calls.lock().unwrap();
        let BankCall::Reserve(round, ..) = calls[0] else {
            panic!("expected reserve");
        };
        let reserve = || {
            BankCall::Reserve(
                round,
                1,
                vec![(10, "Host".into()), (20, "Guest".into())],
                1_000_000,
            )
        };
        assert_eq!(
            *calls,
            [reserve(), reserve(), BankCall::Settle(round, Some(20))]
        );
    }

    #[test]
    fn final_moves_settle_the_loser_or_refund_a_draw_and_sweep_retries_the_same_outcome() {
        for (extra_card, loser) in [(Some(Card(4)), Some(10)), (None, None)] {
            let state = State::default();
            let bank = RecordingBank::default();
            let body = bank_lobby(&state, &bank, 10);
            bank_request(&state, &bank, 10, "online_start", &body).unwrap();
            let revision = {
                let mut store = state.store.lock().unwrap();
                let room = &mut store.rooms[0];
                let game = room.game.as_mut().unwrap();
                game.hands = vec![vec![Card(1)], vec![Card(2)]];
                game.hands[0].extend(extra_card);
                game.deck.clear();
                game.opener = 0;
                game.attacker = 0;
                game.defender = 1;
                game.limit = 1;
                room.revision(room.game.as_ref().unwrap())
            };
            let attacked = bank_request(
                &state,
                &bank,
                10,
                "online_move",
                &json!({
                    "room_id":body["room_id"], "revision":revision, "action":"attack", "card":1
                }),
            )
            .unwrap();
            let revision = &attacked["room"]["game"]["revision"];
            bank.fail_settle.store(true, Ordering::Relaxed);
            assert_eq!(
                bank_request(
                    &state,
                    &bank,
                    20,
                    "online_move",
                    &json!({
                        "room_id":body["room_id"], "revision":revision, "action":"defend", "card":2
                    })
                ),
                Err("settle failed".into())
            );
            {
                let mut store = state.store.lock().unwrap();
                let room = &mut store.rooms[0];
                assert_eq!(
                    room.revision(room.game.as_ref().unwrap()),
                    revision.as_str().unwrap()
                );
                assert!(matches!(room.game.as_ref().unwrap().phase, Phase::Defend));
                room.changed = Instant::now() - INACTIVE_TTL;
            }
            bank.fail_settle.store(false, Ordering::Relaxed);
            state.sweep(&bank).unwrap();
            let finished = bank_request(&state, &bank, 20, "online_state", &json!({})).unwrap();
            assert_eq!(finished["room"]["status"], "finished");
            state.sweep(&bank).unwrap();
            let calls = bank.calls.lock().unwrap();
            let BankCall::Reserve(round, ..) = calls[0] else {
                panic!("expected reserve");
            };
            assert_eq!(
                calls[1..],
                [
                    BankCall::Settle(round, loser),
                    BankCall::Settle(round, loser)
                ]
            );
        }
    }

    #[test]
    fn sweep_retries_timeout_and_expiry_and_ignores_free_and_waiting_rooms() {
        for (stake, start, expire) in [
            (0, true, false),
            (10, false, true),
            (10, true, false),
            (10, true, true),
        ] {
            let state = State::default();
            let bank = RecordingBank::default();
            let body = bank_lobby(&state, &bank, stake);
            if start {
                bank_request(&state, &bank, 10, "online_start", &body).unwrap();
            }
            let loser = {
                let mut store = state.store.lock().unwrap();
                let room = &mut store.rooms[0];
                if expire {
                    room.changed = Instant::now() - INACTIVE_TTL;
                }
                room.game.as_mut().map(|game| {
                    game.deadline = Instant::now();
                    room.players[game.turn().unwrap()].id
                })
            };

            if start && stake > 0 {
                bank.fail_settle.store(true, Ordering::Relaxed);
                assert_eq!(state.sweep(&bank), Err("settle failed".into()));
                let store = state.store.lock().unwrap();
                assert_eq!(store.rooms.len(), 1);
                assert!(store.rooms[0].game.as_ref().unwrap().turn().is_some());
                assert!(store.rooms[0].member(loser.unwrap()).is_some());
                bank.fail_settle.store(false, Ordering::Relaxed);
            }
            state.sweep(&bank).unwrap();
            state.sweep(&bank).unwrap();
            {
                let store = state.store.lock().unwrap();
                if expire {
                    assert!(store.rooms.is_empty());
                } else {
                    assert!(store.rooms[0].game.as_ref().unwrap().turn().is_none());
                    assert!(store.rooms[0].member(loser.unwrap()).is_some());
                }
            }
            let calls = bank.calls.lock().unwrap();
            if start && stake > 0 {
                let BankCall::Reserve(round, ..) = calls[0] else {
                    panic!("expected reserve");
                };
                let outcome = if expire { None } else { loser };
                assert_eq!(
                    calls[1..],
                    [
                        BankCall::Settle(round, outcome),
                        BankCall::Settle(round, outcome)
                    ]
                );
            } else {
                assert!(calls.is_empty());
            }
        }
    }

    fn create_room(state: &State, options: Value) -> (Value, Value) {
        let mut body = json!({"mode":"throw", "capacity":2});
        body.as_object_mut()
            .unwrap()
            .extend(options.as_object().unwrap().clone());
        let created = state.handle(1, 10, "Host", "online_create", &body).unwrap();
        let room = json!({"room_id":created["room"]["id"]});
        state.handle(1, 20, "Guest", "online_join", &room).unwrap();
        (created, room)
    }

    #[test]
    fn room_options_validate_and_default() {
        for invalid in [
            json!({"deck": 32}),
            json!({"deck":"36"}),
            json!({"deck":24,"capacity":5}),
            json!({"deck":52,"capacity":7}),
            json!({"cheating":"true"}),
            json!({"throwers":"other"}),
            json!({"speed":null}),
            json!({"stake":9}),
            json!({"stake":1_000_001}),
            json!({"stake":-10}),
            json!({"stake":10.0}),
        ] {
            let mut body = json!({"mode":"throw", "capacity":2});
            body.as_object_mut()
                .unwrap()
                .extend(invalid.as_object().unwrap().clone());
            assert!(
                State::default()
                    .handle(1, 10, "", "online_create", &body)
                    .is_err(),
                "{invalid}"
            );
        }

        let state = State::default();
        let (created, _) = create_room(&state, json!({}));
        for (field, value) in [
            ("deck", json!(36)),
            ("cheating", json!(false)),
            ("throwers", json!("all")),
            ("speed", json!("slow")),
            ("stake", json!(0)),
        ] {
            assert_eq!(created["room"][field], value);
            assert_eq!(created["rooms"][0][field], value);
        }
    }

    #[test]
    fn each_deck_contains_exactly_its_ranks_in_all_four_suits() {
        for (size, lowest, capacity) in [(24, 7, 4), (36, 4, 6), (52, 0, 6)] {
            let state = State::default();
            let (created, room) = create_room(&state, json!({"deck":size,"capacity":capacity}));
            assert_eq!(created["rooms"][0]["deck"], size);
            for user in 21..21 + capacity - 2 {
                state
                    .handle(1, user, "Player", "online_join", &room)
                    .unwrap();
            }
            assert!(state.handle(1, 100, "Extra", "online_join", &room).is_err());
            state.handle(1, 10, "Host", "online_start", &room).unwrap();
            let store = state.store.lock().unwrap();
            let game = store.rooms[0].game.as_ref().unwrap();
            let mut cards: Vec<_> = game
                .deck
                .iter()
                .chain(game.hands.iter().flatten())
                .map(|&card| (card.suit(), card.rank()))
                .collect();
            cards.sort_unstable();
            assert_eq!(
                cards,
                (0..4)
                    .flat_map(|suit| (lowest..13).map(move |rank| (suit, rank)))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn cheat_changes_only_the_private_hand_and_face_down_stock() {
        let state = State::default();
        let (_, room) = create_room(&state, json!({"deck":52,"cheating":true}));
        state.handle(1, 10, "Host", "online_start", &room).unwrap();
        let (turn, other, top, bottom) = {
            let store = state.store.lock().unwrap();
            let game = store.rooms[0].game.as_ref().unwrap();
            let seat = game.turn().unwrap();
            (
                store.rooms[0].players[seat].id,
                store.rooms[0].players[1 - seat].id,
                *game.deck.last().unwrap(),
                game.deck[0],
            )
        };
        let snapshot = |user| {
            state
                .handle(1, user, "", "online_state", &json!({}))
                .unwrap()["room"]["game"]
                .clone()
        };
        let before = snapshot(turn);
        let observer = snapshot(other);
        let swapped = before["legal_cards"]["cheat"][0].clone();
        assert!(swapped.is_number());
        let mut action = json!({"room_id":room["room_id"],"action":"cheat","revision":before["revision"],"card":swapped});
        assert!(state.handle(1, other, "", "online_move", &action).is_err());
        let result = state.handle(1, turn, "", "online_move", &action).unwrap();
        let after = &result["room"]["game"];
        assert!(
            !after["actions"]
                .as_array()
                .unwrap()
                .contains(&json!("cheat"))
        );
        assert!(after["hand"].as_array().unwrap().contains(&top.view()));
        assert!(
            after["hand"]
                .as_array()
                .unwrap()
                .iter()
                .all(|card| card["id"] != swapped)
        );
        assert_eq!(after["turn"], before["turn"]);
        assert_ne!(after["revision"], before["revision"]);
        let observer_after = snapshot(other);
        for field in [
            "hand",
            "table",
            "turn",
            "deck_count",
            "trump",
            "message",
            "actions",
            "legal_cards",
        ] {
            assert_eq!(observer_after[field], observer[field], "{field}");
        }
        action["revision"] = after["revision"].clone();
        action["card"] = after["hand"][0]["id"].clone();
        assert!(state.handle(1, turn, "", "online_move", &action).is_err());

        let mut store = state.store.lock().unwrap();
        let game = store.rooms[0].game.as_mut().unwrap();
        assert_eq!(game.deck[0], bottom);
        assert_eq!(game.deck.last().unwrap().0, swapped.as_u64().unwrap() as u8);
        game.deck.truncate(1);
        let other_seat = 1 - game.turn().unwrap();
        game.attacker = other_seat;
        assert!(
            store.rooms[0].detail(other, Instant::now())["game"]["legal_cards"]["cheat"].is_null()
        );
    }

    #[test]
    fn reactions_are_scoped_rate_limited_and_expire() {
        let state = State::default();
        let (_, room) = create_room(&state, json!({}));
        state.handle(1, 10, "Host", "online_start", &room).unwrap();
        let mut reaction = json!({"room_id":room["room_id"], "emoji":"x"});
        assert!(state.handle(1, 10, "", "online_react", &reaction).is_err());
        reaction["emoji"] = json!("👍");
        assert!(state.handle(2, 10, "", "online_react", &reaction).is_err());
        assert!(state.handle(1, 30, "", "online_react", &reaction).is_err());
        let before = state.handle(1, 10, "", "online_state", &json!({})).unwrap();
        let sent = state.handle(1, 10, "", "online_react", &reaction).unwrap();
        assert_eq!(sent["room"]["players"][0]["reaction"], "👍");
        assert_eq!(
            sent["room"]["game"]["revision"],
            before["room"]["game"]["revision"]
        );
        assert!(
            sent["room"]["game"]["remaining_ms"].as_u64().unwrap()
                <= before["room"]["game"]["remaining_ms"].as_u64().unwrap()
        );
        assert!(state.handle(1, 10, "", "online_react", &reaction).is_err());
        let other = state.handle(1, 20, "", "online_react", &reaction).unwrap();
        assert_eq!(other["room"]["players"][0]["reaction"], "👍");
        assert_eq!(other["room"]["players"][1]["reaction"], "👍");
        let store = state.store.lock().unwrap();
        let later = Instant::now() + REACTION_TTL;
        assert!(store.rooms[0].detail(20, later)["players"][0]["reaction"].is_null());
    }

    #[test]
    fn shared_room_requires_both_current_members_and_a_matching_guild_and_room() {
        let state = State::default();
        let (_, room) = create_room(&state, json!({"capacity": 3}));
        let id = room["room_id"].as_str().unwrap();
        assert!(state.shared_room(1, 10, 20, id));
        assert!(!state.shared_room(2, 10, 20, id));
        assert!(!state.shared_room(1, 10, 30, id));
        assert!(!state.shared_room(1, 30, 20, id));
        assert!(!state.shared_room(1, 10, 20, "another-room"));
        let reaction = json!({"room_id":id, "emoji":"😂"});
        state.handle(1, 10, "", "online_react", &reaction).unwrap();
        let outside = state.handle(1, 30, "", "online_state", &json!({})).unwrap();
        assert!(outside["rooms"][0]["players"][0]["reaction"].is_null());
        state.handle(1, 10, "", "online_start", &room).unwrap();
        state.handle(1, 20, "", "online_leave", &room).unwrap();
        assert!(!state.shared_room(1, 10, 20, id));
        assert!(!state.shared_room(1, 20, 10, id));
    }

    #[test]
    fn throwers_include_the_attacker_and_only_active_neighbors_in_neighbors_mode() {
        for (throwers, expected) in [
            (Throwers::Neighbors, vec![5, 0, 3]),
            (Throwers::All, vec![5, 0, 3, 4]),
        ] {
            let mut room = transfer_fixture(Mode::Transfer, &[2]);
            room.options.throwers = throwers;
            room.players = (0..6).map(|id| Player::new(id, "Player")).collect();
            let game = room.game.as_mut().unwrap();
            game.hands = vec![
                vec![Card(1)],
                vec![],
                vec![Card(2)],
                vec![Card(19)],
                vec![Card(28)],
                vec![Card(10)],
            ];
            game.cheated = vec![false; 6];
            game.attacker = 5;
            game.opener = 5;
            game.defender = 2;
            game.phase = Phase::Defend;
            game.table.push(Pair {
                attack: Card(37),
                defense: None,
            });

            play_card(&mut room, 2, "take", 0).unwrap();
            for seat in expected {
                assert_eq!(room.game.as_ref().unwrap().turn(), Some(seat));
                play_card(&mut room, seat, "pass", 0).unwrap();
            }
            assert!(matches!(room.game.as_ref().unwrap().phase, Phase::Open));
        }
    }

    #[test]
    fn tempo_resets_per_accepted_move_and_timeout_forfeits_the_turn() {
        for (speed, milliseconds) in [("fast", 15_000), ("slow", 45_000)] {
            let state = State::default();
            let (created, room) = create_room(&state, json!({"speed":speed}));
            assert_eq!(created["room"]["speed"], speed);
            let started = state.handle(1, 10, "", "online_start", &room).unwrap();
            let remaining = started["room"]["game"]["remaining_ms"].as_u64().unwrap();
            assert!((milliseconds - 1_000..=milliseconds).contains(&remaining));
            let (turn, other) = {
                let mut store = state.store.lock().unwrap();
                let room = &mut store.rooms[0];
                let game = room.game.as_mut().unwrap();
                game.deadline = Instant::now() + Duration::from_secs(5);
                let seat = game.turn().unwrap();
                (room.players[seat].id, room.players[1 - seat].id)
            };
            let snapshot = state
                .handle(1, turn, "", "online_state", &json!({}))
                .unwrap();
            let game = &snapshot["room"]["game"];
            let mut attack = json!({
                "room_id":room["room_id"], "revision":game["revision"], "action":"attack", "card":0,
            });
            assert!(state.handle(1, turn, "", "online_move", &attack).is_err());
            let after_invalid = state
                .handle(1, turn, "", "online_state", &json!({}))
                .unwrap();
            assert!(
                after_invalid["room"]["game"]["remaining_ms"]
                    .as_u64()
                    .unwrap()
                    <= 5_000
            );
            attack["card"] = game["legal_cards"]["attack"][0].clone();
            let after_attack = state.handle(1, turn, "", "online_move", &attack).unwrap();
            let remaining = after_attack["room"]["game"]["remaining_ms"]
                .as_u64()
                .unwrap();
            assert!((milliseconds - 1_000..=milliseconds).contains(&remaining));

            state.store.lock().unwrap().rooms[0]
                .game
                .as_mut()
                .unwrap()
                .deadline = Instant::now();
            let late_move = json!({"room_id":room["room_id"], "revision":after_attack["room"]["game"]["revision"], "action":"take"});
            assert!(
                state
                    .handle(1, other, "", "online_move", &late_move)
                    .is_err()
            );
            let finished = state
                .handle(1, turn, "", "online_state", &json!({}))
                .unwrap();
            assert_eq!(finished["room"]["status"], "finished");
            assert_eq!(finished["room"]["game"]["remaining_ms"], 0);
            assert_eq!(finished["room"]["players"].as_array().unwrap().len(), 2);
            let timed_out = state
                .handle(1, other, "", "online_state", &json!({}))
                .unwrap();
            assert_eq!(timed_out["room"]["status"], "finished");
            assert!(
                timed_out["room"]["game"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("не успел")
            );
        }
    }

    #[test]
    fn guilds_lobbies_and_hands_are_scoped() {
        let state = State::default();
        let created = state
            .handle(
                1,
                10,
                "Host",
                "online_create",
                &json!({"mode":"throw","capacity":2}),
            )
            .unwrap();
        let room = json!({"room_id": created["room"]["id"]});
        assert_eq!(created["you"], "10");
        assert_eq!(created["rooms"][0]["id"], room["room_id"]);
        assert!(state.handle(2, 20, "Guest", "online_join", &room).is_err());
        let joined = state.handle(1, 20, "Guest", "online_join", &room).unwrap();
        assert_eq!(joined["rooms"], json!([]));
        assert!(state.handle(1, 20, "Guest", "online_start", &room).is_err());
        let started = state.handle(1, 10, "Host", "online_start", &room).unwrap();
        let guest = state
            .handle(1, 20, "Guest", "online_state", &json!({}))
            .unwrap();
        assert_eq!(started["room"]["status"], "playing");
        assert_eq!(started["room"]["game"]["deck_count"], 24);
        assert_eq!(
            started["room"]["players"][1],
            json!({"id":"20","name":"Guest","cards":6})
        );
        let hand = started["room"]["game"]["hand"].as_array().unwrap();
        let guest_hand = guest["room"]["game"]["hand"].as_array().unwrap();
        assert_eq!((hand.len(), guest_hand.len()), (6, 6));
        assert!(hand.iter().all(|card| !guest_hand.contains(card)));

        let left = state.handle(1, 10, "Host", "online_leave", &room).unwrap();
        assert!(left["room"].is_null());
        let finished = state
            .handle(1, 20, "Guest", "online_state", &json!({}))
            .unwrap();
        assert_eq!(finished["room"]["status"], "finished");
        assert_eq!(finished["room"]["game"]["actions"], json!([]));
        assert!(
            finished["room"]["game"]["message"]
                .as_str()
                .unwrap()
                .contains("Host")
        );
    }

    #[test]
    fn moves_enforce_turn_ownership_and_revision() {
        let state = State::default();
        let created = state
            .handle(
                1,
                10,
                "Host",
                "online_create",
                &json!({"mode":"transfer","capacity":2}),
            )
            .unwrap();
        let room = json!({"room_id": created["room"]["id"]});
        state.handle(1, 20, "Guest", "online_join", &room).unwrap();
        let started = state.handle(1, 10, "Host", "online_start", &room).unwrap();
        let turn = started["room"]["game"]["turn"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        let other = if turn == 10 { 20 } else { 10 };
        let current = state
            .handle(1, turn, "", "online_state", &json!({}))
            .unwrap();
        let game = &current["room"]["game"];
        let mut attack = json!({
            "room_id": room["room_id"], "revision": game["revision"], "action":"attack",
            "card": game["legal_cards"]["attack"][0]
        });
        assert!(state.handle(1, other, "", "online_move", &attack).is_err());
        attack["card"] = json!(257);
        assert!(state.handle(1, turn, "", "online_move", &attack).is_err());
        attack["card"] = game["legal_cards"]["attack"][0].clone();
        let played = state.handle(1, turn, "", "online_move", &attack).unwrap();
        assert_eq!(played["room"]["game"]["turn"], other.to_string());
        assert_eq!(
            played["room"]["game"]["table"][0]["attack"]["id"],
            attack["card"]
        );
        assert!(state.handle(1, turn, "", "online_move", &attack).is_err());
    }

    fn transfer_fixture(mode: Mode, next_hand: &[u8]) -> Room {
        Room {
            id: "fixture".into(),
            guild: 1,
            round: 0,
            mode,
            options: Options::parse(&json!({})).unwrap(),
            capacity: 3,
            players: vec![
                Player::new(1, "First"),
                Player::new(2, "Second"),
                Player::new(3, "Third"),
            ],
            game: Some(Game {
                version: 0,
                hands: vec![
                    vec![Card(1), Card(21), Card(22), Card(23)],
                    vec![Card(10), Card(8)],
                    next_hand.iter().copied().map(Card).collect(),
                ],
                deck: vec![],
                trump: Card(36),
                table: vec![],
                opener: 0,
                attacker: 0,
                defender: 1,
                limit: 2,
                phase: Phase::Open,
                deadline: Instant::now() + Speed::Slow.turn_time(),
                cheated: vec![false; 3],
            }),
            changed: Instant::now(),
            pending: None,
        }
    }

    fn play_card(room: &mut Room, seat: usize, action: &str, card: u8) -> Result<(), String> {
        let revision = room.revision(room.game.as_ref().unwrap());
        room.play(
            seat,
            &json!({"revision":revision, "action":action, "card":card}),
            Instant::now(),
        )
    }

    #[test]
    fn transfer_requires_mode_rank_capacity_and_no_prior_defense() {
        let mut throw = transfer_fixture(Mode::Throw, &[2, 11, 19]);
        play_card(&mut throw, 0, "attack", 1).unwrap();
        assert!(play_card(&mut throw, 1, "transfer", 10).is_err());

        let mut short = transfer_fixture(Mode::Transfer, &[2]);
        play_card(&mut short, 0, "attack", 1).unwrap();
        assert!(play_card(&mut short, 1, "transfer", 10).is_err());

        let mut room = transfer_fixture(Mode::Transfer, &[2, 11, 19]);
        play_card(&mut room, 0, "attack", 1).unwrap();
        assert_eq!(
            room.detail(2, Instant::now())["game"]["legal_cards"]["transfer"],
            json!([10])
        );
        assert!(play_card(&mut room, 1, "transfer", 8).is_err());
        play_card(&mut room, 1, "transfer", 10).unwrap();
        let detail = room.detail(3, Instant::now());
        assert_eq!(detail["game"]["turn"], "3");
        assert_eq!(detail["game"]["table"][1]["attack"]["id"], 10);
        assert_eq!(detail["game"]["legal_cards"]["transfer"], json!([19]));
        play_card(&mut room, 2, "defend", 2).unwrap();
        assert!(play_card(&mut room, 2, "transfer", 19).is_err());
        assert_eq!(
            room.detail(3, Instant::now())["game"]["legal_cards"]["defend"],
            json!([11])
        );
        play_card(&mut room, 2, "defend", 11).unwrap();
        assert_eq!(room.detail(2, Instant::now())["game"]["turn"], "2");
    }

    #[test]
    fn six_players_play_a_fixed_deck_to_a_single_loser() {
        let state = State::default();
        let request = |user, action, body: &Value| {
            state
                .handle(1, user, &format!("Player {user}"), action, body)
                .unwrap()
        };
        let created = request(1, "online_create", &json!({"mode":"throw","capacity":6}));
        let room_id = &created["room"]["id"];
        let room_body = json!({"room_id":room_id});
        for user in 2..=6 {
            request(user, "online_join", &room_body);
        }
        let started = request(1, "online_start", &room_body);
        assert_eq!(started["room"]["players"].as_array().unwrap().len(), 6);
        assert_eq!(started["room"]["game"]["deck_count"], 0);

        state.store.lock().unwrap().rooms[0].game = Some(Game::deal(
            (1..=DECK_SIZE).map(Card).collect(),
            6,
            0,
            Instant::now() + Speed::Slow.turn_time(),
        ));
        let mut snapshot = request(1, "online_state", &json!({}));
        for _ in 0..300 {
            if snapshot["room"]["status"] == "finished" {
                break;
            }
            let turn = snapshot["room"]["game"]["turn"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap();
            let current = request(turn, "online_state", &json!({}));
            let game = &current["room"]["game"];
            let actions = game["actions"].as_array().unwrap();
            let action = ["defend", "take", "pass", "attack"]
                .into_iter()
                .find(|&action| actions.contains(&json!(action)))
                .unwrap();
            snapshot = request(
                turn,
                "online_move",
                &json!({
                    "room_id":room_id, "revision":game["revision"], "action":action,
                    "card":game["legal_cards"][action][0]
                }),
            );
        }
        assert_eq!(snapshot["room"]["status"], "finished");
        assert_eq!(snapshot["room"]["game"]["table"], json!([]));
        assert_eq!(snapshot["room"]["game"]["actions"], json!([]));
        assert!(
            snapshot["room"]["game"]["message"]
                .as_str()
                .unwrap()
                .contains("Player 1")
        );
        let counts: Vec<_> = snapshot["room"]["players"]
            .as_array()
            .unwrap()
            .iter()
            .map(|player| player["cards"].as_u64().unwrap())
            .collect();
        assert_eq!(counts, [2, 0, 0, 0, 0, 0]);
    }
}
