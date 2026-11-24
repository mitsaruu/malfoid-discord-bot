use super::{find_species, income, Database, Species, Wallet};
use rusqlite::params;
use std::collections::BTreeMap;

const BEST_PET_COUNT: usize = 3;

pub(crate) struct LeaderboardPet {
    pub species: &'static Species,
    pub level: i64,
}

pub(crate) struct LeaderboardPlayer {
    pub user: u64,
    pub name: String,
    pub balance: i64,
    pub xp: i64,
    pub hourly_income: i128,
    pub best_pets: Vec<LeaderboardPet>,
}

pub(crate) fn player_leaderboard(database: &Database, viewer: Wallet<'_>) -> rusqlite::Result<Vec<LeaderboardPlayer>> {
    let connection = database.lock();
    let mut statement = connection.prepare(
        "WITH players(user_id) AS (
             SELECT user_id FROM economy WHERE guild_id = ?1
             UNION SELECT user_id FROM levels WHERE guild_id = ?1
             UNION SELECT user_id FROM pets WHERE guild_id = ?1
             UNION SELECT user_id FROM word_profiles WHERE guild_id = ?1
             UNION SELECT ?2
         )
         SELECT players.user_id, COALESCE(NULLIF(levels.name, ''), NULLIF(economy.name, ''), NULLIF(word_profiles.name, '')),
                CASE WHEN economy.user_id IS NULL THEN ?3 ELSE COALESCE(economy.balance, 0) END,
                COALESCE(levels.xp, 0), pets.species, COALESCE(pets.level, 1), pets.inactive_since IS NULL
         FROM players
         LEFT JOIN economy ON economy.guild_id = ?1 AND economy.user_id = players.user_id
         LEFT JOIN levels ON levels.guild_id = ?1 AND levels.user_id = players.user_id
         LEFT JOIN pets ON pets.guild_id = ?1 AND pets.user_id = players.user_id
         LEFT JOIN word_profiles ON word_profiles.guild_id = ?1 AND word_profiles.user_id = players.user_id
         WHERE players.user_id > 0
         ORDER BY players.user_id, pets.id",
    )?;
    let mut rows = statement.query(params![viewer.guild as i64, viewer.user as i64, viewer.start.max(0)])?;
    let mut players = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let user = row.get::<_, i64>(0)? as u64;
        let player = match players.entry(user) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let name = if user == viewer.user {
                    viewer.name.to_owned()
                } else {
                    row.get::<_, Option<String>>(1)?.unwrap_or_else(|| "Игрок".to_owned())
                };
                entry.insert(LeaderboardPlayer {
                    user, name, balance: row.get(2)?, xp: row.get(3)?,
                    hourly_income: 0, best_pets: Vec::new(),
                })
            }
        };
        let Some(species) = row.get::<_, Option<String>>(4)?.as_deref().and_then(find_species) else {
            continue;
        };
        let level = row.get::<_, i64>(5)?.max(1);
        let hourly = income(species, level);
        if row.get::<_, bool>(6)? {
            player.hourly_income += i128::from(hourly);
        }
        let position = player.best_pets.partition_point(|pet| income(pet.species, pet.level) >= hourly);
        if position < BEST_PET_COUNT {
            player.best_pets.insert(position, LeaderboardPet { species, level });
            player.best_pets.truncate(BEST_PET_COUNT);
        }
    }
    Ok(players.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaderboard_merges_server_players_and_ranks_owned_pets_at_their_current_level() {
        let db = Database::in_memory().unwrap();
        let viewer = Wallet { guild: 7, user: 8, name: "Луна", start: 500 };
        db.set_xp(7, 1, "Алиса", 255).unwrap();
        db.wallet(7, 1, "Alice", 1000).unwrap();
        db.wallet(7, 2, "Борис", 900).unwrap();
        db.set_xp(7, 3, "Вера", 100).unwrap();
        db.set_xp(6, 9, "Другой сервер", 900).unwrap();
        let pet = |guild, user, species| db.add_pet(guild, user, species, 0.0, 0).unwrap();
        let dragon = pet(7, 1, "vasily");
        db.feed_pet(dragon, 3000, 0.0).unwrap();
        pet(7, 1, "zhorik");
        pet(7, 1, "valera");
        let secret = pet(7, 1, "stellochka");
        db.lock().execute("UPDATE pets SET inactive_since = 0 WHERE id = ?1", [secret]).unwrap();
        pet(6, 1, "stellochka");
        pet(7, 4, "hermes");
        db.lock().execute(
            "INSERT INTO word_profiles (guild_id, user_id, name, face) VALUES (7, 5, 'Мята', '')",
            [],
        ).unwrap();

        let players = player_leaderboard(&db, viewer).unwrap();
        assert_eq!(players.iter().map(|item| item.user).collect::<Vec<_>>(), [1, 2, 3, 4, 5, 8]);
        let alice = &players[0];
        assert_eq!((alice.name.as_str(), alice.balance, alice.xp, alice.hourly_income), ("Алиса", 1000, 255, 263));
        assert_eq!(alice.best_pets.iter().map(|pet| (pet.species.key, pet.level)).collect::<Vec<_>>(),
                   [("stellochka", 1), ("vasily", 2), ("zhorik", 1)]);
        assert_eq!((players[1].balance, players[1].xp), (900, 0));
        assert_eq!((players[2].balance, players[2].xp), (500, 100));
        assert_eq!(players[3].hourly_income, 62);
        assert_eq!((players[4].name.as_str(), players[4].balance), ("Мята", 500));
        assert_eq!((players[5].name.as_str(), players[5].balance), ("Луна", 500));
    }
}
