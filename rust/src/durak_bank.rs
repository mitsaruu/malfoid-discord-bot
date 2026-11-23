//! Ставки «Дурака» хранятся в общем журнале ставок до расчёта или возврата.

use crate::{
    Data,
    db::{Database, Wallet},
    durak_online::Bank,
    economy::MAX_BET,
    wagers,
};
use rusqlite::{Error, TransactionBehavior};

const MIN_STAKE: i64 = 10;

impl Bank for Data {
    fn reserve(
        &self,
        round: u64,
        guild: u64,
        players: &[(u64, &str)],
        stake: i64,
    ) -> Result<(), String> {
        reserve(
            &self.db,
            self.economy.start_balance,
            round,
            guild,
            players,
            stake,
        )
        .map_err(bank_error)?
    }

    fn settle(&self, round: u64, loser: Option<u64>) -> Result<(), String> {
        settle(&self.db, round, loser).map_err(bank_error)
    }
}

fn bank_error(error: Error) -> String {
    eprintln!("Ставки в Дураке: {error}");
    "Не удалось провести расчёт ставок. Попробуйте ещё раз.".into()
}

fn reserve(
    db: &Database,
    start: i64,
    round: u64,
    guild: u64,
    players: &[(u64, &str)],
    stake: i64,
) -> rusqlite::Result<Result<(), String>> {
    if !(MIN_STAKE..=MAX_BET).contains(&stake) || !(2..=6).contains(&players.len()) {
        return Ok(Err("Недопустимая ставка или число игроков.".into()));
    }
    let mut connection = db.lock();
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM game_wagers WHERE round_id = ?1)",
        [round as i64],
        |row| row.get(0),
    )?;
    if exists {
        return Err(Error::InvalidQuery);
    }
    let bet = stake.to_string();
    for &(user, name) in players {
        Wallet {
            guild,
            user,
            name,
            start,
        }
        .open(&tx)?;
        if let Err(message) =
            wagers::place_on(&tx, round, rand::random(), guild, user, &bet, MIN_STAKE)?
        {
            return Ok(Err(format!("{name}: {message}")));
        }
    }
    tx.commit()?;
    Ok(Ok(()))
}

fn settle(db: &Database, round: u64, loser: Option<u64>) -> rusqlite::Result<()> {
    let Some(loser) = loser else {
        wagers::refund(db, Some(round))?;
        return Ok(());
    };
    db.transaction(|tx| {
        let tickets = tx.prepare("SELECT ticket, user_id, amount FROM game_wagers WHERE round_id = ?1 ORDER BY user_id")?
            .query_map([round as i64], |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64, row.get::<_, i64>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let lost = tickets.iter().find(|(_, user, _)| *user == loser).ok_or(Error::InvalidQuery)?.2;
        let winners = tickets.len().checked_sub(1).filter(|count| *count > 0).ok_or(Error::InvalidQuery)? as i64;
        let mut extra = lost % winners;
        let payouts: Vec<_> = tickets.into_iter().map(|(ticket, user, stake)| {
            if user == loser {
                return (ticket, 0);
            }
            let bonus = i64::from(extra > 0);
            extra -= bonus;
            (ticket, stake + lost / winners + bonus)
        }).collect();
        wagers::settle_on(tx, round, &payouts)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_reservation_leaves_every_balance_intact() {
        let db = Database::in_memory().unwrap();
        db.wallet(1, 21, "Гость", 9).unwrap();
        let players = [(20, "Ведущий"), (21, "Гость")];
        assert!(reserve(&db, 500, 7, 1, &players, 9).unwrap().is_err());
        assert!(reserve(&db, 500, 7, 1, &players, 10).unwrap().is_err());
        assert_eq!(db.wallet(1, 20, "Ведущий", 500).unwrap().0, 500);
        assert_eq!(db.wallet(1, 21, "Гость", 500).unwrap().0, 9);
        assert_eq!(wagers::refund(&db, Some(7)).unwrap(), 0);
    }

    #[test]
    fn loser_pays_the_winners_and_a_refund_restores_each_stake() {
        let db = Database::in_memory().unwrap();
        let players = [(20, "Один"), (21, "Два"), (22, "Три")];
        reserve(&db, 500, 7, 1, &players, 11).unwrap().unwrap();
        settle(&db, 7, Some(21)).unwrap();
        assert!(settle(&db, 7, Some(21)).is_err());
        let balances = || players.map(|(user, name)| db.wallet(1, user, name, 500).unwrap().0);
        assert_eq!(balances(), [506, 489, 505]);

        reserve(&db, 500, 8, 1, &players, 10).unwrap().unwrap();
        settle(&db, 8, None).unwrap();
        assert_eq!(balances(), [506, 489, 505]);
    }
}
