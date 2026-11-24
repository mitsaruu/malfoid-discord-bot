//! Питомцы и кейсы: /case, /cases, /pets, /pets_collect, /pet_feed, /pet_sell.
//! Питомец сам приносит монеты в час; корм поднимает уровень (+25% к доходу за уровень).

use crate::db::{self, Database, Pet, Wallet};
use crate::reply::{challenge, display_name, finish_challenge, insufficient_money, say, user_name, whisper};
use crate::text::{hours_text, money_text, number};
use crate::{Context, Error};
use poise::serenity_prelude as serenity;
use rand::Rng;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::collections::BTreeMap;

mod contracts;
mod leaderboard;
pub(crate) use contracts::{contract_quote, sign_contract, ContractQuote};
pub(crate) use leaderboard::player_leaderboard;

pub(crate) const ACTIVE_PET_LIMIT: i64 = 10;
pub(crate) const MAX_PET_LIMIT: usize = 40;
pub(crate) const UPGRADE_SCALE: u32 = 10_000;
const UPGRADE_CHANCE_SCALE: u32 = 9_000;
const PET_VALUE_PER_HOUR: i64 = 30;
const OLEG_FEED_COST: i64 = 1_000_000;
const LEGACY_PET_LIMIT: i64 = 10;

/// Настройки из settings.ini, раздел [pets].
pub struct PetSettings {
    pub max_pets: i64,
    pub max_level: i64,
    pub collect_cap_hours: i64,
    pub case_prices: [i64; 4], // дешёвый, средний, дорогой, секретный
}

impl PetSettings {
    pub fn load(settings: &crate::config::Settings) -> Self {
        let capacity = MAX_PET_LIMIT as i64;
        let configured_limit = settings.int("pets", "max_pets", capacity);
        Self {
            // Existing servers can keep the previous default in their settings file.
            max_pets: if configured_limit == LEGACY_PET_LIMIT { capacity } else { configured_limit.clamp(1, capacity) },
            max_level: settings.int("pets", "max_level", 20).max(1),
            collect_cap_hours: settings.int("pets", "collect_cap_hours", 24).max(1),
            case_prices: [
                settings.int("pets", "cheap_case_price", 1000).max(1),
                settings.int("pets", "medium_case_price", 3000).max(1),
                settings.int("pets", "expensive_case_price", 9000).max(1),
                settings.int("pets", "secret_case_price", 30000).max(1),
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, poise::ChoiceParameter)]
pub enum Case {
    #[name = "📦 Дешёвый"]
    Cheap,
    #[name = "🎁 Средний"]
    Medium,
    #[name = "💎 Дорогой"]
    Expensive,
    #[name = "🔮 Секретный"]
    Secret,
}

impl Case {
    pub(crate) const ALL: [Case; 4] = [Case::Cheap, Case::Medium, Case::Expensive, Case::Secret];
    pub(crate) fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|case| case.key() == key)
    }


    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Cheap => "cheap",
            Self::Medium => "medium",
            Self::Expensive => "expensive",
            Self::Secret => "secret",
        }
    }

    fn emoji(self) -> &'static str {
        ["📦", "🎁", "💎", "🔮"][self as usize]
    }

    pub(crate) fn title(self) -> &'static str {
        ["Дешёвый", "Средний", "Дорогой", "Секретный"][self as usize]
    }

    pub(crate) fn species(self) -> impl Iterator<Item = &'static Species> {
        RARITIES.iter().rev().filter(move |rarity| rarity.chances[self as usize] > 0.0)
            .flat_map(|rarity| SPECIES.iter().filter(move |species| species.rarity == rarity.key))
    }
}

/// Редкость: название, цвет карточки и шанс выпасть в каждом кейсе (в процентах, в каждом кейсе сумма — 100).
pub struct Rarity {
    pub key: &'static str,
    pub name: &'static str,
    pub color: u32,
    pub chances: [f64; 4], // дешёвый, средний, дорогой, секретный
}

pub const RARITIES: [Rarity; 6] = [
    Rarity { key: "common", name: "Обычный", color: 0x9CA3AF, chances: [75.0, 40.0, 0.0, 0.0] },
    Rarity { key: "rare", name: "Редкий", color: 0x3B82F6, chances: [20.0, 38.0, 40.0, 0.0] },
    Rarity { key: "epic", name: "Эпический", color: 0xA855F7, chances: [4.5, 17.0, 40.0, 60.0] },
    Rarity { key: "legendary", name: "Легендарный", color: 0xF59E0B, chances: [0.49, 4.8, 18.5, 34.95] },
    Rarity { key: "mythic", name: "Мифический", color: 0xEF4444, chances: [0.01, 0.2, 1.5, 4.95] },
    Rarity { key: "secret", name: "Секретный", color: 0x22D3EE, chances: [0.0, 0.0, 0.0, 0.1] },
];

/// Вид питомца: ключ из базы, эмодзи, название, редкость и базовый доход.
pub struct Species {
    pub key: &'static str,
    pub emoji: &'static str,
    pub name: &'static str,
    pub rarity: &'static str,
    pub income: i64,
}

const fn species(
    key: &'static str, emoji: &'static str, name: &'static str, rarity: &'static str, income: i64,
) -> Species {
    Species { key, emoji, name, rarity, income }
}

// Средняя цена продажи выпавшего питомца ниже стандартной цены каждого кейса.
pub const SPECIES: [Species; 19] = [
    species("valera", "🐱", "котик", "common", 10),
    species("gena", "🐹", "хомяк", "common", 12),
    species("tolik", "🐦", "голубь", "common", 8),
    species("frog", "🐸", "лягушка", "common", 9),
    species("arkady", "🦊", "лисичка", "rare", 25),
    species("tamara", "🦉", "сова", "rare", 25),
    species("boris", "🐢", "черепаха", "rare", 22),
    species("igor", "🐺", "волк", "epic", 60),
    species("edik", "🐧", "пингвин", "epic", 55),
    species("zhorik", "🦝", "енот", "epic", 65),
    species("hermes", "👟", "Гермес", "epic", 62),
    species("vasily", "🐉", "дракон", "legendary", 150),
    species("lyudmila", "🦄", "единорог", "legendary", 140),
    species("lion", "🦁", "лев", "legendary", 160),
    species("danya_dog", "🐕", "Даня собака", "legendary", 170),
    species("sergey", "🐶", "собака Сергей", "mythic", 500),
    species("foidka_romana", "🧚", "Фоидка романа", "mythic", 550),
    species("fat_oleg", "👤", "Жирный Олег", "mythic", 533),
    species("stellochka", "🌟", "Стеллочка", "secret", 2000),
];

const STAGES: [(i64, &str); 4] = [(1, "малыш"), (5, "подросток"), (10, "взрослый"), (15, "легенда района")];

pub fn find_species(key: &str) -> Option<&'static Species> {
    SPECIES.iter().find(|species| species.key == key)
}

pub(crate) fn rarity_of(species: &Species) -> &'static Rarity {
    RARITIES.iter().find(|rarity| rarity.key == species.rarity).unwrap()
}

const SECRET_PET_FILENAME: &str = "stellochka.webp";

pub(crate) async fn secret_pet_art(data: &crate::Data) -> Option<Vec<u8>> {
    tokio::fs::read(data.dir.join("miniapp/pets").join(SECRET_PET_FILENAME)).await.ok()
}

fn needs_sale_confirmation(species: &Species) -> bool {
    species.rarity != "common"
}

pub fn stage(level: i64) -> &'static str {
    STAGES.iter().rev().find(|(start, _)| level >= *start).map(|(_, name)| *name).unwrap_or(STAGES[0].1)
}

/// Монет в час у питомца этого вида и уровня (округление — как round() в Python: половинки к чётному).
pub fn income(species: &Species, level: i64) -> i64 {
    let extra_levels = level.max(1) - 1;
    if species.key == "fat_oleg" {
        return species.income.saturating_add(extra_levels);
    }
    (species.income as f64 * (1.0 + 0.25 * extra_levels as f64)).round_ties_even() as i64
}

pub(crate) fn value(species: &Species, level: i64) -> i64 {
    income(species, level).saturating_mul(PET_VALUE_PER_HOUR)
}

/// Сколько стоит покормить питомца, чтобы он вырос с level до level + 1.
pub fn feed_cost(species: &Species, level: i64) -> i64 {
    if species.key == "fat_oleg" {
        OLEG_FEED_COST
    } else {
        (species.income * 20).saturating_mul(level.max(1))
    }
}

/// «🐱 котик (ур. 3, малыш)»
pub fn title(species: &Species, level: i64) -> String {
    format!("{} {} (ур. {level}, {})", species.emoji, species.name, stage(level))
}

/// Сколько монет питомец накопил с последнего сбора (не больше, чем за cap_hours часов).
pub fn pending(species: &Species, level: i64, last_collect: f64, now: f64, cap_hours: i64) -> i64 {
    let hours = ((now - last_collect) / 3600.0).clamp(0.0, cap_hours as f64);
    (income(species, level) as f64 * hours) as i64
}

fn income_clock(pet: &Pet, now: f64) -> f64 {
    pet.inactive_since.map_or(now, |paused| now.min(paused))
}

fn owned_pending(pet: &Pet, species: &Species, now: f64, cap_hours: i64) -> i64 {
    pending(species, pet.level, pet.last_collect, income_clock(pet, now), cap_hours)
}

/// Случайный вид из кейса: сначала редкость по шансам (roll — число от 0 до 100), потом вид внутри редкости.
pub fn roll_pet(case: Case, roll: f64, pick: usize) -> &'static Species {
    let mut left = roll;
    let mut rarity = RARITIES.iter().rev().find(|rarity| rarity.chances[case as usize] > 0.0).unwrap();
    for candidate in RARITIES.iter() {
        let chance = candidate.chances[case as usize];
        if chance > 0.0 && left < chance {
            rarity = candidate;
            break;
        }
        left -= chance;
    }
    let options: Vec<&Species> = SPECIES.iter().filter(|species| species.rarity == rarity.key).collect();
    options[pick % options.len()]
}

