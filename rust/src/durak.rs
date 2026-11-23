//! Подкидной дурак один на один с ботом, без ставок.

use rand::{seq::SliceRandom, Rng};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Mutex};

pub(crate) const HAND_SIZE: usize = 6;
pub(crate) const DECK_SIZE: u8 = 36;
const MAX_GAMES: usize = 1024;
const RANKS: [&str; 9] = ["6", "7", "8", "9", "10", "J", "Q", "K", "A"];
const SUITS: [&str; 4] = ["♣", "♦", "♥", "♠"];
const STALE: &str = "Игра уже изменилась. Обновите её состояние.";
const ILLEGAL: &str = "Этот ход сейчас недоступен.";

pub struct State {
    store: Mutex<Store>,
    epoch: u64,
}

#[derive(Default)]
struct Store {
    sequence: u64,
    games: HashMap<(u64, u64), SavedGame>,
}

struct SavedGame {
    version: u64,
    game: Game,
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
        action: &str,
        body: &Value,
    ) -> Result<Value, String> {
        let mut store = self.store.lock().map_err(|_| "Игра недоступна.")?;
        let Store { sequence, games } = &mut *store;
        let key = (guild, user);
        match action {
            "state" => Ok(json!({"game": games.get(&key).map(|saved| self.view(saved))})),
            "start" => {
                let version = next_version(*sequence)?;
                if games.len() == MAX_GAMES && !games.contains_key(&key) {
                    // Bound memory while retaining the games with the most recent moves.
                    if let Some((&oldest, _)) = games.iter().min_by_key(|(_, game)| game.version) {
                        games.remove(&oldest);
                    }
                }
                let saved = SavedGame {
                    version,
                    game: Game::new(),
                };
                let response = json!({"game": self.view(&saved)});
                games.insert(key, saved);
                *sequence = version;
                Ok(response)
            }
            "move" => {
                let saved = games.get_mut(&key).ok_or("Сначала начните игру.")?;
                let revision = self.revision(saved.version);
                if body["revision"].as_str() != Some(revision.as_str()) {
                    return Err(STALE.into());
                }
                let version = next_version(*sequence)?;
                saved.game.play(body)?;
                saved.version = version;
                *sequence = version;
                Ok(json!({"game": self.view(saved)}))
            }
            _ => Err("Неизвестный запрос игры.".into()),
        }
    }

    fn revision(&self, version: u64) -> String {
        format!("{:016x}-{version:016x}", self.epoch)
    }

    fn view(&self, saved: &SavedGame) -> Value {
        saved.game.view(&self.revision(saved.version))
    }
}

fn next_version(version: u64) -> Result<u64, &'static str> {
    version.checked_add(1).ok_or("Игра недоступна.")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Card(pub(crate) u8);

impl Card {
    pub(crate) fn rank(self) -> usize {
        usize::from(self.0 - 1) % RANKS.len()
    }

    pub(crate) fn suit(self) -> usize {
        usize::from(self.0 - 1) / RANKS.len()
    }

    pub(crate) fn beats(self, attack: Self, trump: usize) -> bool {
        if self.suit() == attack.suit() {
            self.rank() > attack.rank()
        } else {
            self.suit() == trump
        }
    }

