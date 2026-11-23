//! Guild-local multiplayer Durak. Room mutations and snapshots share one lock.

use crate::durak::{Card, DECK_SIZE, HAND_SIZE};
use rand::{seq::SliceRandom, Rng};
use serde_json::{json, Value};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

const MAX_ROOMS: usize = 256;
const INACTIVE_TTL: Duration = Duration::from_secs(30 * 60);
const FINISHED_TTL: Duration = Duration::from_secs(10 * 60);
const ROOM_MISSING: &str = "Комната не найдена.";
const ILLEGAL: &str = "Этот ход сейчас недоступен.";

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
    pub fn handle(
        &self,
        guild: u64,
        user: u64,
        name: &str,
        action: &str,
        body: &Value,
    ) -> Result<Value, String> {
        let mut store = self.store.lock().map_err(|_| "Игра недоступна.")?;
        let now = Instant::now();
        store.rooms.retain(|room| !room.expired(now));
        let current = store
            .rooms
            .iter()
            .position(|room| room.guild == guild && room.member(user).is_some());
        let mut current = current;

        match action {
            "online_state" => {}
            "online_create" => {
                if current.is_some() {
                    return Err("Сначала выйдите из текущей комнаты.".into());
                }
                let mode = Mode::parse(&body["mode"])?;
                let capacity = body["capacity"]
                    .as_u64()
                    .filter(|capacity| (2..=6).contains(capacity))
                    .ok_or("В комнате может быть от 2 до 6 игроков.")?
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
                    mode,
                    capacity,
                    players: vec![Player::new(user, name)],
                    game: None,
                    changed: now,
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
            "online_start" | "online_leave" | "online_move" => {
                let id = room_id(body)?;
                let index = current
                    .filter(|&index| store.rooms[index].id == id)
                    .ok_or(ROOM_MISSING)?;
                let room = &mut store.rooms[index];
                let seat = room.member(user).expect("current room member");
                match action {
                    "online_start" => {
                        if seat != 0 || room.players.len() < 2 || room.game.is_some() {
                            return Err(
                                "Только хозяин комнаты может начать игру с двумя и более игроками."
                                    .into(),
                            );
                        }
                        room.game = Some(Game::new(room.players.len()));
                    }
                    "online_move" => room.play(seat, body)?,
                    "online_leave" => {
                        room.leave(seat);
                        current = None;
                    }
                    _ => unreachable!(),
                }
                room.changed = now;
                if room.host().is_none() {
                    store.rooms.remove(index);
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
        Ok(json!({
            "you": user.to_string(),
            "rooms": rooms,
            "room": current.map(|index| store.rooms[index].detail(user)),
        }))
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

struct Player {
    id: u64,
    name: String,
    departed: bool,
}

impl Player {
    fn new(id: u64, name: &str) -> Self {
        Self {
            id,
            name: name.to_owned(),
            departed: false,
        }
    }
}

struct Room {
    id: String,
    guild: u64,
    mode: Mode,
    capacity: usize,
    players: Vec<Player>,
    game: Option<Game>,
    changed: Instant,
}

impl Room {
    fn expired(&self, now: Instant) -> bool {
        let ttl = match self.game.as_ref().map(|game| &game.phase) {
            Some(Phase::Finished(_)) => FINISHED_TTL,
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

    fn summary(&self) -> Value {
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
                view
            })
            .collect();
        json!({
            "id": self.id,
            "mode": self.mode.name(),
            "capacity": self.capacity,
            "host": self.host().expect("occupied room").id.to_string(),
            "players": players,
        })
    }

    fn detail(&self, user: u64) -> Value {
        let seat = self.member(user).expect("current room member");
        let mut detail = self.summary();
        detail["status"] = json!(match self.game.as_ref().map(|game| &game.phase) {
            None => "waiting",
            Some(Phase::Finished(_)) => "finished",
            _ => "playing",
        });
        detail["game"] = self
            .game
            .as_ref()
            .map_or(Value::Null, |game| game.view(self, seat));
        detail
    }

    fn revision(&self, game: &Game) -> String {
        format!("{}-{}", self.id, game.version)
    }

    fn play(&mut self, seat: usize, body: &Value) -> Result<(), String> {
        let game = self.game.as_ref().ok_or(ILLEGAL)?;
        if body["revision"].as_str() != Some(self.revision(game).as_str()) {
            return Err("Игра уже изменилась. Обновите её состояние.".into());
        }
        let movement = Move::parse(body).ok_or(ILLEGAL)?;
        if !game.legal_moves(self.mode, seat).contains(&movement) {
            return Err(ILLEGAL.into());
        }
        let game = self.game.as_mut().expect("game checked");
        let version = game.version.checked_add(1).ok_or("Игра недоступна.")?;
        game.play(seat, movement, &self.players);
        game.version = version;
        Ok(())
    }

    fn leave(&mut self, seat: usize) {
        if let Some(game) = &mut self.game {
            self.players[seat].departed = true;
            if !matches!(game.phase, Phase::Finished(_)) {
                game.phase = Phase::Finished(format!(
                    "{} покинул игру и проиграл.",
                    self.players[seat].name
                ));
                game.version = game.version.saturating_add(1);
            }
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
    Take,
    Pass,
}

impl Move {
    fn parse(body: &Value) -> Option<Self> {
        let card = || {
            body["card"]
                .as_u64()
                .filter(|id| (1..=u64::from(DECK_SIZE)).contains(id))
                .map(|id| Card(id as u8))
        };
        match body["action"].as_str()? {
            "attack" => Some(Self::Attack(card()?)),
            "defend" => Some(Self::Defend(card()?)),
            "transfer" => Some(Self::Transfer(card()?)),
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
            Self::Take => ("take", None),
            Self::Pass => ("pass", None),
        }
    }
}

enum Phase {
    Open,
    Defend,
    Throw { offset: usize, taking: bool },
    Finished(String),
}

struct Pair {
    attack: Card,
    defense: Option<Card>,
}

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
}

impl Game {
    fn new(count: usize) -> Self {
        let mut rng = rand::rng();
        let mut deck: Vec<_> = (1..=DECK_SIZE).map(Card).collect();
        deck.shuffle(&mut rng);
        Self::deal(deck, count, rng.random_range(0..count))
    }

    fn deal(mut deck: Vec<Card>, count: usize, first: usize) -> Self {
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
            Phase::Finished(_) => None,
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

    fn legal_moves(&self, mode: Mode, seat: usize) -> Vec<Move> {
        if self.turn() != Some(seat) {
            return Vec::new();
        }
        let hand = self.hands[seat].iter().copied();
        match self.phase {
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
            Phase::Finished(_) => Vec::new(),
        }
    }

    fn play(&mut self, seat: usize, movement: Move, players: &[Player]) {
        if let Some(card) = movement.view().1 {
            let hand = &mut self.hands[seat];
            let index = hand
                .iter()
                .position(|&held| held == card)
                .expect("legal card");
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
                    self.offer_throws(offset, true, players);
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
                    self.offer_throws(0, false, players);
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
            Move::Take => self.offer_throws(0, true, players),
            Move::Pass => {
                let Phase::Throw { offset, taking } = self.phase else {
                    unreachable!("only attackers can pass");
                };
                self.offer_throws(offset + 1, taking, players);
            }
        }
    }

    fn offer_throws(&mut self, start: usize, taking: bool, players: &[Player]) {
        if self.table.len() < self.limit {
            let next = (start..self.hands.len()).find(|&offset| {
                let seat = self.seat_after(self.attacker, offset);
                seat != self.defender && !self.hands[seat].is_empty()
            });
            if let Some(offset) = next {
                self.phase = Phase::Throw { offset, taking };
                return;
            }
        }
        self.finish_bout(taking, players);
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
                    self.phase = Phase::Finished("Ничья.".into());
                    return;
                }
                (Some((seat, _)), None) => {
                    self.phase = Phase::Finished(format!("Проиграл игрок {}.", players[seat].name));
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

    fn view(&self, room: &Room, seat: usize) -> Value {
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
        for movement in self.legal_moves(room.mode, seat) {
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
            Phase::Finished(result) => result.clone(),
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
            "message": message,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert!(finished["room"]["game"]["message"]
            .as_str()
            .unwrap()
            .contains("Host"));
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
            mode,
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
            }),
            changed: Instant::now(),
        }
    }

    fn play_card(room: &mut Room, seat: usize, action: &str, card: u8) -> Result<(), String> {
        let revision = room.revision(room.game.as_ref().unwrap());
        room.play(
            seat,
            &json!({"revision":revision, "action":action, "card":card}),
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
            room.detail(2)["game"]["legal_cards"]["transfer"],
            json!([10])
        );
        assert!(play_card(&mut room, 1, "transfer", 8).is_err());
        play_card(&mut room, 1, "transfer", 10).unwrap();
        let detail = room.detail(3);
        assert_eq!(detail["game"]["turn"], "3");
        assert_eq!(detail["game"]["table"][1]["attack"]["id"], 10);
        assert_eq!(detail["game"]["legal_cards"]["transfer"], json!([19]));
        play_card(&mut room, 2, "defend", 2).unwrap();
        assert!(play_card(&mut room, 2, "transfer", 19).is_err());
        assert_eq!(room.detail(3)["game"]["legal_cards"]["defend"], json!([11]));
        play_card(&mut room, 2, "defend", 11).unwrap();
        assert_eq!(room.detail(2)["game"]["turn"], "2");
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

        state.store.lock().unwrap().rooms[0].game =
            Some(Game::deal((1..=DECK_SIZE).map(Card).collect(), 6, 0));
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
        assert!(snapshot["room"]["game"]["message"]
            .as_str()
            .unwrap()
            .contains("Player 1"));
        let counts: Vec<_> = snapshot["room"]["players"]
            .as_array()
            .unwrap()
            .iter()
            .map(|player| player["cards"].as_u64().unwrap())
            .collect();
        assert_eq!(counts, [2, 0, 0, 0, 0, 0]);
    }
}