/// Питомцы человека вместе с видом, от самого доходного к самому слабому (вид, которого больше нет, пропускаем).
pub(crate) fn sorted_pets(pets: Vec<Pet>) -> Vec<(Pet, &'static Species)> {
    let mut pets: Vec<(Pet, &'static Species)> =
        pets.into_iter().filter_map(|pet| find_species(&pet.species).map(|species| (pet, species))).collect();
    pets.sort_by_key(|(pet, species)| (-income(species, pet.level), pet.id));
    pets
}

fn buy_pet(
    tx: &Transaction<'_>,
    wallet: Wallet<'_>,
    face: &str,
    kind: Case,
    species: &Species,
    price: i64,
    max_pets: i64,
) -> rusqlite::Result<Result<(i64, i64), String>> {
    let count = Database::pet_count_on(tx, wallet.guild, wallet.user)?;
    if count >= max_pets {
        return Ok(Err(format!(
            "🏠 У тебя уже {max_pets} питомцев — больше не поместится. Продай кого-нибудь: /pet_sell")));
    }
    let (balance, _) = wallet.open(tx)?;
    if balance < price {
        return Ok(Err(insufficient_money(balance, price, "цена кейса")));
    }
    let balance = db::change_balance(tx, wallet.guild, wallet.user, -price)?;
    let now = db::now();
    let active = Database::active_pet_count_on(tx, wallet.guild, wallet.user)? < ACTIVE_PET_LIMIT;
    insert_owned_pet(tx, wallet, (species, 1), now, price, active)?;
    tx.execute(
        "INSERT INTO case_openings (guild_id, user_id, name, face, species, case_key) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![wallet.guild as i64, wallet.user as i64, wallet.name, face, species.key, kind.key()],
    )?;
    Ok(Ok((tx.last_insert_rowid(), balance)))
}

fn insert_owned_pet(
    tx: &Transaction<'_>, wallet: Wallet<'_>, (species, level): (&Species, i64), now: f64, spent: i64, active: bool,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO pets (guild_id, user_id, species, level, last_collect, spent, inactive_since)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![wallet.guild as i64, wallet.user as i64, species.key, level, now, spent, (!active).then_some(now)],
    )?;
    Ok(())
}

pub(crate) fn move_pet(
    database: &Database, wallet: Wallet<'_>, id: i64, active: bool, replace: Option<i64>, now: f64,
) -> rusqlite::Result<Result<(), &'static str>> {
    database.transaction(|tx| {
        let find = |id| Database::own_pet_on(tx, id, wallet.guild, wallet.user);
        let missing = "Питомец уже недоступен. Обнови склад.";
        let Some(pet) = find(id)? else { return Ok(Err(missing)); };
        if active == pet.inactive_since.is_none() {
            return Ok(if replace.is_none() { Ok(()) } else { Err("Состав уже изменился. Обнови склад.") });
        }
        if let Some(other_id) = replace {
            let Some(other) = find(other_id)? else { return Ok(Err(missing)); };
            if other.inactive_since.is_none() != active {
                return Ok(Err("Для замены выбери питомца из другой группы."));
            }
            Database::set_pet_activity_on(tx, &other, !active, now)?;
        } else if active && Database::active_pet_count_on(tx, wallet.guild, wallet.user)? >= ACTIVE_PET_LIMIT {
            return Ok(Err("Все активные места заняты. Выбери питомца для замены."));
        }
        Database::set_pet_activity_on(tx, &pet, active, now)?;
        Ok(Ok(()))
    })
}

pub(crate) struct PetUpgrade {
    pub won: bool,
    pub chance: u32,
    pub roll: u32,
    pub target: &'static Species,
    pub balance: i64,
}

type PetStakes = Vec<(Pet, &'static Species)>;

fn owned_stakes(
    connection: &Connection, wallet: Wallet<'_>, stakes: &BTreeMap<i64, i64>,
) -> rusqlite::Result<Result<PetStakes, &'static str>> {
    let mut selected = Vec::with_capacity(stakes.len());
    for (&id, &level) in stakes {
        let found = Database::own_pet_on(connection, id, wallet.guild, wallet.user)?
            .and_then(|pet| find_species(&pet.species).map(|species| (pet, species)));
        let Some((pet, species)) = found else {
            return Ok(Err("Один из питомцев уже недоступен. Обнови список."));
        };
        if pet.level.max(1) != level {
            return Ok(Err("Уровень изменился. Выбери питомцев заново."));
        }
        selected.push((pet, species));
    }
    Ok(Ok(selected))
}

fn stake_value(selected: &[(Pet, &Species)]) -> i128 {
    selected.iter().map(|(pet, species)| i128::from(value(species, pet.level))).sum()
}

fn consume_stakes(
    tx: &Transaction<'_>, wallet: Wallet<'_>, selected: &[(Pet, &Species)], now: f64, cap_hours: i64,
) -> rusqlite::Result<(i64, bool)> {
    let earned = selected.iter().try_fold(0_i64, |sum, (pet, species)| {
        sum.checked_add(owned_pending(pet, species, now, cap_hours))
            .ok_or(rusqlite::Error::IntegralValueOutOfRange(0, sum))
    })?;
    let (balance, _) = wallet.open(tx)?;
    let balance = if earned > 0 { db::change_balance(tx, wallet.guild, wallet.user, earned)? } else { balance };
    let active = selected.iter().any(|(pet, _)| pet.inactive_since.is_none());
    for (pet, _) in selected {
        tx.execute("DELETE FROM pets WHERE id = ?1", [pet.id])?;
    }
    Ok((balance, active))
}

pub(crate) fn upgrade_pet(
    database: &Database, wallet: Wallet<'_>, face: &str, stakes: &BTreeMap<i64, i64>, target: &'static Species,
    cap_hours: i64, roll: impl FnOnce() -> u32,
) -> rusqlite::Result<Result<PetUpgrade, &'static str>> {
    if stakes.is_empty() || stakes.len() > MAX_PET_LIMIT {
        return Ok(Err("Выбери питомцев для ставки."));
    }
    database.transaction(|tx| {
        let selected = match owned_stakes(tx, wallet, stakes)? {
            Ok(selected) => selected,
            Err(message) => return Ok(Err(message)),
        };
        let source_value = stake_value(&selected);
        let target_value = i128::from(value(target, 1));
        if target.rarity == "secret" || source_value >= target_value {
            return Ok(Err("Выбери доступного питомца дороже ставки."));
        }
        // На шкале из 10 000 исходов используем 90% от отношения стоимостей.
        let chance = (source_value * i128::from(UPGRADE_CHANCE_SCALE) / target_value) as u32;
        if chance == 0 {
            return Ok(Err("Разница в стоимости слишком велика."));
        }
        let roll = roll();
        let won = roll < chance;
        let now = db::now();
        let (balance, active) = consume_stakes(tx, wallet, &selected, now, cap_hours)?;
        if won {
            insert_owned_pet(tx, wallet, (target, 1), now, source_value as i64, active)?;
        }
        let sources: Vec<_> = selected.iter().map(|(pet, species)| (species.key, pet.level.max(1))).collect();
        tx.execute(
            "INSERT INTO case_openings (guild_id, user_id, name, face, species, upgrade) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![wallet.guild as i64, wallet.user as i64, wallet.name, face, target.key,
                serde_json::json!([sources, won]).to_string()],
        )?;
        Ok(Ok(PetUpgrade { won, chance, roll, target, balance }))
    })
}

#[derive(Clone, Copy)]
pub(crate) struct CaseRequest<'a> {
    pub id: &'a str,
    pub price: i64,
}

pub(crate) fn buy_case(
    database: &Database,
    wallet: Wallet<'_>,
    face: &str,
    settings: &PetSettings,
    kind: Case,
    request: Option<CaseRequest<'_>>,
) -> rusqlite::Result<Result<(&'static Species, i64), String>> {
    database.transaction(|tx| {
        let price = settings.case_prices[kind as usize];
        if let Some(request) = request {
            let earlier: Option<(String, String)> = tx.query_row(
                "SELECT requests.case_key, openings.species FROM case_opening_requests AS requests
                 JOIN case_openings AS openings ON openings.id = requests.opening_id
                 WHERE requests.guild_id = ?1 AND requests.user_id = ?2 AND requests.request_id = ?3",
                params![wallet.guild as i64, wallet.user as i64, request.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?;
            if let Some((case_key, species)) = earlier {
                if case_key != kind.key() {
                    return Ok(Err("Этот запрос уже использован для другого кейса.".into()));
                }
                let species = find_species(&species).ok_or(rusqlite::Error::InvalidQuery)?;
                return Ok(Ok((species, wallet.open(tx)?.0)));
            }
            if request.price != price {
                return Ok(Err("Цена кейса изменилась. Проверь новую цену.".into()));
            }
        }

        let mut rng = rand::rng();
        let species = roll_pet(kind, rng.random_range(0.0..100.0), rng.random_range(0..usize::MAX));
        let (opening_id, balance) = match buy_pet(tx, wallet, face, kind, species, price, settings.max_pets)? {
            Ok(opening) => opening,
            Err(reason) => return Ok(Err(reason)),
        };
        if let Some(request) = request {
            tx.execute(
                "INSERT INTO case_opening_requests (guild_id, user_id, request_id, case_key, opening_id)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![wallet.guild as i64, wallet.user as i64, request.id, kind.key(), opening_id],
            )?;
        }
        Ok(Ok((species, balance)))
    })
}

pub(crate) struct CaseOpening {
    pub id: i64,
    pub user: u64,
    pub name: String,
    pub species: String,
    pub kind: Option<Case>,
    pub upgrade: Option<UpgradeOpening>,
}

pub(crate) struct UpgradeOpening {
    pub stakes: Vec<(String, i64)>,
    pub won: bool,
}

impl UpgradeOpening {
    pub(crate) fn most_expensive_stake(&self) -> Option<(&'static Species, i64)> {
        self.stakes.iter().filter_map(|(key, level)| find_species(key).map(|species| (species, *level)))
            .max_by_key(|(species, level)| value(species, *level))
    }
}

pub(crate) fn case_history(database: &Database, guild: u64) -> rusqlite::Result<Vec<CaseOpening>> {
    let connection = database.lock();
    let mut statement = connection.prepare(
        "SELECT id, user_id, name, species, case_key, upgrade FROM case_openings
         WHERE guild_id = ?1 ORDER BY id DESC LIMIT 12",
    )?;
    let openings = statement.query_map([guild as i64], |row| {
        let case_key: Option<String> = row.get(4)?;
        let upgrade: Option<String> = row.get(5)?;
        let upgrade = upgrade.as_deref().map(|json| {
            serde_json::from_str(json).map(|(stakes, won)| UpgradeOpening { stakes, won })
                .map_err(|error| rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(error)))
        }).transpose()?;
        Ok(CaseOpening {
            id: row.get(0)?,
            user: row.get::<_, i64>(1)? as u64,
            name: row.get(2)?,
            species: row.get(3)?,
            kind: case_key.as_deref().and_then(Case::from_key),
            upgrade,
        })
    })?.collect();
    openings
}

pub(crate) fn case_opening_avatar(database: &Database, guild: u64, opening: i64) -> rusqlite::Result<Option<String>> {
    database.lock().query_row(
        "SELECT face FROM case_openings WHERE id = ?1 AND guild_id = ?2",
        params![opening, guild as i64],
        |row| row.get(0),
    ).optional()
}

pub(crate) fn has_opened_stellochka(database: &Database, guild: u64, user: u64) -> rusqlite::Result<bool> {
    database.lock().query_row(
        "SELECT EXISTS(
             SELECT 1 FROM case_openings WHERE guild_id = ?1 AND user_id = ?2 AND species = 'stellochka' AND upgrade IS NULL
         )",
        params![guild as i64, user as i64],
        |row| row.get(0),
    )
}

