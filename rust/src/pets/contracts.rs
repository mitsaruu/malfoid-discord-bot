use super::{
    consume_stakes, db, insert_owned_pet, owned_stakes, stake_value, value, Database,
    PetSettings, PetStakes, Species, Wallet, MAX_PET_LIMIT, SPECIES, UPGRADE_SCALE,
};
use rusqlite::Connection;
use std::collections::BTreeMap;

const MIN_CONTRACT_PETS: usize = 3;
const CONTRACT_RETURN_PERCENT: i128 = 90;
const CHEAPER_PRIZE_PERCENT: i128 = 50;
const DEARER_PRIZE_PERCENT: i128 = 175;
const PRIZES_PER_OUTCOME: usize = 4;

#[derive(Clone, Copy)]
pub(crate) struct ContractPrize {
    pub species: &'static Species,
    pub level: i64,
}

impl ContractPrize {
    pub(crate) fn price(self) -> i64 {
        value(self.species, self.level)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ContractQuote {
    pub stake: i64,
    pub range: (i64, i64),
}

struct ContractOdds {
    quote: ContractQuote,
    chance: u32,
    cheaper: Vec<ContractPrize>,
    dearer: Vec<ContractPrize>,
}

pub(crate) struct ContractResult {
    pub prize: ContractPrize,
    pub stake: i64,
    pub dearer: bool,
    pub balance: i64,
}

fn prize_levels(max_level: i64) -> impl Iterator<Item = (&'static Species, i64)> {
    SPECIES.iter().filter(|species| species.rarity != "secret").map(move |species| {
        // Honor Oleg's fixed feeding cost by awarding him at level one.
        (species, if species.key == "fat_oleg" { 1 } else { max_level })
    })
}

fn max_stake(max_level: i64) -> i64 {
    let maximum_prize = prize_levels(max_level).map(|(species, level)| value(species, level)).max().unwrap();
    (i128::from(maximum_prize) * 100 / DEARER_PRIZE_PERCENT) as i64
}

fn prize_at_or_above(species: &'static Species, max_level: i64, price: i128) -> Option<ContractPrize> {
    if i128::from(value(species, max_level)) < price {
        return None;
    }
    let (mut low, mut high) = (1, max_level);
    while low < high {
        let middle = low + (high - low) / 2;
        if i128::from(value(species, middle)) < price {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    Some(ContractPrize { species, level: low })
}

fn contract_odds(total: i128, max_level: i64) -> Option<ContractOdds> {
    let stake = i64::try_from(total).ok()?;
    let cheaper_target = total * CHEAPER_PRIZE_PERCENT / 100;
    let dearer_target = (total * DEARER_PRIZE_PERCENT + 99) / 100;
    let mut cheaper = Vec::new();
    let mut dearer = Vec::new();
    for (species, max_level) in prize_levels(max_level) {
        let cheaper_level = prize_at_or_above(species, max_level, cheaper_target + 1)
            .map_or(max_level, |prize| prize.level - 1);
        if cheaper_level > 0 {
            cheaper.push(ContractPrize { species, level: cheaper_level });
        }
        if let Some(prize) = prize_at_or_above(species, max_level, dearer_target) {
            dearer.push(prize);
        }
    }
    cheaper.sort_by_key(|prize| std::cmp::Reverse(prize.price()));
    dearer.sort_by_key(|prize| prize.price());
    cheaper.truncate(PRIZES_PER_OUTCOME);
    dearer.truncate(PRIZES_PER_OUTCOME);
    let range = (cheaper.last()?.price(), dearer.last()?.price());
    let lo_count = cheaper.len() as i128;
    let hi_count = dearer.len() as i128;
    let lo_sum: i128 = cheaper.iter().map(|prize| i128::from(prize.price())).sum();
    let hi_sum: i128 = dearer.iter().map(|prize| i128::from(prize.price())).sum();
    // Solve for the probability that makes the weighted average 90% of the stake.
    let numerator = (total * CONTRACT_RETURN_PERCENT * lo_count - lo_sum * 100) * hi_count;
    let denominator = (hi_sum * lo_count - lo_sum * hi_count) * 100;
    let chance = (numerator * i128::from(UPGRADE_SCALE) / denominator) as u32;
    let quote = ContractQuote { stake, range };
    Some(ContractOdds { quote, chance, cheaper, dearer })
}

fn contract_stakes(
    connection: &Connection, wallet: Wallet<'_>, stakes: &BTreeMap<i64, i64>, max_level: i64,
) -> rusqlite::Result<Result<(PetStakes, ContractOdds), String>> {
    if !(MIN_CONTRACT_PETS..=MAX_PET_LIMIT).contains(&stakes.len()) {
        return Ok(Err("Для контракта выбери минимум трёх питомцев.".into()));
    }
    let selected = match owned_stakes(connection, wallet, stakes)? {
        Ok(selected) => selected,
        Err(message) => return Ok(Err(message.into())),
    };
    let Some(odds) = contract_odds(stake_value(&selected), max_level) else {
        return Ok(Err(format!("Максимальная ставка — {} монет. Уменьши сумму контракта.",
            crate::text::number(max_stake(max_level)))));
    };
    Ok(Ok((selected, odds)))
}

pub(crate) fn contract_quote(
    database: &Database, wallet: Wallet<'_>, stakes: &BTreeMap<i64, i64>, max_level: i64,
) -> rusqlite::Result<Result<ContractQuote, String>> {
    Ok(contract_stakes(&database.lock(), wallet, stakes, max_level)?.map(|(_, odds)| odds.quote))
}

pub(crate) fn sign_contract(
    database: &Database, wallet: Wallet<'_>, stakes: &BTreeMap<i64, i64>, expected: &ContractQuote,
    settings: &PetSettings, mut draw: impl FnMut(u32) -> u32,
) -> rusqlite::Result<Result<ContractResult, String>> {
    database.transaction(|tx| {
        let (selected, odds) = match contract_stakes(tx, wallet, stakes, settings.max_level)? {
            Ok(contract) => contract,
            Err(message) => return Ok(Err(message)),
        };
        if expected != &odds.quote {
            return Ok(Err("Условия контракта изменились. Обнови расчёт.".into()));
        }
        let dearer = draw(UPGRADE_SCALE) < odds.chance;
        let pool = if dearer { odds.dearer } else { odds.cheaper };
        let prize = pool[draw(pool.len() as u32) as usize];
        let now = db::now();
        let (balance, active) = consume_stakes(tx, wallet, &selected, now, settings.collect_cap_hours)?;
        insert_owned_pet(tx, wallet, (prize.species, prize.level), now, odds.quote.stake, active)?;
        Ok(Ok(ContractResult { prize, stake: odds.quote.stake, dearer, balance }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;

    #[test]
    fn ranges_and_odds_hold_for_small_and_large_stakes() {
        let sample = contract_odds(810, 20).unwrap();
        assert_eq!((sample.chance, sample.quote.range), (3369, (330, 1500)));

        for max_level in [1, 20] {
            let limit = max_stake(max_level);
            assert!(contract_odds(i128::from(limit + 1), max_level).is_none());
            for stake in (720..limit).step_by(30).chain([limit]) {
                let odds = contract_odds(i128::from(stake), max_level).unwrap();
                assert!((1..UPGRADE_SCALE).contains(&odds.chance));
                assert!(odds.cheaper.iter().all(|prize| prize.price() <= stake / 2));
                assert!(odds.dearer.iter().all(|prize| prize.price() * 100 >= stake * 175));
                for prize in odds.cheaper.iter().chain(&odds.dearer) {
                    assert_ne!(prize.species.rarity, "secret");
                    assert!((1..=max_level).contains(&prize.level));
                    if prize.species.key == "fat_oleg" {
                        assert_eq!(prize.level, 1);
                    }
                }
                let average = |pool: &[ContractPrize]| -> f64 {
                    pool.iter().map(|prize| prize.price() as f64).sum::<f64>() / pool.len() as f64
                };
                let cheaper = average(&odds.cheaper);
                let dearer = average(&odds.dearer);
                let probability = f64::from(odds.chance) / f64::from(UPGRADE_SCALE);
                let expected = cheaper + (dearer - cheaper) * probability;
                assert!(expected <= stake as f64 * 0.9 + 1e-7);
                assert!(expected >= stake as f64 * 0.9 - (dearer - cheaper) / f64::from(UPGRADE_SCALE) - 1e-7);
            }
        }
    }

    #[test]
    fn signing_uses_the_quote_and_consumes_only_owned_pets_once() {
        let db = Database::in_memory().unwrap();
        let settings = PetSettings::load(&Settings::default());
        let wallet = Wallet { guild: 7, user: 1, name: "Alice", start: 50 };
        let now = db::now();
        let foreign = db.add_pet(7, 2, "valera", now, 0).unwrap();
        let a = db.add_pet(7, 1, "valera", now, 0).unwrap();
        let b = db.add_pet(7, 1, "valera", now, 0).unwrap();
        let c = db.add_pet(7, 1, "valera", now, 0).unwrap();
        let stakes = [(a, 1), (b, 1), (c, 1)].into();
        let quote = contract_quote(&db, wallet, &stakes, settings.max_level).unwrap().unwrap();
        assert_eq!(quote.stake, 900);
        let chance = contract_odds(i128::from(quote.stake), settings.max_level).unwrap().chance;
        let sign = |bets, expected, draw| sign_contract(&db, wallet, bets, expected, &settings, draw);
        assert!(contract_quote(&db, wallet, &[(a, 1), (b, 1)].into(), settings.max_level).unwrap().is_err());
        let wrong = |_: u32| panic!("invalid contract must not draw");
        assert!(sign_contract(&db, wallet, &[(a, 1), (b, 1), (foreign, 1)].into(),
            &quote, &settings, wrong).unwrap().is_err());
        let stale = ContractQuote { stake: quote.stake, range: (quote.range.0, quote.range.1 - 30) };
        assert!(sign_contract(&db, wallet, &stakes, &stale, &settings, wrong).unwrap().is_err());
        assert_eq!(db.user_pets(7, 1).unwrap().len(), 3);

        let draw = |limit| if limit == UPGRADE_SCALE { chance - 1 } else { 0 };
        let result = sign(&stakes, &quote, draw).unwrap().unwrap();
        assert!(result.dearer && result.prize.price() * 100 >= quote.stake * 175);
        let pets = db.user_pets(7, 1).unwrap();
        assert_eq!(pets.len(), 1);
        assert_eq!((pets[0].species.as_str(), pets[0].level), (result.prize.species.key, result.prize.level));
        assert!(pets[0].inactive_since.is_none());
        assert!(db.own_pet(foreign, 7, 2).unwrap().is_some());
        assert!(sign(&stakes, &quote, draw).unwrap().is_err());
    }

    #[test]
    fn cheaper_prize_and_accrued_income_survive_the_exchange() {
        let db = Database::in_memory().unwrap();
        let settings = PetSettings::load(&Settings::default());
        let wallet = Wallet { guild: 7, user: 1, name: "Alice", start: 50 };
        let now = db::now();
        let stakes = (0..3).map(|_| {
            let pet_id = db.add_pet(7, 1, "valera", now - 3600.0, 0).unwrap();
            super::super::move_pet(&db, wallet, pet_id, false, None, now).unwrap().unwrap();
            (pet_id, 1)
        }).collect();
        let quote = contract_quote(&db, wallet, &stakes, settings.max_level).unwrap().unwrap();
        let chance = contract_odds(i128::from(quote.stake), settings.max_level).unwrap().chance;
        let result = sign_contract(&db, wallet, &stakes, &quote, &settings,
            |limit| if limit == UPGRADE_SCALE { chance } else { 0 }).unwrap().unwrap();
        assert!(!result.dearer && result.prize.price() < quote.stake);
        assert_eq!(result.balance, 80);
        let pets = db.user_pets(7, 1).unwrap();
        assert_eq!(pets.len(), 1);
        assert!(pets[0].inactive_since.is_some());
    }

    #[test]
    fn an_over_limit_contract_cannot_consume_the_stake() {
        let db = Database::in_memory().unwrap();
        let settings = PetSettings::load(&Settings::default());
        assert_eq!(max_stake(settings.max_level), 54_205);
        let wallet = Wallet { guild: 7, user: 1, name: "Alice", start: 50 };
        let stakes = (0..4).map(|_| {
            let id = db.add_pet(7, 1, "foidka_romana", db::now(), 0).unwrap();
            (id, 1)
        }).collect();
        let before = db.user_pets(7, 1).unwrap();
        let error = contract_quote(&db, wallet, &stakes, settings.max_level).unwrap().unwrap_err();
        assert!(error.contains("54 205"), "{error}");
        let stale = ContractQuote { stake: 66_000, range: (30_000, 94_860) };
        let result = sign_contract(&db, wallet, &stakes, &stale, &settings, |_| panic!("over limit draw")).unwrap();
        assert!(result.is_err());
        assert_eq!(db.user_pets(7, 1).unwrap(), before);
    }
}