    pub(crate) fn view(self) -> Value {
        json!({"id": self.0, "rank": RANKS[self.rank()], "suit": SUITS[self.suit()]})
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Side {
    User,
    Bot,
}

impl Side {
    fn other(self) -> Self {
        match self {
            Self::User => Self::Bot,
            Self::Bot => Self::User,
        }
    }
}

struct Pair {
    attack: Card,
    defense: Option<Card>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    Won,
    Lost,
    Draw,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Attack,
    Defend,
    Throw,
    Finished,
}

struct Game {
    user: Vec<Card>,
    bot: Vec<Card>,
    deck: Vec<Card>,
    discard: Vec<Card>,
    trump: Card,
    table: Vec<Pair>,
    attacker: Side,
    attack_limit: usize,
    bot_taking: bool,
    outcome: Option<Outcome>,
}

impl Game {
    fn new() -> Self {
        let mut rng = rand::rng();
        let mut deck: Vec<_> = (1..=DECK_SIZE).map(Card).collect();
        deck.shuffle(&mut rng);
        Self::deal(deck, &mut rng)
    }

    fn deal(mut deck: Vec<Card>, rng: &mut impl Rng) -> Self {
        // Draw from the end so the face-up trump is the last stock card drawn.
        let trump = deck[0];
        let mut user = Vec::with_capacity(HAND_SIZE);
        let mut bot = Vec::with_capacity(HAND_SIZE);
        for _ in 0..HAND_SIZE {
            user.push(deck.pop().expect("full deck"));
            bot.push(deck.pop().expect("full deck"));
        }
        let attacker = [(Side::User, &user), (Side::Bot, &bot)]
            .into_iter()
            .flat_map(|(side, hand)| hand.iter().map(move |&card| (side, card)))
            .filter(|&(_, card)| card.suit() == trump.suit())
            .min_by_key(|&(_, card)| card.rank())
            .map_or_else(
                || if rng.random() { Side::User } else { Side::Bot },
                |(side, _)| side,
            );
        let mut game = Self {
            user,
            bot,
            deck,
            discard: Vec::new(),
            trump,
            table: Vec::new(),
            attacker,
            attack_limit: HAND_SIZE,
            bot_taking: false,
            outcome: None,
        };
        game.advance();
        game
    }

    fn hand(&self, side: Side) -> &[Card] {
        match side {
            Side::User => &self.user,
            Side::Bot => &self.bot,
        }
    }

    fn hand_mut(&mut self, side: Side) -> &mut Vec<Card> {
        match side {
            Side::User => &mut self.user,
            Side::Bot => &mut self.bot,
        }
    }

    fn remove_card(&mut self, side: Side, card: Card) {
        let hand = self.hand_mut(side);
        let index = hand
            .iter()
            .position(|&held| held == card)
            .expect("legal card");
        hand.remove(index);
    }

    fn attack_cards(&self) -> impl Iterator<Item = Card> + '_ {
        self.hand(self.attacker).iter().copied().filter(|&card| {
            self.table.len() < self.attack_limit
                && (self.table.is_empty()
                    || self.table.iter().any(|pair| {
                        card.rank() == pair.attack.rank()
                            || pair
                                .defense
                                .is_some_and(|defense| card.rank() == defense.rank())
                    }))
        })
    }

    fn defense_cards(&self) -> impl Iterator<Item = Card> + '_ {
        let attack = self
            .table
            .last()
            .filter(|pair| pair.defense.is_none())
            .map(|pair| pair.attack);
        self.hand(self.attacker.other())
            .iter()
            .copied()
            .filter(move |&card| attack.is_some_and(|attack| card.beats(attack, self.trump.suit())))
    }

    fn put_attack(&mut self, card: Card) {
        self.remove_card(self.attacker, card);
        self.table.push(Pair {
            attack: card,
            defense: None,
        });
    }

    fn put_defense(&mut self, card: Card) {
        self.remove_card(self.attacker.other(), card);
        self.table.last_mut().expect("open attack").defense = Some(card);
    }

    fn bot_attack(&self) -> Option<Card> {
        self.attack_cards().min_by_key(|card| self.card_cost(*card))
    }

    fn bot_defense(&self) -> Option<Card> {
        self.defense_cards()
            .min_by_key(|card| self.card_cost(*card))
    }

    fn card_cost(&self, card: Card) -> (bool, usize, usize) {
        (card.suit() == self.trump.suit(), card.rank(), card.suit())
    }

    fn finish_bout(&mut self, take: bool) {
        let attacker = self.attacker;
        let defender = attacker.other();
        let table = std::mem::take(&mut self.table);
        let destination = if take {
            self.hand_mut(defender)
        } else {
            &mut self.discard
        };
        destination.extend(
            table
                .into_iter()
                .flat_map(|pair| std::iter::once(pair.attack).chain(pair.defense)),
        );

        for side in [attacker, defender] {
            while self.hand(side).len() < HAND_SIZE {
                let Some(card) = self.deck.pop() else { break };
                self.hand_mut(side).push(card);
            }
        }

        if self.deck.is_empty() {
            self.outcome = match (self.user.is_empty(), self.bot.is_empty()) {
                (true, true) => Some(Outcome::Draw),
                (true, false) => Some(Outcome::Won),
                (false, true) => Some(Outcome::Lost),
                (false, false) => None,
            };
        }
        if !take {
            self.attacker = defender;
        }
        self.attack_limit = self.hand(self.attacker.other()).len().min(HAND_SIZE);
        self.bot_taking = false;
    }