pub(crate) fn collect_pets(
    database: &Database,
    wallet: Wallet<'_>,
    cap_hours: i64,
    now: f64,
) -> rusqlite::Result<Result<(i64, i64), &'static str>> {
    database.transaction(|tx| {
        let pets = sorted_pets(Database::pets_on(tx, wallet.guild, wallet.user)?);
        if pets.is_empty() {
            return Ok(Err("🐾 Питомцев пока нет. Открой кейс: /case"));
        }
        let total = pets.iter().try_fold(0_i64, |total, (pet, species)| {
            total.checked_add(owned_pending(pet, species, now, cap_hours))
                .ok_or(rusqlite::Error::IntegralValueOutOfRange(0, total))
        })?;
        if total <= 0 {
            return Ok(Err("⏳ Питомцы ещё ничего не накопили — загляни попозже."));
        }
        wallet.open(tx)?;
        let balance = db::change_balance(tx, wallet.guild, wallet.user, total)?;
        for (pet, _) in &pets {
            tx.execute(
                "UPDATE pets SET last_collect = ?2 WHERE id = ?1 AND COALESCE(last_collect, 0) < ?2",
                params![pet.id, income_clock(pet, now)],
            )?;
        }
        Ok(Ok((total, balance)))
    })
}

pub(crate) struct Feeding {
    pub pet: Pet,
    pub species: &'static Species,
    pub previous_level: i64,
    pub cost: i64,
    pub balance: i64,
}

pub(crate) fn feed_owned_pet(
    database: &Database,
    wallet: Wallet<'_>,
    id: i64,
    settings: &PetSettings,
    expected_level: Option<i64>,
) -> rusqlite::Result<Result<Feeding, String>> {
    database.transaction(|tx| {
        let found = Database::own_pet_on(tx, id, wallet.guild, wallet.user)?
            .and_then(|pet| find_species(&pet.species).map(|species| (pet, species)));
        let Some((pet, species)) = found else {
            return Ok(Err("Такого питомца у тебя нет — выбери из списка.".into()));
        };
        if expected_level.is_some_and(|level| level != pet.level.max(1)) {
            return Ok(Err("Уровень питомца уже изменился. Проверь новый уровень и стоимость.".into()));
        }
        if pet.level >= settings.max_level {
            return Ok(Err(format!("{} уже вырос до максимума!", title(species, pet.level))));
        }
        let cost = feed_cost(species, pet.level);
        let (balance, _) = wallet.open(tx)?;
        if balance < cost {
            return Ok(Err(insufficient_money(balance, cost, "корм")));
        }
        let spent = pet.spent.checked_add(cost).ok_or(rusqlite::Error::IntegralValueOutOfRange(0, pet.spent))?;
        let now = income_clock(&pet, db::now());
        let saved = pending(species, pet.level, pet.last_collect, now, settings.collect_cap_hours);
        let new_level = pet.level.max(1) + 1;
        let mut last_collect = now - saved as f64 / income(species, new_level) as f64 * 3600.0;
        if pending(species, new_level, last_collect, now, settings.collect_cap_hours) < saved {
            // The timestamp conversion can round up by one float step.
            last_collect = last_collect.next_down();
        }
        let balance = db::change_balance(tx, wallet.guild, wallet.user, -cost)?;
        tx.execute(
            "UPDATE pets SET level = ?2, spent = ?3, last_collect = ?4 WHERE id = ?1",
            params![pet.id, new_level, spent, last_collect],
        )?;
        Ok(Ok(Feeding {
            previous_level: pet.level,
            pet: Pet { level: new_level, spent, last_collect, ..pet },
            species, cost, balance,
        }))
    })
}

/// Подсказки в поле «питомец»: список своих питомцев.
async fn pet_autocomplete(ctx: Context<'_>, partial: &str) -> serenity::CreateAutocompleteResponse {
    owned_pet_autocomplete(ctx, partial, false)
}

async fn pet_sale_autocomplete(ctx: Context<'_>, partial: &str) -> serenity::CreateAutocompleteResponse {
    owned_pet_autocomplete(ctx, partial, true)
}

fn owned_pet_autocomplete(ctx: Context<'_>, partial: &str, show_sale_price: bool) -> serenity::CreateAutocompleteResponse {
    let response = serenity::CreateAutocompleteResponse::new();
    let Some(guild) = ctx.guild_id() else { return response };
    let pets = ctx.data().db.user_pets(guild.get(), ctx.author().id.get()).unwrap_or_default();
    let partial = partial.to_lowercase();
    let choices: Vec<serenity::AutocompleteChoice> = sorted_pets(pets)
        .into_iter()
        .enumerate()
        .map(|(index, (pet, species))| (format!("{}. {}", index + 1, title(species, pet.level)), pet, species))
        .filter(|(label, _, _)| label.to_lowercase().contains(&partial))
        .take(25)
        .map(|(mut label, pet, species)| {
            if show_sale_price {
                label.push_str(&format!(" — {}", money_text(value(species, pet.level))));
            }
            serenity::AutocompleteChoice::new(label, pet.id)
        })
        .collect();
    response.set_choices(choices)
}

/// Открыть кейс с питомцем: дешёвый, средний, дорогой или секретный
#[poise::command(slash_command, guild_only, rename = "case")]
pub async fn open_case(ctx: Context<'_>, #[description = "Какой кейс открыть"] kind: Case) -> Result<(), Error> {
    let (data, guild, user) = (ctx.data(), ctx.guild_id().unwrap().get(), ctx.author().id.get());
    let (name, face) = match ctx.author_member().await {
        Some(member) => (member.display_name().to_string(), member.face()),
        None => (user_name(ctx.author()), ctx.author().face()),
    };
    let wallet = Wallet { guild, user, name: &name, start: data.economy.start_balance };
    let (species, _) = match buy_case(&data.db, wallet, &face, &data.pets, kind, None)? {
        Ok(opening) => opening,
        Err(reason) => return whisper(ctx, reason).await,
    };

    let no_pings = serenity::CreateAllowedMentions::new();
    let message = ctx
        .send(poise::CreateReply::default()
            .content(format!("{} <@{user}> открывает {} кейс...", kind.emoji(), kind.title().to_lowercase()))
            .allowed_mentions(no_pings.clone()))
        .await?;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await; // немного интриги
    let rarity = rarity_of(species);
    let ending = if rarity.key == "secret" { "!!! Самый редкий в игре!" } else { "!" };
    let mut embed = serenity::CreateEmbed::new()
        .color(rarity.color)
        .title(format!("{} {}", species.emoji, species.name))
        .description(format!(
            "**{}** питомец{ending}\nПриносит **{} в час**.", rarity.name, money_text(income(species, 1))))
        .author(serenity::CreateEmbedAuthor::new(name).icon_url(ctx.author().face()))
        .footer(serenity::CreateEmbedFooter::new("Забрать доход — /pets_collect · вырастить — /pet_feed"));
    let mut reply = poise::CreateReply::default().content("").allowed_mentions(no_pings);
    if rarity.key == "secret" {
        if let Some(art) = secret_pet_art(data).await {
            embed = embed.image(format!("attachment://{SECRET_PET_FILENAME}"));
            reply = reply.attachment(serenity::CreateAttachment::bytes(art, SECRET_PET_FILENAME));
        }
    }
    message.edit(ctx, reply.embed(embed)).await?;
    Ok(())
}

fn case_catalogue(prices: [i64; 4]) -> String {
    let mut blocks = vec![Case::ALL
        .iter()
        .map(|case| format!("{} **{}** — {}", case.emoji(), case.title(), money_text(prices[*case as usize])))
        .collect::<Vec<_>>()
        .join(" · ")];
    for rarity in RARITIES.iter() {
        let mut kinds: Vec<&Species> = SPECIES.iter().filter(|species| species.rarity == rarity.key).collect();
        kinds.sort_by_key(|species| species.income); // от меньшего дохода к большему
        let pets = kinds
            .iter()
            .map(|species| format!(
                "{} {} — {}/ч, корм от {}", species.emoji, species.name, money_text(species.income),
                number(feed_cost(species, 1))))
            .collect::<Vec<_>>()
            .join("\n");
        let odds = Case::ALL
            .iter()
            .map(|case| format!("{} {}%", case.emoji(), rarity.chances[*case as usize]))
            .collect::<Vec<_>>()
            .join(" · ");
        blocks.push(format!("**{}** — {odds}\n{pets}", rarity.name));
    }
    blocks.join("\n\n")
}

