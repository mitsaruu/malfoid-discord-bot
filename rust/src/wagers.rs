//! Ставка и её выплата или возврат записываются вместе с изменением баланса.

use crate::{db::{change_balance, Database}, economy::parse_bet, text::money_text};
use rusqlite::{params, Transaction, TransactionBehavior};
use std::collections::BTreeMap;

pub fn prepare(db: &Database) -> rusqlite::Result<()> {
    db.lock().execute_batch(
        "CREATE TABLE IF NOT EXISTS game_wagers (
            ticket INTEGER PRIMARY KEY, round_id INTEGER NOT NULL,
            guild_id INTEGER NOT NULL, user_id INTEGER NOT NULL, amount INTEGER NOT NULL CHECK(amount > 0)
        );
        CREATE INDEX IF NOT EXISTS game_wagers_round ON game_wagers(round_id);",
    )
}

pub fn place(
    db: &Database,
    round: u64,
    ticket: u64,
    guild: u64,
    user: u64,
    bet: &str,
    minimum: i64,
) -> rusqlite::Result<Result<i64, String>> {
    let mut conn = db.lock();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let result = place_on(&tx, round, ticket, guild, user, bet, minimum)?;
    if result.is_ok() {
        tx.commit()?;
    }
    Ok(result)
}

pub(crate) fn place_on(
    tx: &Transaction<'_>,
    round: u64,
    ticket: u64,
    guild: u64,
    user: u64,
    bet: &str,
    minimum: i64,
) -> rusqlite::Result<Result<i64, String>> {
    let balance: i64 = tx.query_row(
        "SELECT COALESCE(balance, 0) FROM economy WHERE guild_id = ?1 AND user_id = ?2",
        params![guild as i64, user as i64],
        |row| row.get(0),
    )?;
    let amount = match parse_bet(bet, balance, minimum.max(1)) {
        Ok(amount) => amount,
        Err(error) => return Ok(Err(error)),
    };
    if amount > balance {
        return Ok(Err(format!(
            "Для ставки нужно {}, у тебя {}.",
            money_text(amount),
            money_text(balance)
        )));
    }
    tx.execute(
        "INSERT INTO game_wagers (ticket, round_id, guild_id, user_id, amount) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![ticket as i64, round as i64, guild as i64, user as i64, amount],
    )?;
    change_balance(tx, guild, user, -amount)?;
    Ok(Ok(amount))
}

/// В списке должен быть каждый билет раунда, в том числе проигравшие с выплатой 0.
pub fn settle(db: &Database, round: u64, payouts: &[(u64, i64)]) -> rusqlite::Result<()> {
    db.transaction(|tx| settle_on(tx, round, payouts))
}

pub(crate) fn settle_on(tx: &Transaction<'_>, round: u64, payouts: &[(u64, i64)]) -> rusqlite::Result<()> {
    let total: i64 = tx.query_row(
        "SELECT COUNT(*) FROM game_wagers WHERE round_id = ?1",
        [round as i64],
        |row| row.get(0),
    )?;
    let payouts_by_ticket: BTreeMap<_, _> = payouts.iter().copied().collect();
    if total == 0 || total != payouts.len() as i64 || payouts.len() != payouts_by_ticket.len() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    for (ticket, payout) in payouts_by_ticket {
        if payout < 0 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let (guild, user): (i64, i64) = tx.query_row(
            "SELECT guild_id, user_id FROM game_wagers WHERE round_id = ?1 AND ticket = ?2",
            params![round as i64, ticket as i64],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if payout > 0 {
            change_balance(tx, guild as u64, user as u64, payout)?;
        }
    }
    tx.execute(
        "DELETE FROM game_wagers WHERE round_id = ?1",
        [round as i64],
    )?;
    Ok(())
}

/// None при запуске возвращает ставки всех раундов, оставшихся от прежнего процесса.
pub fn refund(db: &Database, round: Option<u64>) -> rusqlite::Result<usize> {
    let Some(round) = round else {
        let rounds = {
            let conn = db.lock();
            let mut query = conn.prepare("SELECT DISTINCT round_id FROM game_wagers ORDER BY round_id")?;
            let rows = query.query_map([], |row| row.get::<_, i64>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut settled = 0;
        for round in rounds {
            match refund(db, Some(round as u64)) {
                Ok(count) => settled += count,
                Err(error) => eprintln!("Ставки раунда {} остались в журнале: {error}", round as u64),
            }
        }
        return Ok(settled);
    };
    db.transaction(|tx| {
        let mut query = tx.prepare("SELECT guild_id, user_id, amount FROM game_wagers WHERE round_id = ?1")?;
        let rows = query
            .query_map([round as i64], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (guild, user, amount) in rows {
            change_balance(tx, guild as u64, user as u64, amount)?;
        }
        tx.execute("DELETE FROM game_wagers WHERE round_id = ?1", [round as i64])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_table_reservation_can_roll_back_every_player() {
        let db = Database::in_memory().unwrap();
        db.wallet(1, 2, "first", 500).unwrap();
        db.wallet(1, 3, "second", 50).unwrap();
        {
            let mut connection = db.lock();
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
            assert_eq!(place_on(&tx, 7, 10, 1, 2, "100", 10).unwrap(), Ok(100));
            assert!(place_on(&tx, 7, 11, 1, 3, "100", 10).unwrap().is_err());
        }
        assert_eq!(db.wallet(1, 2, "first", 500).unwrap().0, 500);
        assert_eq!(db.wallet(1, 3, "second", 50).unwrap().0, 50);
        assert_eq!(refund(&db, Some(7)).unwrap(), 0);
    }

    #[test]
    fn incomplete_or_duplicate_settlement_cannot_pay_and_refund_only_applies_once() {
        let db = Database::in_memory().unwrap();
        db.wallet(1, 2, "test", 500).unwrap();
        let balance = || db.wallet(1, 2, "test", 500).unwrap().0;
        assert_eq!(place(&db, 4, 5, 1, 2, "100", 10).unwrap(), Ok(100));
        assert_eq!(place(&db, 4, 6, 1, 2, "all", 10).unwrap(), Ok(400));
        assert!(place(&db, 4, 7, 1, 2, "100", 10).unwrap().is_err());
        assert!(settle(&db, 4, &[(5, 200), (9, 800)]).is_err());
        assert_eq!(balance(), 0);
        settle(&db, 4, &[(5, 200), (6, 0)]).unwrap();
        assert!(settle(&db, 4, &[(5, 200), (6, 0)]).is_err());
        assert_eq!(balance(), 200);
        assert_eq!(place(&db, 8, 9, 1, 2, "all", 10).unwrap(), Ok(200));
        assert_eq!(refund(&db, None).unwrap(), 1);
        assert_eq!(refund(&db, None).unwrap(), 0);
        assert_eq!(balance(), 200);
    }
}