    // Run the bot and settle any bout with no further choices until the user can act.
    fn advance(&mut self) {
        while self.outcome.is_none() {
            match self.attacker {
                Side::User => {
                    let Some(pair) = self.table.last() else {
                        return;
                    };
                    if pair.defense.is_none() && !self.bot_taking {
                        if let Some(card) = self.bot_defense() {
                            self.put_defense(card);
                        } else {
                            self.bot_taking = true;
                        }
                    }
                    if self.attack_cards().next().is_some() {
                        return;
                    }
                    self.finish_bout(self.bot_taking);
                }
                Side::Bot => {
                    if self.table.last().is_some_and(|pair| pair.defense.is_none()) {
                        return;
                    }
                    if let Some(card) = self.bot_attack() {
                        self.put_attack(card);
                        return;
                    }
                    self.finish_bout(false);
                }
            }
        }
    }

    fn phase(&self) -> Phase {
        if self.outcome.is_some() {
            Phase::Finished
        } else if self.attacker == Side::Bot {
            Phase::Defend
        } else if self.table.is_empty() {
            Phase::Attack
        } else {
            Phase::Throw
        }
    }

    fn play(&mut self, body: &Value) -> Result<(), &'static str> {
        let card = body["card"]
            .as_u64()
            .and_then(|id| u8::try_from(id).ok())
            .map(Card);
        match (self.phase(), body["action"].as_str()) {
            (Phase::Attack | Phase::Throw, Some("attack")) => {
                let card = self
                    .attack_cards()
                    .find(|&legal| Some(legal) == card)
                    .ok_or(ILLEGAL)?;
                self.put_attack(card);
            }
            (Phase::Defend, Some("defend")) => {
                let card = self
                    .defense_cards()
                    .find(|&legal| Some(legal) == card)
                    .ok_or(ILLEGAL)?;
                self.put_defense(card);
            }
            (Phase::Defend, Some("take")) => {
                // Once the user takes, the bot may still throw in matching cards.
                while let Some(card) = self.bot_attack() {
                    self.put_attack(card);
                }
                self.finish_bout(true);
            }
            (Phase::Throw, Some("pass")) => self.finish_bout(self.bot_taking),
            _ => return Err(ILLEGAL),
        }
        self.advance();
        Ok(())
    }

    fn view(&self, revision: &str) -> Value {
        let status = match self.outcome {
            None => "playing",
            Some(Outcome::Won) => "won",
            Some(Outcome::Lost) => "lost",
            Some(Outcome::Draw) => "draw",
        };
        let phase = self.phase();
        let legal_cards: Vec<_> = match phase {
            Phase::Attack | Phase::Throw => self.attack_cards().map(|card| card.0).collect(),
            Phase::Defend => self.defense_cards().map(|card| card.0).collect(),
            Phase::Finished => Vec::new(),
        };
        let (phase, actions, message): (&str, &[&str], &str) = match phase {
            Phase::Attack => ("attack", &["attack"], "Ваш ход. Выберите карту для атаки."),
            Phase::Defend if legal_cards.is_empty() => {
                ("defend", &["take"], "Заберите карты со стола.")
            }
            Phase::Defend => (
                "defend",
                &["defend", "take"],
                "Отбейте карту или заберите карты со стола.",
            ),
            Phase::Throw if self.bot_taking => (
                "throw",
                &["attack", "pass"],
                "Бот забирает карты. Можно подкинуть ещё или спасовать.",
            ),
            Phase::Throw => (
                "throw",
                &["attack", "pass"],
                "Подкиньте карту или завершите атаку.",
            ),
            Phase::Finished => {
                let message = match self.outcome.expect("finished game") {
                    Outcome::Won => "Вы выиграли!",
                    Outcome::Lost => "Бот выиграл.",
                    Outcome::Draw => "Ничья.",
                };
                ("finished", &[], message)
            }
        };
        let mut hand = self.user.clone();
        hand.sort_unstable_by_key(|card| (card.suit() == self.trump.suit(), card.0));
        let table: Vec<_> = self.table.iter().map(|pair| {
            json!({"attack": pair.attack.view(), "defense": pair.defense.map(Card::view)})
        }).collect();
        json!({
            "revision": revision,
            "status": status,
            "phase": phase,
            "hand": hand.into_iter().map(Card::view).collect::<Vec<_>>(),
            "trump": self.trump.view(),
            "deck_count": self.deck.len(),
            "opponent_count": self.bot.len(),
            "table": table,
            "legal_cards": legal_cards,
            "actions": actions,
            "message": message,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, seq::IndexedRandom, SeedableRng};

    #[test]
    fn start_returns_a_playable_game_scoped_to_guild_and_user() {
        let state = State::default();
        let body = json!({});
        assert_eq!(
            state.handle(1, 2, "state", &body).unwrap(),
            json!({"game": null})
        );

        let started = state.handle(1, 2, "start", &body).unwrap();
        let game = &started["game"];
        assert_eq!(game["status"], "playing");
        assert!(game["revision"].is_string());
        assert_eq!(game["deck_count"], 24);
        assert_eq!(game["hand"].as_array().unwrap().len(), 6);
        assert!(matches!(game["phase"].as_str(), Some("attack" | "defend")));
        assert_eq!(started, state.handle(1, 2, "state", &body).unwrap());
        assert_eq!(
            state.handle(1, 3, "state", &body).unwrap()["game"],
            Value::Null
        );
        assert_eq!(
            state.handle(4, 2, "state", &body).unwrap()["game"],
            Value::Null
        );
    }

    #[test]
    fn invalid_moves_leave_the_snapshot_unchanged() {
        let state = State::default();
        let started = state.handle(1, 2, "start", &json!({})).unwrap();
        let game = &started["game"];
        assert_eq!(
            state.handle(1, 2, "move", &json!({"revision": 1, "action": "pass"})),
            Err(STALE.into())
        );
        for card in [json!("1"), json!(1.0), json!(0), json!(256), Value::Null] {
            let body = json!({
                "revision": game["revision"],
                "action": game["phase"],
                "card": card,
            });
            assert_eq!(state.handle(1, 2, "move", &body), Err(ILLEGAL.into()));
        }
        assert_eq!(state.handle(1, 2, "state", &json!({})).unwrap(), started);
    }

    #[test]
    fn concurrent_moves_and_restarts_invalidate_previous_revisions() {
        let state = State::default();
        let started = state.handle(1, 2, "start", &json!({})).unwrap();
        let game = &started["game"];
        let move_body = if game["phase"] == "attack" {
            json!({"revision": game["revision"], "action": "attack", "card": game["legal_cards"][0]})
        } else {
            json!({"revision": game["revision"], "action": "take"})
        };
        let results = std::thread::scope(|scope| {
            let first = scope.spawn(|| state.handle(1, 2, "move", &move_body));
            let second = scope.spawn(|| state.handle(1, 2, "move", &move_body));
            [first.join().unwrap(), second.join().unwrap()]
        });
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .find_map(|result| result.as_ref().err())
                .unwrap(),
            STALE
        );
        let current = state.handle(1, 2, "state", &json!({})).unwrap();
        let restarted = state.handle(1, 2, "start", &json!({})).unwrap();
        assert_ne!(current["game"]["revision"], restarted["game"]["revision"]);
        let body = json!({"revision": current["game"]["revision"], "action": "take"});
        assert_eq!(state.handle(1, 2, "move", &body), Err(STALE.into()));
    }

    #[test]
    fn storage_is_bounded_and_restart_reuses_the_players_slot() {
        let state = State::default();
        let body = json!({});
        for user in 0..MAX_GAMES as u64 {
            state.handle(1, user, "start", &body).unwrap();
        }
        state.handle(1, 0, "start", &body).unwrap();
        assert_eq!(state.store.lock().unwrap().games.len(), MAX_GAMES);
        state.handle(1, MAX_GAMES as u64, "start", &body).unwrap();
        assert_eq!(state.store.lock().unwrap().games.len(), MAX_GAMES);
        assert!(state.handle(1, 0, "state", &body).unwrap()["game"].is_object());
        assert!(state.handle(1, 1, "state", &body).unwrap()["game"].is_null());
    }

    #[test]
    fn defense_requires_a_higher_same_suit_card_or_a_trump() {
        let spades = 3;
        assert!(Card(6).beats(Card(5), spades));
        assert!(!Card(4).beats(Card(5), spades));
        assert!(!Card(15).beats(Card(5), spades));
        assert!(Card(28).beats(Card(9), spades));
        assert!(!Card(9).beats(Card(28), spades));
        assert!(!Card(28).beats(Card(29), spades));
        assert!(Card(30).beats(Card(29), spades));
        assert_eq!(Card(9).view(), json!({"id": 9, "rank": "A", "suit": "♣"}));
    }

    #[test]
    fn lowest_dealt_trump_determines_the_first_attacker() {
        let mut rng = StdRng::seed_from_u64(1);
        let mut deck: Vec<_> = (1..=DECK_SIZE).map(Card).collect();
        deck.swap(1, 35);
        deck.swap(2, 34);
        let game = Game::deal(deck.clone(), &mut rng);
        assert_eq!(game.trump, Card(1));
        assert_eq!(game.phase(), Phase::Attack);

        deck.swap(34, 35);
        let game = Game::deal(deck, &mut rng);
        assert_eq!(game.phase(), Phase::Defend);
        assert_eq!(game.table[0].attack, Card(29));
    }

    fn fixture(user: &[u8], bot: &[u8], stock: &[u8], attacker: Side) -> Game {
        let discard = (1..=DECK_SIZE)
            .filter(|card| !user.contains(card) && !bot.contains(card) && !stock.contains(card))
            .map(Card)
            .collect();
        Game {
            user: user.iter().copied().map(Card).collect(),
            bot: bot.iter().copied().map(Card).collect(),
            deck: stock.iter().copied().map(Card).collect(),
            discard,
            trump: Card(36),
            table: Vec::new(),
            attacker,
            attack_limit: HAND_SIZE.min(if attacker == Side::User {
                bot.len()
            } else {
                user.len()
            }),
            bot_taking: false,
            outcome: None,
        }
    }

    #[test]
    fn throw_matches_a_defense_rank_and_pass_resolves_a_bot_pickup() {
        let mut game = fixture(&[1, 11, 20, 12], &[2, 3, 22, 23], &[], Side::User);
        game.play(&json!({"action": "attack", "card": 1})).unwrap();
        let view = game.view("rev");
        assert_eq!(view["phase"], "throw");
        assert_eq!(view["legal_cards"], json!([11, 20]));
        assert_eq!(view["table"][0]["defense"]["id"], 2);
        assert_eq!(
            game.play(&json!({"action": "attack", "card": 12})),
            Err(ILLEGAL)
        );

        game.play(&json!({"action": "attack", "card": 11})).unwrap();
        let view = game.view("rev");
        assert_eq!(view["actions"], json!(["attack", "pass"]));
        assert_eq!(view["legal_cards"], json!([20]));
        assert!(view["table"][1]["defense"].is_null());
        game.play(&json!({"action": "pass"})).unwrap();
        assert_eq!(game.phase(), Phase::Attack);
        assert_eq!(
            game.bot,
            [Card(3), Card(22), Card(23), Card(1), Card(2), Card(11)]
        );
        assert!(game.table.is_empty());
    }

    #[test]
    fn user_can_take_after_defending_and_bot_respects_the_original_hand_limit() {
        let mut game = fixture(&[2, 14, 15, 16], &[1, 11, 20, 29, 21, 24], &[], Side::Bot);
        game.advance();
        assert_eq!(game.table[0].attack, Card(1));
        game.play(&json!({"action": "defend", "card": 2})).unwrap();
        let view = game.view("rev");
        assert_eq!(view["actions"], json!(["defend", "take"]));
        assert_eq!(view["table"][1]["attack"]["id"], 11);

        game.play(&json!({"action": "take"})).unwrap();
        assert_eq!(
            game.user,
            [
                Card(14),
                Card(15),
                Card(16),
                Card(1),
                Card(2),
                Card(11),
                Card(20),
                Card(29)
            ]
        );
        assert_eq!(game.table[0].attack, Card(21));
        assert_eq!(game.bot, [Card(24)]);
        assert_eq!(game.view("rev")["legal_cards"], json!([29]));
    }

    #[test]
    fn successful_defense_discards_the_table_and_swaps_roles_after_attacker_refills_first() {
        let mut game = fixture(&[1, 8], &[2, 9], &[36, 5], Side::User);
        game.put_attack(Card(1));
        game.put_defense(Card(2));
        game.finish_bout(false);
        assert_eq!(game.attacker, Side::Bot);
        assert_eq!(game.attack_limit, 3);
        assert_eq!(game.user, [Card(8), Card(5), Card(36)]);
        assert_eq!(game.bot, [Card(9)]);
        assert!(game.deck.is_empty());
        assert!(game.table.is_empty());
        assert!(game.discard.ends_with(&[Card(1), Card(2)]));
        assert_eq!(game.outcome, None);
    }

    #[test]
    fn six_attacks_end_the_bout_even_with_another_matching_card() {
        let mut game = fixture(
            &[1, 2, 3, 4, 5, 6, 7],
            &[29, 30, 31, 32, 33, 34, 10],
            &[],
            Side::User,
        );
        for card in 1..=6 {
            game.play(&json!({"action": "attack", "card": card}))
                .unwrap();
        }
        assert_eq!(game.user, [Card(7)]);
        assert_eq!(game.phase(), Phase::Defend);
        assert_eq!(game.table.len(), 1);
        assert_eq!(game.table[0].attack, Card(10));
        assert_eq!(game.view("rev")["actions"], json!(["take"]));
        assert_eq!(game.view("rev")["legal_cards"], json!([]));
    }

    #[test]
    fn endgame_is_scored_after_the_final_bout() {
        for (user, bot, outcome, status) in [
            (&[1][..], &[2, 3][..], Outcome::Won, "won"),
            (&[1, 4][..], &[2][..], Outcome::Lost, "lost"),
            (&[1][..], &[2][..], Outcome::Draw, "draw"),
        ] {
            let mut game = fixture(user, bot, &[], Side::User);
            game.play(&json!({"action": "attack", "card": 1})).unwrap();
            assert_eq!(game.outcome, Some(outcome));
            let view = game.view("rev");
            assert_eq!(view["status"], status);
            assert_eq!(view["phase"], "finished");
            assert_eq!(view["actions"], json!([]));
            assert_eq!(view["legal_cards"], json!([]));
            assert!(game.play(&json!({"action": "take"})).is_err());
        }

        let mut game = fixture(&[1], &[2], &[36], Side::User);
        game.play(&json!({"action": "attack", "card": 1})).unwrap();
        assert_eq!(game.outcome, Some(Outcome::Lost));
        assert_eq!(game.user, [Card(36)]);
    }

    #[test]
    fn complete_games_preserve_every_card() {
        let mut rng = StdRng::seed_from_u64(42);
        for deal in 0..128 {
            let mut deck: Vec<_> = (1..=DECK_SIZE).map(Card).collect();
            deck.shuffle(&mut rng);
            let mut game = Game::deal(deck, &mut rng);
            for _ in 0..400 {
                let mut cards: Vec<_> = game
                    .user
                    .iter()
                    .chain(&game.bot)
                    .chain(&game.deck)
                    .chain(&game.discard)
                    .map(|card| card.0)
                    .collect();
                for pair in &game.table {
                    cards.push(pair.attack.0);
                    cards.extend(pair.defense.map(|card| card.0));
                }
                cards.sort_unstable();
                assert!(cards.into_iter().eq(1..=DECK_SIZE), "deal {deal}");
                assert!(game.table.len() <= game.attack_limit && game.attack_limit <= HAND_SIZE);
                if let Some(bottom) = game.deck.first() {
                    assert_eq!(*bottom, game.trump);
                }

                let view = game.view("rev");
                let legal = view["legal_cards"].as_array().unwrap();
                let body = match game.phase() {
                    Phase::Attack => {
                        json!({"action": "attack", "card": legal.choose(&mut rng).unwrap()})
                    }
                    Phase::Throw if rng.random_ratio(1, 4) => json!({"action": "pass"}),
                    Phase::Throw => {
                        json!({"action": "attack", "card": legal.choose(&mut rng).unwrap()})
                    }
                    Phase::Defend if legal.is_empty() || rng.random_ratio(1, 5) => {
                        json!({"action": "take"})
                    }
                    Phase::Defend => {
                        json!({"action": "defend", "card": legal.choose(&mut rng).unwrap()})
                    }
                    Phase::Finished => break,
                };
                game.play(&body).unwrap();
            }
            assert!(game.outcome.is_some(), "deal {deal} did not finish");
        }
    }
}