/// Какие питомцы бывают в кейсах и с каким шансом
#[poise::command(slash_command)]
pub async fn cases(ctx: Context<'_>) -> Result<(), Error> {
    let embed = serenity::CreateEmbed::new()
        .color(0x8B5CF6)
        .title("🎁 Кейсы с питомцами")
        .description(case_catalogue(ctx.data().pets.case_prices))
        .footer(serenity::CreateEmbedFooter::new(
            "Корм поднимает питомца на 1 уровень (+25% к доходу) и дорожает с каждым уровнем: \
             на ур. 2 — цена «корм от», на ур. 3 — вдвое больше, на ур. 4 — втрое и так далее."));
    ctx.send(poise::CreateReply::default().embed(embed).ephemeral(true)).await?;
    Ok(())
}

/// Посмотреть питомцев и сколько они накопили
#[poise::command(slash_command, guild_only, rename = "pets")]
pub async fn show_pets(
    ctx: Context<'_>,
    #[description = "Чьих питомцев посмотреть (если не выбрать — твоих)"] member: Option<serenity::Member>,
) -> Result<(), Error> {
    let (data, guild) = (ctx.data(), ctx.guild_id().unwrap().get());
    let (owner, owner_name, own) = match &member {
        Some(member) => (member.user.id.get(), member.display_name().to_string(), member.user.id == ctx.author().id),
        None => (ctx.author().id.get(), display_name(ctx).await, true),
    };
    let pets = sorted_pets(data.db.user_pets(guild, owner)?);
    if pets.is_empty() {
        let text = if own { "Питомцев пока нет. Открой кейс: /case" } else { "У него пока нет питомцев." };
        return whisper(ctx, format!("🐾 {text}")).await;
    }
    let (now, cap) = (db::now(), data.pets.collect_cap_hours);
    let (mut lines, mut total_income, mut total_pending) = (Vec::new(), 0, 0);
    for (index, (pet, species)) in pets.iter().enumerate() {
        let (hourly, saved) = (if pet.inactive_since.is_none() { income(species, pet.level) } else { 0 },
            owned_pending(pet, species, now, cap));
        total_income += hourly;
        total_pending += saved;
        let feed = if pet.level < data.pets.max_level {
            format!("корм {}", number(feed_cost(species, pet.level)))
        } else {
            "макс. уровень".to_string()
        };
        lines.push(format!(
            "`{}.` {} — {}/ч, накопил {}, {feed}", index + 1, title(species, pet.level), money_text(hourly), number(saved)));
    }
    let mut description = lines.join("\n");
    if description.encode_utf16().count() > 4096 {
        let mut length = 0;
        description = lines.into_iter().take_while(|line| {
            length += line.encode_utf16().count() + 1;
            length <= 4000
        }).collect::<Vec<_>>().join("\n");
        description.push_str("\nОстальные питомцы видны на складе в /games.");
    }
    let mut embed = serenity::CreateEmbed::new()
        .color(0x22C55E)
        .title(format!("🐾 Питомцы {owner_name} ({}/{})", pets.len(), data.pets.max_pets))
        .description(description)
        .field("Доход", format!("{} в час", money_text(total_income)), true)
        .field("Можно забрать", money_text(total_pending), true)
        .footer(serenity::CreateEmbedFooter::new(format!(
            "Доход копится до {} — потом питомцы ждут, пока заберёшь", hours_text(cap * 3600))));
    let mut reply = poise::CreateReply::default();
    if own && pets.iter().any(|(_, species)| species.rarity == "secret") {
        if let Some(art) = secret_pet_art(data).await {
            embed = embed.thumbnail(format!("attachment://{SECRET_PET_FILENAME}"));
            reply = reply.ephemeral(true).attachment(serenity::CreateAttachment::bytes(art, SECRET_PET_FILENAME));
        }
    }
    ctx.send(reply.embed(embed)).await?;
    Ok(())
}

/// Забрать монеты, которые заработали питомцы
#[poise::command(slash_command, guild_only)]
pub async fn pets_collect(ctx: Context<'_>) -> Result<(), Error> {
    let (data, guild, user) = (ctx.data(), ctx.guild_id().unwrap().get(), ctx.author().id.get());
    let name = display_name(ctx).await;
    let wallet = Wallet { guild, user, name: &name, start: data.economy.start_balance };
    let (total, balance) = match collect_pets(&data.db, wallet, data.pets.collect_cap_hours, db::now())? {
        Ok(result) => result,
        Err(reason) => return whisper(ctx, reason).await,
    };
    say(ctx, format!(
        "🐾 Питомцы <@{user}> принесли **+{}**. Баланс: **{}**.", money_text(total), money_text(balance))).await
}

/// Покормить питомца — он вырастет на уровень и станет приносить больше
#[poise::command(slash_command, guild_only)]
pub async fn pet_feed(
    ctx: Context<'_>,
    #[description = "Какого питомца покормить"]
    #[autocomplete = "pet_autocomplete"]
    pet: i64,
) -> Result<(), Error> {
    let (data, guild, user) = (ctx.data(), ctx.guild_id().unwrap().get(), ctx.author().id.get());
    let name = display_name(ctx).await;
    let wallet = Wallet { guild, user, name: &name, start: data.economy.start_balance };
    let Feeding { pet, species, cost, previous_level, .. } = match feed_owned_pet(&data.db, wallet, pet, &data.pets, None)? {
        Ok(result) => result,
        Err(reason) => return whisper(ctx, reason).await,
    };
    let max_level = data.pets.max_level;
    let new_level = pet.level;
    let new_income = income(species, new_level);
    let grown = if stage(new_level) != stage(previous_level) {
        format!(" и теперь он **{}**!", stage(new_level))
    } else {
        String::new()
    };
    let next = if new_level < max_level {
        format!(", следующая кормёжка — {}.", money_text(feed_cost(species, new_level)))
    } else {
        ".".to_string()
    };
    say(ctx, format!(
        "🍖 Питомец <@{user}> {} {} поел за {} — теперь **ур. {new_level}**{grown}. Доход: **{}/ч**{next}",
        species.emoji, species.name, money_text(cost), money_text(new_income))).await
}

fn sell_owned_pet(
    database: &Database,
    wallet: Wallet<'_>,
    pet: &Pet,
    species: &Species,
    cap_hours: i64,
    now: impl FnOnce() -> f64,
) -> rusqlite::Result<Option<(i64, i64)>> {
    database.transaction(|tx| {
        if Database::own_pet_on(tx, pet.id, wallet.guild, wallet.user)?.as_ref() != Some(pet) {
            return Ok(None);
        }
        let sale = commit_pet_sale(tx, wallet, [(pet, species)], cap_hours, now())?;
        Ok(Some((sale.earned, sale.balance)))
    })
}

pub(crate) struct PetSale {
    pub earned: i64,
    pub payout: i64,
    pub balance: i64,
}

fn commit_pet_sale<'a>(
    tx: &Transaction<'_>,
    wallet: Wallet<'_>,
    pets: impl IntoIterator<Item = (&'a Pet, &'a Species)>,
    cap_hours: i64,
    now: f64,
) -> rusqlite::Result<PetSale> {
    let mut earned = 0_i64;
    let mut payout = 0_i64;
    for (pet, species) in pets {
        let saved = owned_pending(pet, species, now, cap_hours);
        earned = earned.checked_add(saved).ok_or(rusqlite::Error::IntegralValueOutOfRange(0, earned))?;
        payout = payout.checked_add(value(species, pet.level)).and_then(|amount| amount.checked_add(saved))
            .ok_or(rusqlite::Error::IntegralValueOutOfRange(0, payout))?;
        tx.execute("DELETE FROM pets WHERE id = ?1", [pet.id])?;
    }
    wallet.open(tx)?;
    let balance = db::change_balance(tx, wallet.guild, wallet.user, payout)?;
    Ok(PetSale { earned, payout, balance })
}

pub(crate) fn sell_inventory_pets(
    database: &Database,
    wallet: Wallet<'_>,
    ids: &std::collections::BTreeSet<i64>,
    cap_hours: i64,
    now: impl FnOnce() -> f64,
) -> rusqlite::Result<Result<PetSale, &'static str>> {
    database.transaction(|tx| {
        if ids.is_empty() {
            return Ok(Err("Выбери питомцев для продажи."));
        }
        let mut animals = Vec::with_capacity(ids.len());
        for id in ids {
            let animal = Database::own_pet_on(tx, *id, wallet.guild, wallet.user)?
                .and_then(|pet| find_species(&pet.species).map(|species| (pet, species)));
            let Some(animal) = animal else {
                return Ok(Err("Один из питомцев уже продан или передан. Проверь склад."));
            };
            animals.push(animal);
        }
        commit_pet_sale(tx, wallet, animals.iter().map(|(pet, species)| (pet, *species)), cap_hours, now()).map(Ok)
    })
}

/// Продать питомца и забрать накопленный доход
#[poise::command(slash_command, guild_only)]
pub async fn pet_sell(
    ctx: Context<'_>,
    #[description = "Какого питомца продать"]
    #[autocomplete = "pet_sale_autocomplete"]
    pet: i64,
) -> Result<(), Error> {
    let (data, guild, user) = (ctx.data(), ctx.guild_id().unwrap().get(), ctx.author().id.get());
    let found = data.db.own_pet(pet, guild, user)?.and_then(|pet| find_species(&pet.species).map(|species| (pet, species)));
    let Some((pet, species)) = found else {
        return whisper(ctx, "Такого питомца у тебя нет — выбери из списка.").await;
    };
    if needs_sale_confirmation(species) {
        let offer = format!(
            "Продать {} за **{}**? Накопленный к моменту продажи доход будет начислен отдельно. \
             Вернуть питомца после продажи нельзя.",
            title(species, pet.level), money_text(value(species, pet.level)));
        if !crate::qol::confirm(ctx, offer).await? {
            return Ok(());
        }
    }
    let name = display_name(ctx).await;
    let wallet = Wallet { guild, user, name: &name, start: data.economy.start_balance };
    let sold = sell_owned_pet(&data.db, wallet, &pet, species, data.pets.collect_cap_hours, db::now)?;
    let Some((earned, balance)) = sold else {
        return whisper(ctx, "Питомец уже продан, передан или изменился. Повтори команду.").await;
    };
    say(ctx, format!(
        "💸 <@{user}> продаёт питомца {} за **{}**. Накопленный доход: **+{}**. Баланс: **{}**.",
        title(species, pet.level), money_text(value(species, pet.level)), money_text(earned), money_text(balance))).await
}

const BATTLE_MIN_CHANCE: i64 = 10; // шанс победить в бою не бывает меньше 10% и больше 90%

/// Шанс победить в процентах: доля своей силы (дохода в час) в общей, но не меньше 10% и не больше 90%.
pub fn battle_chance(power: i64, enemy_power: i64) -> i64 {
    let chance = (100.0 * power as f64 / (power as f64 + enemy_power as f64)).round_ties_even() as i64;
    chance.clamp(BATTLE_MIN_CHANCE, 100 - BATTLE_MIN_CHANCE)
}

/// Бой питомцев на ставку: твой питомец против сильнейшего у соперника
#[poise::command(slash_command, guild_only)]
pub async fn pet_battle(
    ctx: Context<'_>,
    #[description = "Кого вызвать на бой"] member: serenity::User,
    #[description = "Какой твой питомец дерётся"]
    #[autocomplete = "pet_autocomplete"]
    pet: i64,
    #[description = "Ставка в монетах"]
    #[min = 1]
    #[max = 1_000_000]
    bet: i64,
) -> Result<(), Error> {
    let (data, guild, user) = (ctx.data(), ctx.guild_id().unwrap().get(), ctx.author().id.get());
    let (start, name, enemy_name) = (data.economy.start_balance, display_name(ctx).await, user_name(&member));
    let enemy = member.id.get();
    if member.bot || enemy == user {
        return whisper(ctx, "Вызови на бой другого участника (не бота и не себя).").await;
    }
    let mine = data.db.own_pet(pet, guild, user)?.and_then(|pet| find_species(&pet.species).map(|species| (pet, species)));
    let Some((mine, my_species)) = mine else {
        return whisper(ctx, "Такого питомца у тебя нет — выбери из списка.").await;
    };
    if sorted_pets(data.db.user_pets(guild, enemy)?).is_empty() {
        return whisper(ctx, "У него пока нет питомцев — драться не с кем.").await;
    }
    if bet < data.economy.min_bet || data.db.wallet(guild, user, &name, start)?.0 < bet {
        return whisper(ctx, format!(
            "Ставка — от {}, и у тебя должно хватать монет.", money_text(data.economy.min_bet))).await;
    }
    let offer = format!(
        "⚔️ <@{enemy}>, <@{user}> вызывает твоего сильнейшего питомца на бой с {}! Ставка — **{}** с каждого.",
        title(my_species, mine.level), money_text(bet));
    let Some(press) = challenge(ctx, member.id, offer).await? else { return Ok(()) };

    // Пока ждали ответа, питомца могли продать, а монеты — потратить: проверяем заново
    let result = data.db.transaction(|tx| {
        let still_mine = Database::own_pet_on(tx, mine.id, guild, user)?
            .filter(|pet| pet.species == my_species.key);
        let enemy_pets = sorted_pets(Database::pets_on(tx, guild, enemy)?);
        let (Some(my_pet), Some((enemy_pet, enemy_species))) = (still_mine, enemy_pets.first()) else {
            return Ok("Бой отменён: у кого-то больше нет питомца.".into());
        };
        let first = Wallet { guild, user, name: &name, start };
        let second = Wallet { guild, user: enemy, name: &enemy_name, start };
        if first.open(tx)?.0 < bet || second.open(tx)?.0 < bet {
            return Ok(format!("Бой отменён: кому-то не хватает {} на ставку.", money_text(bet)));
        }
        let chance = battle_chance(income(my_species, my_pet.level), income(enemy_species, enemy_pet.level));
        let won = rand::rng().random_range(0.0..100.0) < chance as f64;
        let (winner, loser) = if won { (user, enemy) } else { (enemy, user) };
        db::change_balance(tx, guild, loser, -bet)?;
        let balance = db::change_balance(tx, guild, winner, bet)?;
        Ok(format!(
            "⚔️ {} (<@{user}>, шанс {chance}%) против {} (<@{enemy}>, шанс {}%)\n🏆 Побеждает <@{winner}> и забирает **{}**! Баланс: **{}**.",
            title(my_species, my_pet.level), title(enemy_species, enemy_pet.level), 100 - chance, money_text(bet),
            money_text(balance)))
    })?;
    finish_challenge(ctx, &press, result).await
}

/// Подсказки в поле «питомец соперника»: питомцы того, кого выбрали в поле member.
async fn their_pet_autocomplete(ctx: Context<'_>, partial: &str) -> serenity::CreateAutocompleteResponse {
    let response = serenity::CreateAutocompleteResponse::new();
    let (Some(guild), poise::Context::Application(app)) = (ctx.guild_id(), ctx) else { return response };
    let chosen = app.interaction.data.options.iter().find_map(|option| match (&*option.name, &option.value) {
        ("member", serenity::CommandDataOptionValue::User(id)) => Some(*id),
        _ => None,
    });
    let Some(member) = chosen else { return response };
    let pets = ctx.data().db.user_pets(guild.get(), member.get()).unwrap_or_default();
    let partial = partial.to_lowercase();
    let choices: Vec<serenity::AutocompleteChoice> = sorted_pets(pets)
        .into_iter()
        .enumerate()
        .map(|(index, (pet, species))| (format!("{}. {}", index + 1, title(species, pet.level)), pet.id))
        .filter(|(label, _)| label.to_lowercase().contains(&partial))
        .take(25)
        .map(|(label, id)| serenity::AutocompleteChoice::new(label, id))
        .collect();
    response.set_choices(choices)
}

/// Предложить обмен питомцами: твой питомец на питомца другого участника
#[poise::command(slash_command, guild_only)]
pub async fn pet_trade(
    ctx: Context<'_>,
    #[description = "С кем меняться"] member: serenity::User,
    #[description = "Какого своего питомца отдаёшь"]
    #[autocomplete = "pet_autocomplete"]
    pet: i64,
    #[description = "Какого питомца хочешь взамен"]
    #[autocomplete = "their_pet_autocomplete"]
    their_pet: i64,
) -> Result<(), Error> {
    let (data, guild, user, other) = (ctx.data(), ctx.guild_id().unwrap().get(), ctx.author().id.get(), member.id.get());
    if member.bot || other == user {
        return whisper(ctx, "Меняться можно с другим участником (не с ботом и не с собой).").await;
    }
    let with_species = |pet: Option<Pet>| pet.and_then(|pet| find_species(&pet.species).map(|species| (pet, species)));
    let (Some((mine, my_species)), Some((theirs, their_species))) =
        (with_species(data.db.own_pet(pet, guild, user)?), with_species(data.db.own_pet(their_pet, guild, other)?))
    else {
        return whisper(ctx, "Выбери своего питомца и питомца соперника из списков.").await;
    };
    let (my_title, their_title) = (title(my_species, mine.level), title(their_species, theirs.level));
    let offer = format!(
        "🤝 <@{other}>, <@{user}> предлагает обмен: его {my_title} на твоего {their_title}. \
         Накопленные монеты уходят вместе с питомцем.");
    let Some(press) = challenge(ctx, member.id, offer).await? else { return Ok(()) };
    let text = if data.db.swap_pets(guild, (mine.id, user), (theirs.id, other))? {
        format!("🤝 Обмен состоялся: <@{user}> получает {their_title}, <@{other}> получает {my_title}.")
    } else {
        "Обмен отменён: у кого-то больше нет этого питомца.".to_string()
    };
    finish_challenge(ctx, &press, text).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_history_includes_own_openings_and_keeps_servers_separate() {
        let db = Database::in_memory().unwrap();
        db.add_pet(7, 4, "valera", 0.0, 500).unwrap();
        let buy = |guild, user, name, face, case, species, price| {
            let wallet = Wallet { guild, user, name, start: 3000 };
            db.transaction(|tx| buy_pet(tx, wallet, face, case, kind(species), price, 10)).unwrap()
        };
        assert!(buy(7, 1, "Алиса", "alice.png", Case::Cheap, "valera", 1000).is_ok());
        assert!(buy(7, 2, "Борис", "boris.png", Case::Medium, "arkady", 3000).is_ok());
        assert!(buy(8, 3, "Вера", "vera.png", Case::Cheap, "frog", 1000).is_ok());
        assert!(buy(7, 4, "Глеб", "gleb.png", Case::Expensive, "zhorik", 9000).is_err());
        db.delete_pet(db.user_pets(7, 2).unwrap()[0].id).unwrap();

        let openings = case_history(&db, 7).unwrap();
        assert_eq!(openings.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(), ["Борис", "Алиса"]);
        assert_eq!(openings.iter().map(|entry| entry.kind).collect::<Vec<_>>(), [Some(Case::Medium), Some(Case::Cheap)]);
        assert_eq!((openings[0].user, openings[0].species.as_str()), (2, "arkady"));
        let id = openings[0].id;
        assert_eq!(case_opening_avatar(&db, 7, id).unwrap().as_deref(), Some("boris.png"));
        assert_eq!(case_opening_avatar(&db, 8, id).unwrap(), None);
    }

    #[test]
    fn failure_to_record_case_opening_rolls_back_purchase() {
        let db = Database::in_memory().unwrap();
        db.lock().execute_batch(
            "CREATE TEMP TRIGGER reject_case_opening BEFORE INSERT ON case_openings
             BEGIN SELECT RAISE(ABORT, 'history unavailable'); END;",
        ).unwrap();
        let wallet = Wallet { guild: 7, user: 1, name: "Алиса", start: 500 };
        wallet.open(&db.lock()).unwrap();
        assert!(db.transaction(|tx| buy_pet(tx, wallet, "alice.png", Case::Cheap, kind("valera"), 500, 10)).is_err());
        assert_eq!(wallet.open(&db.lock()).unwrap().0, 500);
        assert!(db.user_pets(7, 1).unwrap().is_empty());
        assert!(case_history(&db, 7).unwrap().is_empty());
    }

    #[test]
    fn failure_to_record_an_upgrade_leaves_the_stake_intact() {
        let db = Database::in_memory().unwrap();
        let wallet = Wallet { guild: 7, user: 1, name: "Алиса", start: 500 };
        let stake = db.add_pet(7, 1, "valera", db::now(), 0).unwrap();
        let before = db.user_pets(7, 1).unwrap();
        db.lock().execute_batch(
            "CREATE TEMP TRIGGER reject_upgrade BEFORE INSERT ON case_openings
             WHEN NEW.upgrade IS NOT NULL BEGIN SELECT RAISE(ABORT, 'history unavailable'); END;",
        ).unwrap();
        let result = upgrade_pet(&db, wallet, "alice.png", &[(stake, 1)].into(), kind("vasily"), 24, || 0);
        assert!(result.is_err());
        assert_eq!(db.user_pets(7, 1).unwrap(), before);
        assert!(case_history(&db, 7).unwrap().is_empty());
    }

    #[test]
    fn stellochka_reveal_belongs_to_the_opener_even_after_selling_the_pet() {
        let db = Database::in_memory().unwrap();
        let wallet = Wallet { guild: 7, user: 1, name: "Алиса", start: 25_000 };
        db.add_pet(7, 2, "stellochka", 0.0, 0).unwrap();
        assert!(!has_opened_stellochka(&db, 7, 2).unwrap());
        assert!(!has_opened_stellochka(&db, 7, 1).unwrap());

        db.transaction(|tx| buy_pet(tx, wallet, "", Case::Secret, kind("stellochka"), 25_000, 10)).unwrap().unwrap();
        assert!(has_opened_stellochka(&db, 7, 1).unwrap());
        assert!(!has_opened_stellochka(&db, 7, 2).unwrap());
        assert!(!has_opened_stellochka(&db, 8, 1).unwrap());
        db.delete_pet(db.user_pets(7, 1).unwrap()[0].id).unwrap();
        assert!(has_opened_stellochka(&db, 7, 1).unwrap());
    }

    #[test]
    fn case_purchase_replays_the_original_pet_after_price_changes_and_respects_the_purchase_limit() {
        let db = Database::in_memory().unwrap();
        let wallet = Wallet { guild: 7, user: 1, name: "Алиса", start: 2000 };
        let mut settings = PetSettings::load(&crate::config::Settings::default());
        settings.max_pets = 1;
        let request = CaseRequest { id: "first", price: 1000 };
        let (pet, balance) = buy_case(&db, wallet, "alice.png", &settings, Case::Cheap, Some(request))
            .unwrap().unwrap();
        assert_eq!(balance, 1000);
        assert!(Case::Cheap.species().any(|allowed| allowed.key == pet.key));

        settings.case_prices[Case::Cheap as usize] = 1100;
        let (replayed, balance) = buy_case(&db, wallet, "alice.png", &settings, Case::Cheap, Some(request))
            .unwrap().unwrap();
        assert_eq!((replayed.key, balance), (pet.key, 1000));
        assert!(buy_case(&db, wallet, "alice.png", &settings, Case::Medium, Some(request)).unwrap().is_err());
        let second = Some(CaseRequest { id: "second", price: 1100 });
        assert!(buy_case(&db, wallet, "alice.png", &settings, Case::Cheap, second).unwrap().is_err());

        let other = Wallet { user: 2, name: "Борис", ..wallet };
        assert!(buy_case(&db, other, "boris.png", &settings, Case::Cheap, Some(request)).unwrap().is_err());
        assert_eq!(other.open(&db.lock()).unwrap().0, 2000);
        assert!(db.user_pets(7, 2).unwrap().is_empty());
        assert_eq!(db.user_pets(7, 1).unwrap().len(), 1);
        let openings = case_history(&db, 7).unwrap();
        assert_eq!(openings.len(), 1);
        assert_eq!(openings[0].kind, Some(Case::Cheap));
        assert_eq!(wallet.open(&db.lock()).unwrap().0, 1000);
    }

    #[test]
    fn secret_case_price_choices_and_catalogue() {
        let defaults = PetSettings::load(&crate::config::Settings::default());
        assert_eq!(defaults.case_prices, [1000, 3000, 9000, 30000]);
        let custom = PetSettings::load(&crate::config::Settings::parse("[pets]\nsecret_case_price = 35000\n"));
        assert_eq!(custom.case_prices[Case::Secret as usize], 35000);
        let command = open_case();
        let choices = &command.parameters[0].choices;
        assert_eq!(choices.iter().map(|choice| choice.name.as_ref()).collect::<Vec<_>>(),
                   ["📦 Дешёвый", "🎁 Средний", "💎 Дорогой", "🔮 Секретный"]);
        let catalogue = case_catalogue(defaults.case_prices);
        for text in ["Секретный", "Стеллочка", "Фоидка романа", "Даня собака", "0.1%", "30 000"] {
            assert!(catalogue.contains(text), "missing {text}: {catalogue}");
        }
        assert!(catalogue.encode_utf16().count() <= 4096);
    }

    #[test]
    fn secret_case_boundaries_and_exclusivity() {
        assert_eq!(roll_pet(Case::Secret, 0.0, 0).rarity, "epic");
        assert_eq!(roll_pet(Case::Secret, 59.999, 0).rarity, "epic");
        assert_eq!(roll_pet(Case::Secret, 60.0, 0).rarity, "legendary");
        assert_eq!(roll_pet(Case::Secret, 94.949, 0).rarity, "legendary");
        assert_eq!(roll_pet(Case::Secret, 94.95, 0).rarity, "mythic");
        assert_eq!(roll_pet(Case::Secret, 99.899, 0).rarity, "mythic");
        for pick in 0..25 { assert_eq!(roll_pet(Case::Secret, 99.9, pick).key, "stellochka"); }
        assert_eq!(roll_pet(Case::Secret, 99.99999, 3).key, "stellochka");
        let secret = RARITIES.iter().find(|rarity| rarity.key == "secret").unwrap();
        assert_eq!(secret.chances, [0.0, 0.0, 0.0, 0.1]);
        assert_eq!(SPECIES.iter().filter(|species| species.rarity == "secret").count(), 1);
        for case in [Case::Cheap, Case::Medium, Case::Expensive] {
            for step in 0..10000 { assert_ne!(roll_pet(case, step as f64 / 100.0, step).rarity, "secret"); }
        }
    }

    #[test]
    fn new_pet_economy_and_existing_keys_remain_compatible() {
        for (key, rarity, hourly) in [
            ("stellochka", "secret", 2000),
            ("foidka_romana", "mythic", 550),
            ("danya_dog", "legendary", 170),
            ("hermes", "epic", 62),
        ] {
            let species = kind(key);
            assert_eq!((species.rarity, income(species, 1)), (rarity, hourly));
            assert_eq!(pending(species, 1, 0.0, 3600.0, 24), hourly);
            assert_eq!(feed_cost(species, 1), hourly * 20);
            assert!(needs_sale_confirmation(species));
        }
        for key in ["valera", "gena", "tolik", "frog", "arkady", "tamara", "boris", "igor", "edik", "zhorik", "vasily", "lyudmila", "lion", "sergey"] {
            assert!(find_species(key).is_some());
        }
        assert!(!needs_sale_confirmation(kind("valera")));
        let unique = SPECIES.iter().map(|species| species.key).collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), SPECIES.len());
    }

    #[test]
    fn battle_chances() {
        assert_eq!([battle_chance(500, 10), battle_chance(10, 500), battle_chance(60, 60), battle_chance(500, 150)],
                   [90, 10, 50, 77]);
    }

    fn kind(key: &str) -> &'static Species {
        find_species(key).unwrap()
    }

    #[test]
    fn chances_sum_to_100() {
        for case in 0..Case::ALL.len() {
            let total: f64 = RARITIES.iter().map(|rarity| rarity.chances[case]).sum();
            assert!((total - 100.0).abs() < 1e-9, "кейс {case}: {total}");
        }
        for species in SPECIES.iter() {
            assert!(RARITIES.iter().any(|rarity| rarity.key == species.rarity), "{}", species.key);
        }
    }

    #[test]
    fn case_contents_are_ordered_from_the_rarest_available_pets() {
        for case in Case::ALL {
            let eligible: Vec<_> = case.species().collect();
            assert_eq!(eligible.len(), [18, 18, 14, 12][case as usize]);
            assert!(eligible.windows(2).all(|pair| {
                let rank = |species: &Species| RARITIES.iter().position(|rarity| rarity.key == species.rarity);
                rank(pair[0]) >= rank(pair[1])
            }));
        }
        assert_eq!(Case::Secret.species().next().map(|species| species.key), Some("stellochka"));
    }

    #[test]
    fn income_and_costs_match_python() {
        // Те же числа, что даёт bot.py: round(base * (1 + 0.25 * (level - 1)))
        assert_eq!(income(kind("valera"), 1), 10);
        assert_eq!(income(kind("igor"), 3), 90);
        assert_eq!(income(kind("tolik"), 2), 10);
        assert_eq!(income(kind("frog"), 2), 11); // 11.25 → 11
        assert_eq!(income(kind("valera"), 2), 12); // 12.5 → 12 (половинки к чётному, как в Python)
        assert_eq!(income(kind("gena"), 4), 21);
        assert_eq!(income(kind("sergey"), 20), 2875);
        assert_eq!(feed_cost(kind("valera"), 1), 200);
        assert_eq!(feed_cost(kind("sergey"), 3), 30000);
    }

    #[test]
    fn stages_and_titles() {
        assert_eq!([stage(1), stage(4), stage(5), stage(10), stage(14), stage(15), stage(20)],
                   ["малыш", "малыш", "подросток", "взрослый", "взрослый", "легенда района", "легенда района"]);
        assert_eq!(title(kind("sergey"), 1), "🐶 собака Сергей (ур. 1, малыш)");
    }

    #[test]
    fn pending_is_capped() {
        let cat = kind("valera");
        assert_eq!(pending(cat, 1, 0.0, 3600.0 * 2.5, 24), 25);
        assert_eq!(pending(cat, 1, 0.0, 3600.0 * 100.0, 24), 240); // не больше суток
        assert_eq!(pending(cat, 1, 1000.0, 500.0, 24), 0); // время пошло назад — ноль, а не минус
    }

    #[test]
    fn opening_cases_respects_both_inventory_limits() {
        let db = Database::in_memory().unwrap();
        let wallet = Wallet { guild: 7, user: 1, name: "Alice", start: 100_000 };
        let settings = PetSettings::load(&crate::config::Settings::parse("[pets]\nmax_pets = 10"));
        assert_eq!(settings.max_pets, 40);
        let open = || buy_case(&db, wallet, "", &settings, Case::Cheap, None).unwrap();
        for _ in 0..40 {
            assert!(open().is_ok());
        }
        assert!(open().is_err());
        let owned = db.user_pets(7, 1).unwrap();
        assert_eq!(owned.len(), 40);
        assert_eq!(owned.iter().filter(|pet| pet.inactive_since.is_none()).count(), 10);
        assert_eq!(wallet.open(&db.lock()).unwrap().0, 100_000 - 40 * settings.case_prices[0]);
        assert!(move_pet(&db, wallet, owned[10].id, true, None, db::now()).unwrap().is_err());
        assert!(move_pet(&db, wallet, owned[10].id, true, Some(owned[0].id), db::now()).unwrap().is_ok());
        assert!(db.own_pet(owned[0].id, 7, 1).unwrap().unwrap().inactive_since.is_some());
        assert!(db.own_pet(owned[10].id, 7, 1).unwrap().unwrap().inactive_since.is_none());
    }

    #[test]
    fn standby_freezes_income_and_reactivation_resumes_it() {
        let db = Database::in_memory().unwrap();
        let wallet = Wallet { guild: 7, user: 1, name: "Alice", start: 100 };
        let id = db.add_pet(7, 1, "valera", 0.0, 500).unwrap();
        assert!(move_pet(&db, wallet, id, false, None, 3600.0).unwrap().is_ok());
        assert_eq!(collect_pets(&db, wallet, 24, 7200.0).unwrap(), Ok((10, 110)));
        assert!(collect_pets(&db, wallet, 24, 10_800.0).unwrap().is_err());
        assert!(move_pet(&db, wallet, id, true, None, 10_800.0).unwrap().is_ok());
        assert_eq!(collect_pets(&db, wallet, 24, 14_400.0).unwrap(), Ok((10, 120)));
    }

    #[test]
    fn trading_a_standby_pet_preserves_both_active_counts_and_unpaid_income() {
        let db = Database::in_memory().unwrap();
        let wallet = Wallet { guild: 7, user: 1, name: "Alice", start: 500 };
        let now = db::now();
        let cat = db.add_pet(7, 1, "valera", now - 720.0, 500).unwrap();
        let fox = db.add_pet(7, 2, "arkady", now - 440.0, 2000).unwrap();
        assert!(move_pet(&db, wallet, cat, false, None, now - 280.0).unwrap().is_ok());
        for _ in 0..ACTIVE_PET_LIMIT {
            db.add_pet(7, 1, "valera", now, 500).unwrap();
        }
        assert!(db.swap_pets(7, (cat, 1), (fox, 2)).unwrap());
        let new_fox = db.own_pet(fox, 7, 1).unwrap().unwrap();
        let new_cat = db.own_pet(cat, 7, 2).unwrap().unwrap();
        assert!(new_cat.inactive_since.is_none());
        assert!(new_fox.inactive_since.is_some());
        assert_eq!(owned_pending(&new_cat, kind("valera"), db::now(), 24), 1);
        assert_eq!(owned_pending(&new_fox, kind("arkady"), db::now(), 24), 3);
        for (guild, user, expected) in [(7, 1, 10), (7, 2, 1)] {
            assert_eq!(Database::active_pet_count_on(&db.lock(), guild, user).unwrap(), expected);
        }
    }

    #[test]
    fn feeding_a_standby_pet_keeps_its_frozen_coins_at_epoch_timestamps() {
        let db = Database::in_memory().unwrap();
        let wallet = Wallet { guild: 7, user: 1, name: "Alice", start: 10_000 };
        let paused = 1_730_000_000.25;
        let id = db.add_pet(7, 1, "arkady", paused - 288.0, 500).unwrap();
        assert!(move_pet(&db, wallet, id, false, None, paused).unwrap().is_ok());
        let settings = PetSettings::load(&crate::config::Settings::default());
        let fed = feed_owned_pet(&db, wallet, id, &settings, Some(1)).unwrap().ok().unwrap();
        assert_eq!(fed.pet.inactive_since, Some(paused));
        assert_eq!(collect_pets(&db, wallet, 24, paused + 3600.0).unwrap(), Ok((2, fed.balance + 2)));
    }

    #[test]
    fn pet_operations_reject_ids_outside_the_players_inventory_without_side_effects() {
        let db = Database::in_memory().unwrap();
        let wallet = Wallet { guild: 7, user: 1, name: "Alice", start: 500 };
        let own = db.add_pet(7, 1, "valera", 0.0, 500).unwrap();
        let foreign = db.add_pet(7, 2, "arkady", 0.0, 2000).unwrap();
        let other_guild = db.add_pet(8, 1, "valera", 0.0, 500).unwrap();
        assert!(move_pet(&db, wallet, own, false, None, 3600.0).unwrap().is_ok());
        let accounts = [(7, 1), (7, 2), (8, 1)];
        let snapshots = accounts.map(|(guild, user)| db.user_pets(guild, user).unwrap());
        let settings = PetSettings::load(&crate::config::Settings::default());
        for id in [foreign, other_guild] {
            assert!(move_pet(&db, wallet, id, false, None, 7200.0).unwrap().is_err());
            assert!(move_pet(&db, wallet, own, true, Some(id), 7200.0).unwrap().is_err());
            assert!(feed_owned_pet(&db, wallet, id, &settings, Some(1)).unwrap().is_err());
            assert!(sell_inventory_pets(&db, wallet, &[own, id].into(), 24, || 7200.0).unwrap().is_err());
            assert!(upgrade_pet(&db, wallet, "", &[(own, 1), (id, 1)].into(), kind("vasily"), 24,
                || panic!("unowned stake was rolled")).unwrap().is_err());
        }
        assert_eq!(accounts.map(|(guild, user)| db.user_pets(guild, user).unwrap()), snapshots);
        assert_eq!(wallet.open(&db.lock()).unwrap().0, 500);
    }

    #[test]
    fn upgrade_uses_value_at_the_current_level_and_consumes_one_owned_stake() {
        let db = Database::in_memory().unwrap();
        let wallet = Wallet { guild: 7, user: 1, name: "Alice", start: 100 };
        let stake = kind("vasily");
        let target = kind("sergey");
        let first = db.add_pet(7, 1, stake.key, db::now(), 500).unwrap();
        db.feed_pet(first, feed_cost(stake, 1), db::now()).unwrap();
        let other = db.add_pet(7, 2, stake.key, db::now(), 500).unwrap();
        let upgrade = |id, level, target, roll| upgrade_pet(&db, wallet, "alice.png", &[(id, level)].into(), target, 24, || roll).unwrap();
        assert!(upgrade(other, 1, target, 0).is_err());
        assert!(upgrade(first, 1, target, 0).is_err());
        assert!(upgrade(first, 2, kind("stellochka"), 0).is_err());
        let chance = 3384;
        let won = upgrade(first, 2, target, chance - 1).unwrap();
        assert!(won.won);
        assert_eq!(won.chance, chance);
        assert!(upgrade(first, 2, target, 0).is_err());
        let only = db.user_pets(7, 1).unwrap();
        assert_eq!((only.len(), only[0].species.as_str(), only[0].level), (1, target.key, 1));
        let second = db.add_pet(7, 1, stake.key, db::now(), 500).unwrap();
        let lost = upgrade(second, 1, target, 2700).unwrap();
        assert_eq!((lost.won, lost.chance), (false, 2700));
        assert!(db.own_pet(second, 7, 1).unwrap().is_none());
        assert!(db.own_pet(other, 7, 2).unwrap().is_some());

        let history = case_history(&db, 7).unwrap();
        assert_eq!(history.len(), 2);
        for (opening, (level, won)) in history.iter().zip([(1, false), (2, true)]) {
            assert_eq!((opening.user, opening.species.as_str(), opening.kind), (1, "sergey", None));
            let activity = opening.upgrade.as_ref().unwrap();
            assert_eq!(activity.stakes, [("vasily".to_string(), level)]);
            assert_eq!(activity.won, won);
        }
        assert_eq!(case_opening_avatar(&db, 7, history[0].id).unwrap().as_deref(), Some("alice.png"));
        assert!(case_history(&db, 8).unwrap().is_empty());
    }

    #[test]
    fn multi_pet_upgrade_uses_the_sum_and_validates_every_pet_before_spending() {
        let db = Database::in_memory().unwrap();
        let wallet = Wallet { guild: 7, user: 1, name: "Alice", start: 100 };
        let now = db::now();
        let first = db.add_pet(7, 1, "valera", now, 500).unwrap();
        let second = db.add_pet(7, 1, "valera", now, 500).unwrap();
        let other = db.add_pet(7, 2, "valera", now, 500).unwrap();
        db.feed_pet(first, 200, now).unwrap();
        assert!(move_pet(&db, wallet, second, false, None, now).unwrap().is_ok());
        let target = kind("vasily");
        let invalid = [(first, 2), (other, 1)].into();
        assert!(upgrade_pet(&db, wallet, "", &invalid, target, 24, || panic!("invalid stake was rolled"))
            .unwrap().is_err());
        assert_eq!(db.user_pets(7, 1).unwrap().len(), 2);

        let stakes = [(first, 2), (second, 1)].into();
        let chance = 1320;
        let result = upgrade_pet(&db, wallet, "", &stakes, target, 24, || chance - 1).unwrap().unwrap();
        assert_eq!((result.won, result.chance), (true, chance));
        assert!(upgrade_pet(&db, wallet, "", &stakes, target, 24, || 0).unwrap().is_err());
        let pets = db.user_pets(7, 1).unwrap();
        assert_eq!(pets.len(), 1);
        assert_eq!(pets[0].species, target.key);
        assert!(pets[0].inactive_since.is_none());
        assert!(db.own_pet(other, 7, 2).unwrap().is_some());
        let history = case_history(&db, 7).unwrap();
        assert_eq!(history.len(), 1);
        let opening = history[0].upgrade.as_ref().unwrap();
        assert_eq!(opening.stakes, [("valera".to_string(), 2), ("valera".to_string(), 1)]);
        assert_eq!(opening.most_expensive_stake().map(|(pet, level)| (pet.key, level)), Some(("valera", 2)));
    }

    #[test]
    fn collecting_inventory_income_pays_the_owner_once_and_clears_income_for_sale() {
        let db = Database::in_memory().unwrap();
        let wallet = Wallet { guild: 7, user: 1, name: "Alice", start: 100 };
        let id = db.add_pet(7, 1, "valera", 0.0, 500).unwrap();
        db.add_pet(7, 1, "valera", 1800.0, 500).unwrap();
        let other_player = db.add_pet(7, 2, "valera", 0.0, 500).unwrap();
        let other_server = db.add_pet(8, 1, "valera", 0.0, 500).unwrap();

        let collect = || collect_pets(&db, wallet, 24, 3600.0).unwrap();
        assert_eq!(collect(), Ok((15, 115)));
        assert!(collect().is_err());
        assert_eq!(db.own_pet(other_player, 7, 2).unwrap().unwrap().last_collect, 0.0);
        assert_eq!(db.own_pet(other_server, 8, 1).unwrap().unwrap().last_collect, 0.0);

        let sale = sell_inventory_pets(&db, wallet, &[id].into(), 24, || 3600.0).unwrap().unwrap();
        assert_eq!(sale.earned, 0);
        assert_eq!(sale.balance, 415);
    }

    #[test]
    fn sale_pays_the_level_adjusted_price_and_unpaid_income_once() {
        let db = Database::in_memory().unwrap();
        let wallet = Wallet { guild: 7, user: 1, name: "Alice", start: 100 };
        let species = kind("valera");
        let first = db.add_pet(7, 1, species.key, 0.0, 500).unwrap();
        let second = db.add_pet(7, 1, species.key, 0.0, 7500).unwrap();
        let first = db.own_pet(first, 7, 1).unwrap().unwrap();
        let stale = db.own_pet(second, 7, 1).unwrap().unwrap();
        db.feed_pet(second, 200, 0.0).unwrap();
        let second = db.own_pet(second, 7, 1).unwrap().unwrap();
        let sell = |pet| sell_owned_pet(&db, wallet, pet, species, 24, || 7200.0).unwrap();

        assert_eq!(sell(&stale), None);
        assert_eq!(sell(&first), Some((20, 420)));
        assert_eq!(sell(&second), Some((24, 804)));
        assert_eq!(sell(&second), None);
        assert!(db.user_pets(7, 1).unwrap().is_empty());
        assert_eq!(db.wallet(7, 1, "Alice", 100).unwrap().0, 804);
    }

    #[test]
    fn sale_rolls_back_if_wallet_credit_overflows() {
        let db = Database::in_memory().unwrap();
        let wallet = Wallet { guild: 7, user: 1, name: "Alice", start: i64::MAX };
        let species = kind("valera");
        let id = db.add_pet(7, 1, species.key, 0.0, 500).unwrap();
        let pet = db.own_pet(id, 7, 1).unwrap().unwrap();
        db.wallet(7, 1, "Alice", i64::MAX).unwrap();

        assert!(matches!(sell_owned_pet(&db, wallet, &pet, species, 24, || 0.0),
            Err(rusqlite::Error::IntegralValueOutOfRange(..))));
        assert_eq!(db.own_pet(id, 7, 1).unwrap(), Some(pet));
        assert_eq!(db.wallet(7, 1, "Alice", 0).unwrap().0, i64::MAX);
    }

    #[test]
    fn inventory_sale_checks_the_whole_selection_and_pays_once() {
        let db = Database::in_memory().unwrap();
        let wallet = Wallet { guild: 7, user: 1, name: "Alice", start: 100 };
        let first = db.add_pet(7, 1, "valera", 0.0, 500).unwrap();
        let second = db.add_pet(7, 1, "valera", 0.0, 500).unwrap();
        let foreign = db.add_pet(7, 2, "valera", 0.0, 500).unwrap();
        let invalid = [first, foreign].into();
        let selected = [first, second].into();
        let sell = |ids| sell_inventory_pets(&db, wallet, ids, 24, || 3600.0).unwrap();

        assert!(sell(&invalid).is_err());
        assert_eq!(db.user_pets(7, 1).unwrap().len(), 2);
        let paid = sell(&selected).unwrap();
        assert_eq!((paid.payout, paid.earned, paid.balance), (620, 20, 720));
        assert!(sell(&selected).is_err());
        assert_eq!(db.user_pets(7, 2).unwrap()[0].id, foreign);
        assert_eq!(db.wallet(7, 1, "Alice", 0).unwrap().0, 720);
    }

    #[test]
    fn inventory_upgrade_with_a_stale_level_cannot_charge_twice() {
        let db = Database::in_memory().unwrap();
        let wallet = Wallet { guild: 7, user: 1, name: "Alice", start: 1000 };
        let settings = PetSettings::load(&crate::config::Settings::default());
        let id = db.add_pet(7, 1, "valera", 0.0, 500).unwrap();
        let upgrade = || feed_owned_pet(&db, wallet, id, &settings, Some(1)).unwrap();

        let first = upgrade().unwrap();
        assert_eq!((first.pet.level, first.cost, first.balance), (2, 200, 800));
        assert!(upgrade().is_err());
        assert_eq!(db.wallet(7, 1, "Alice", 0).unwrap().0, 800);
    }

    #[test]
    fn oleg_has_fixed_feed_cost_and_gains_one_coin_per_hour_at_each_level() {
        let db = Database::in_memory().unwrap();
        let wallet = Wallet { guild: 7, user: 1, name: "Alice", start: 2_000_000 };
        let settings = PetSettings::load(&crate::config::Settings::default());
        let species = kind("fat_oleg");
        assert_eq!((species.rarity, income(species, 1), value(species, 1)), ("mythic", 533, 15_990));
        let id = db.add_pet(7, 1, species.key, db::now(), 500).unwrap();
        let upgrade = |level| feed_owned_pet(&db, wallet, id, &settings, Some(level)).unwrap();

        let first = upgrade(1).unwrap();
        assert_eq!((first.pet.level, first.cost, first.balance), (2, 1_000_000, 1_000_000));
        assert_eq!(income(species, first.pet.level), 534);
        let second = upgrade(2).unwrap();
        assert_eq!((second.pet.level, second.cost, second.balance), (3, 1_000_000, 0));
        assert_eq!(income(species, second.pet.level), 535);
        assert!(upgrade(3).is_err());
        assert_eq!(db.own_pet(id, 7, 1).unwrap().unwrap().level, 3);
    }

    #[test]
    fn sale_prices_follow_income_and_keep_resale_below_case_cost_on_average() {
        for species in &SPECIES {
            assert!(value(species, 1) > 0);
            for level in 1..=20 {
                assert_eq!(value(species, level), income(species, level) * 30);
            }
        }
        assert_eq!(value(kind("stellochka"), 1), 60_000);

        let mut expected = [0.0; Case::ALL.len()];
        for rarity in &RARITIES {
            let kinds: Vec<_> = SPECIES.iter().filter(|species| species.rarity == rarity.key).collect();
            let average = kinds.iter().map(|species| value(species, 1)).sum::<i64>() as f64 / kinds.len() as f64;
            for (payout, chance) in expected.iter_mut().zip(rarity.chances) {
                *payout += average * chance / 100.0;
            }
        }
        let settings = PetSettings::load(&crate::config::Settings::default());
        for (payout, cost) in expected.into_iter().zip(settings.case_prices) {
            assert!(payout < cost as f64, "{payout:.2} >= {cost}");
        }
    }

    #[test]
    fn rolls() {
        assert_eq!(roll_pet(Case::Cheap, 0.0, 0).rarity, "common");
        assert_eq!(roll_pet(Case::Cheap, 74.9, 0).rarity, "common");
        assert_eq!(roll_pet(Case::Cheap, 75.0, 0).rarity, "rare");
        assert_eq!(roll_pet(Case::Cheap, 95.1, 3).key, "hermes");
        assert_eq!(roll_pet(Case::Cheap, 99.995, 0).key, "sergey");
        assert_eq!(roll_pet(Case::Cheap, 99.995, 2).key, "fat_oleg");
        assert_eq!(roll_pet(Case::Expensive, 0.0, 0).rarity, "rare"); // обычных в дорогом кейсе нет
        assert_eq!(roll_pet(Case::Expensive, 99.0, 5).rarity, "mythic");
        assert_eq!(roll_pet(Case::Medium, 50.0, 1).key, "tamara");
    }

    #[test]
    fn strongest_first() {
        let pet = |id, species: &str, level| Pet { id, species: species.to_string(), level, last_collect: 0.0, spent: 0, inactive_since: None };
        let pets = sorted_pets(vec![pet(1, "valera", 1), pet(2, "sergey", 1), pet(3, "igor", 3), pet(4, "нет такого", 1), pet(5, "tolik", 2)]);
        let order: Vec<i64> = pets.iter().map(|(pet, _)| pet.id).collect();
        assert_eq!(order, [2, 3, 1, 5]); // у котика и голубя доход 10 — раньше тот, кто раньше появился
    }
}
