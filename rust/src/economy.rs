//! Экономика: /balance, /work, /daily, /give_money, /coinflip. Тексты и правила — как в bot.py.

use crate::db::Wallet;
use crate::reply::{display_name, insufficient_money, say, user_name, whisper};
use crate::text::{hours_text, money_text, time_left_text};
use crate::{db, Context, Error};
use chrono::{Duration, NaiveDate, TimeZone};
use poise::serenity_prelude as serenity;
use rand::seq::IndexedRandom;
use rand::Rng;
use rusqlite::{params, OptionalExtension};

pub const MAX_BET: i64 = 1_000_000;
const ALL_IN_WORDS: [&str; 7] = ["all", "все", "всё", "ва-банк", "вабанк", "allin", "all-in"];
const WORK_TEXTS: [&str; 10] = [
    "Ремонт осциллографа в лаборатории",
    "Пайка усилителя для соседа",
    "Репетиторство по физике",
    "Настройка роутера для бабушки",
    "Ночная отладка чужого кода",
    "Доставка пиццы по общаге",
    "Сборка компьютера на заказ",
    "Перемотка трансформатора",
    "Смена в серверной",
    "Лабораторная работа за двоих",
];

/// Настройки экономики из settings.ini, раздел [economy].
pub struct EconomySettings {
    pub start_balance: i64,
    pub work_cooldown: i64, // секунды
    pub work_min: i64,
    pub work_max: i64,
    pub min_bet: i64,
    pub daily_bonus: i64,
    pub daily_streak_bonus: i64,
    pub daily_streak_max: i64,
    pub timezone: chrono_tz::Tz,
}

impl EconomySettings {
    pub fn load(settings: &crate::config::Settings) -> Self {
        let (a, b) = (settings.int("economy", "work_min", 150), settings.int("economy", "work_max", 400));
        let zone = settings.text("economy", "timezone", "Europe/Moscow");
        Self {
            start_balance: settings.int("economy", "start_balance", 500),
            work_cooldown: settings.int("economy", "work_cooldown_hours", 8).max(0).saturating_mul(3600),
            work_min: a.min(b).max(0),
            work_max: a.max(b).max(0),
            min_bet: settings.int("economy", "min_bet", 10).max(1),
            daily_bonus: settings.int("economy", "daily_bonus", 200).max(0),
            daily_streak_bonus: settings.int("economy", "daily_streak_bonus", 50).max(0),
            daily_streak_max: settings.int("economy", "daily_streak_max", 7).max(1),
            timezone: zone.parse().unwrap_or_else(|_| {
                println!("settings.ini: часовой пояс «{zone}» не найден, беру Europe/Moscow");
                chrono_tz::Europe::Moscow
            }),
        }
    }

    /// Сколько монет даёт бонус на таком-то дне серии.
    pub fn daily_reward(&self, streak: i64) -> i64 {
        self.daily_bonus.saturating_add(
            self.daily_streak_bonus.saturating_mul(streak.clamp(1, self.daily_streak_max) - 1),
        )
    }
}

/// Какой станет серия дней, если забрать бонус сегодня. None — сегодня бонус уже забран.
pub fn next_streak(last: Option<(&str, i64)>, today: NaiveDate) -> Option<i64> {
    let Some((last_day, streak)) = last else { return Some(1) };
    if last_day == today.to_string() {
        return None;
    }
    let yesterday = (today - Duration::days(1)).to_string();
    Some(if last_day == yesterday { streak.max(0).saturating_add(1) } else { 1 })
}

/// Ставка из поля bet: число или all. Ok(ставка) или Err(текст, что не так).
pub fn parse_bet(text: &str, balance: i64, min_bet: i64) -> Result<i64, String> {
    let text = text.trim().to_lowercase().replace(' ', "");
    if ALL_IN_WORDS.contains(&text.as_str()) {
        if balance < min_bet {
            return Err(format!(
                "Ставить нечего: у тебя {}, а минимальная ставка — {}. Заработать можно через /work.",
                money_text(balance), money_text(min_bet)));
        }
        return Ok(balance.min(MAX_BET));
    }
    let Ok(bet) = text.parse::<i64>() else {
        return Err("Ставка — это число монет (например, 100) или **all**, чтобы поставить всё.".into());
    };
    if !text.chars().all(|c| c.is_ascii_digit()) || !(min_bet..=MAX_BET).contains(&bet) {
        return Err(format!("Ставка — от {} до {}.", money_text(min_bet), money_text(MAX_BET)));
    }
    Ok(bet)
}

fn claim_work(
    database: &db::Database,
    wallet: Wallet<'_>,
    earned: i64,
    time: f64,
    cooldown: i64,
) -> rusqlite::Result<Result<i64, (i64, f64)>> {
    database.transaction(|tx| {
        let (balance, last_work) = wallet.open(tx)?;
        let remaining = last_work + cooldown as f64 - time;
        if remaining > 0.0 {
            return Ok(Err((balance, remaining)));
        }
        let balance = db::change_balance(tx, wallet.guild, wallet.user, earned)?;
        tx.execute(
            "UPDATE economy SET last_work = ?3 WHERE guild_id = ?1 AND user_id = ?2",
            params![wallet.guild as i64, wallet.user as i64, time],
        )?;
        Ok(Ok(balance))
    })
}

fn claim_daily(
    database: &db::Database,
    wallet: Wallet<'_>,
    economy: &EconomySettings,
    today: NaiveDate,
) -> rusqlite::Result<Result<(i64, i64, i64), i64>> {
    database.transaction(|tx| {
        let last = tx.query_row(
            "SELECT last_day, streak FROM daily_bonus WHERE guild_id = ?1 AND user_id = ?2",
            params![wallet.guild as i64, wallet.user as i64],
            |row| Ok((row.get::<_, Option<String>>(0)?.unwrap_or_default(), row.get::<_, Option<i64>>(1)?.unwrap_or(0))),
        ).optional()?;
        let Some(streak) = next_streak(last.as_ref().map(|(day, streak)| (day.as_str(), *streak)), today) else {
            return Ok(Err(last.map_or(0, |(_, streak)| streak)));
        };
        wallet.open(tx)?;
        let reward = economy.daily_reward(streak);
        let balance = db::change_balance(tx, wallet.guild, wallet.user, reward)?;
        tx.execute(
            "INSERT INTO daily_bonus (guild_id, user_id, last_day, streak) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (guild_id, user_id) DO UPDATE SET last_day = excluded.last_day, streak = excluded.streak",
            params![wallet.guild as i64, wallet.user as i64, today.to_string(), streak],
        )?;
        Ok(Ok((streak, reward, balance)))
    })
}

fn transfer_money(
    database: &db::Database,
    sender: Wallet<'_>,
    recipient: Wallet<'_>,
    amount: i64,
) -> rusqlite::Result<Result<i64, i64>> {
    if amount <= 0 || sender.guild != recipient.guild || sender.user == recipient.user {
        return Err(rusqlite::Error::InvalidQuery);
    }
    database.transaction(|tx| {
        let (balance, _) = sender.open(tx)?;
        if balance < amount {
            return Ok(Err(balance));
        }
        recipient.open(tx)?;
        let left = db::change_balance(tx, sender.guild, sender.user, -amount)?;
        db::change_balance(tx, recipient.guild, recipient.user, amount)?;
        Ok(Ok(left))
    })
}

/// Результат мгновенной игры: множитель выплаты 0 проигрывает ставку, 1 возвращает её.
fn settle_bet(
    database: &db::Database,
    wallet: Wallet<'_>,
    input: &str,
    minimum: i64,
    multiplier: i64,
) -> rusqlite::Result<Result<(i64, i64), String>> {
    database.transaction(|tx| {
        let (balance, _) = wallet.open(tx)?;
        let bet = match parse_bet(input, balance, minimum) {
            Ok(bet) => bet,
            Err(reason) => return Ok(Err(reason)),
        };
        if balance < bet {
            return Ok(Err(insufficient_money(balance, bet, "ставка")));
        }
        let net = (multiplier - 1) * bet; // ограниченные таблицей игры множители и MAX_BET
        let new_balance = db::change_balance(tx, wallet.guild, wallet.user, net)?;
        Ok(Ok((bet, new_balance)))
    })
}

/// Посмотреть баланс монет
#[poise::command(slash_command, guild_only)]
pub async fn balance(
    ctx: Context<'_>,
    #[description = "Чей баланс посмотреть (если не выбрать — твой)"] member: Option<serenity::User>,
) -> Result<(), Error> {
    let (data, guild) = (ctx.data(), ctx.guild_id().unwrap().get());
    let own = member.as_ref().is_none_or(|member| member.id == ctx.author().id);
    let user = member.unwrap_or_else(|| ctx.author().clone());
    if user.bot {
        return whisper(ctx, "У ботов кошельков нет 🙂").await;
    }
    let (balance, last_work) = data.db.wallet(guild, user.id.get(), &user_name(&user), data.economy.start_balance)?;
    let mut text = format!(
        "💰 Баланс <@{}>: **{}** · место в рейтинге **#{}**",
        user.id, money_text(balance), data.db.money_position(guild, balance)?);
    if own {
        let passed = db::now() - last_work;
        if passed >= data.economy.work_cooldown as f64 {
            text += "\n✅ Можно поработать: /work";
        } else {
            text += &format!("\n⏳ Следующая смена через {}", time_left_text(data.economy.work_cooldown as f64 - passed));
        }
    }
    say(ctx, text).await
}

/// Поработать и заработать монеты
#[poise::command(slash_command, guild_only)]
pub async fn work(ctx: Context<'_>) -> Result<(), Error> {
    let (data, guild, user) = (ctx.data(), ctx.guild_id().unwrap().get(), ctx.author().id.get());
    let economy = &data.economy;
    let name = display_name(ctx).await;
    let (earned, job) = {
        let mut rng = rand::rng();
        (rng.random_range(economy.work_min..=economy.work_max), *WORK_TEXTS.choose(&mut rng).unwrap())
    };
    let wallet = Wallet { guild, user, name: &name, start: economy.start_balance };
    let balance = match claim_work(&data.db, wallet, earned, db::now(), economy.work_cooldown)? {
        Ok(balance) => balance,
        Err((balance, remaining)) => return whisper(ctx, format!(
            "⏳ Рано! Следующая смена через **{}**. Баланс: {}.",
            time_left_text(remaining), money_text(balance))).await,
    };
    say(ctx, format!(
        "💼 {job} — <@{user}> получает **+{}**.\nБаланс: **{}**. Следующая смена — через {}.",
        money_text(earned), money_text(balance), hours_text(economy.work_cooldown))).await
}

/// Забрать ежедневный бонус (каждый день подряд — больше)
#[poise::command(slash_command, guild_only)]
pub async fn daily(ctx: Context<'_>) -> Result<(), Error> {
    let (data, guild, user) = (ctx.data(), ctx.guild_id().unwrap().get(), ctx.author().id.get());
    let economy = &data.economy;
    let name = display_name(ctx).await;
    let today = chrono::Utc::now().with_timezone(&economy.timezone).date_naive();
    let wallet = Wallet { guild, user, name: &name, start: economy.start_balance };
    let (streak, reward, balance) = match claim_daily(&data.db, wallet, economy, today)? {
        Ok(grant) => grant,
        Err(streak) => {
            let midnight = (today + Duration::days(1)).and_hms_opt(0, 0, 0).unwrap();
            let tomorrow = economy.timezone.from_local_datetime(&midnight).earliest().map(|time| time.timestamp()).unwrap_or(0);
            return whisper(ctx, format!(
                "🎁 Сегодняшний бонус уже забран. Следующий — <t:{tomorrow}:R> (+{}).",
                money_text(economy.daily_reward(streak.saturating_add(1))))).await;
        }
    };
    let mut text = format!(
        "🎁 <@{user}> забирает ежедневный бонус: **+{}**. Баланс: **{}**.\n🔥 Дней подряд: **{streak}**",
        money_text(reward), money_text(balance));
    if streak < economy.daily_streak_max {
        text += &format!(" — завтра будет +{}.", money_text(economy.daily_reward(streak + 1)));
    } else {
        text += " — это максимальный бонус, главное не пропускать.";
    }
    say(ctx, text).await
}

/// Перевести монеты другому участнику
#[poise::command(slash_command, guild_only)]
pub async fn give_money(
    ctx: Context<'_>,
    #[description = "Кому перевести"] member: serenity::User,
    #[description = "Сколько монет"]
    #[min = 1]
    #[max = 1_000_000_000]
    amount: i64,
) -> Result<(), Error> {
    let (data, guild, user) = (ctx.data(), ctx.guild_id().unwrap().get(), ctx.author().id.get());
    let start = data.economy.start_balance;
    if member.bot {
        return whisper(ctx, "Ботам монеты ни к чему 🙂").await;
    }
    if member.id == ctx.author().id {
        return whisper(ctx, "Самому себе перевести нельзя.").await;
    }
    let sender_name = display_name(ctx).await;
    let recipient_name = user_name(&member);
    let sender = Wallet { guild, user, name: &sender_name, start };
    let recipient = Wallet { guild, user: member.id.get(), name: &recipient_name, start };
    let new_balance = match transfer_money(&data.db, sender, recipient, amount)? {
        Ok(balance) => balance,
        Err(balance) => return whisper(ctx, format!("Не хватает монет: у тебя {}.", money_text(balance))).await,
    };
    // Получателя упоминаем с пингом — пусть узнает о переводе
    ctx.say(format!(
        "💸 <@{user}> переводит <@{}> **{}**.\nОстаток у отправителя: {}.",
        member.id, money_text(amount), money_text(new_balance))).await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, poise::ChoiceParameter)]
pub enum CoinSide {
    #[name = "🦅 Орёл"]
    Heads,
    #[name = "🪙 Решка"]
    Tails,
}

/// Подбросить монету на ставку (угадал — ставка удваивается)
#[poise::command(slash_command, guild_only)]
pub async fn coinflip(
    ctx: Context<'_>,
    #[description = "Сколько монет поставить или all — все монеты"] bet: String,
    #[description = "На что ставишь"] side: CoinSide,
) -> Result<(), Error> {
    let (data, guild, user) = (ctx.data(), ctx.guild_id().unwrap().get(), ctx.author().id.get());
    let (start, name) = (data.economy.start_balance, display_name(ctx).await);
    let result = if rand::rng().random_bool(0.5) { CoinSide::Heads } else { CoinSide::Tails };
    let wallet = Wallet { guild, user, name: &name, start };
    let multiplier = if result == side { 2 } else { 0 };
    let (bet, new_balance) = match settle_bet(&data.db, wallet, &bet, data.economy.min_bet, multiplier)? {
        Ok(result) => result,
        Err(problem) => return whisper(ctx, problem).await,
    };
    let shown = if result == CoinSide::Heads { "Выпал **орёл** 🦅" } else { "Выпала **решка** 🪙" };
    let text = if result == side {
        format!("{shown}\n🎉 <@{user}> угадывает и выигрывает **+{}**!", money_text(bet))
    } else {
        let chosen = if side == CoinSide::Heads { "орёл" } else { "решка" };
        format!("{shown}\n😔 <@{user}> ставил(а) на «{chosen}» и проигрывает {}.", money_text(bet))
    };
    say(ctx, format!("{text}\nБаланс: {}", money_text(new_balance))).await
}

// Слоты: символ, «вес» (чем больше, тем чаще выпадает) и выплата за три одинаковых (во сколько раз
// умножается ставка). Два одинаковых символа — ставка возвращается. В среднем игрок получает обратно около 93%.
const SLOTS: [(&str, u32, i64); 6] =
    [("🍒", 30, 5), ("🍋", 25, 8), ("🍇", 20, 12), ("🔔", 12, 25), ("💎", 8, 50), ("7️⃣", 5, 100)];

/// Символ на барабане по случайному числу от 0 до суммы весов.
pub fn slot_symbol(mut roll: u32) -> usize {
    for (index, (_, weight, _)) in SLOTS.iter().enumerate() {
        if roll < *weight {
            return index;
        }
        roll -= weight;
    }
    SLOTS.len() - 1
}

/// Во сколько раз умножается ставка: три одинаковых — по таблице, два одинаковых — 1 (возврат), иначе 0.
pub fn slots_multiplier(reels: [usize; 3]) -> i64 {
    if reels[0] == reels[1] && reels[1] == reels[2] {
        SLOTS[reels[0]].2
    } else if reels[0] == reels[1] || reels[1] == reels[2] || reels[0] == reels[2] {
        1
    } else {
        0
    }
}

/// Сыграть в слоты на монеты
#[poise::command(slash_command, guild_only)]
pub async fn slots(
    ctx: Context<'_>,
    #[description = "Сколько монет поставить или all — все монеты"] bet: String,
) -> Result<(), Error> {
    let (data, guild, user) = (ctx.data(), ctx.guild_id().unwrap().get(), ctx.author().id.get());
    let (start, name) = (data.economy.start_balance, display_name(ctx).await);
    let reels = {
        let total: u32 = SLOTS.iter().map(|(_, weight, _)| weight).sum();
        let mut rng = rand::rng();
        [0; 3].map(|_| slot_symbol(rng.random_range(0..total)))
    };
    let multiplier = slots_multiplier(reels);
    let wallet = Wallet { guild, user, name: &name, start };
    let (bet, new_balance) = match settle_bet(&data.db, wallet, &bet, data.economy.min_bet, multiplier)? {
        Ok(result) => result,
        Err(problem) => return whisper(ctx, problem).await,
    };
    let payout = bet * multiplier;
    let outcome = if multiplier > 1 {
        format!("🎉 Три {}! Выигрыш ×{multiplier}: **+{}**", SLOTS[reels[0]].0, money_text(payout - bet))
    } else if multiplier == 1 {
        "😌 Два одинаковых — ставка возвращается.".to_string()
    } else {
        format!("😔 Мимо. Проигрыш: {}.", money_text(bet))
    };
    let shown = reels.map(|index| SLOTS[index].0).join(" ");
    let paytable = SLOTS.iter().map(|(symbol, _, payout)| format!("{symbol}×{payout}")).collect::<Vec<_>>().join(" ");
    // "-# " — мелкий серый текст в Discord
    say(ctx, format!(
        "🎰 <@{user}>\n# {shown}\n{outcome}\nБаланс: {}\n-# Три одинаковых: {paytable} · два одинаковых — возврат ставки",
        money_text(new_balance))).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_machine() {
        assert_eq!([slot_symbol(0), slot_symbol(29), slot_symbol(30), slot_symbol(54), slot_symbol(55), slot_symbol(99)],
                   [0, 0, 1, 1, 2, 5]);
        assert_eq!(slots_multiplier([5, 5, 5]), 100);
        assert_eq!(slots_multiplier([0, 0, 0]), 5);
        assert_eq!(slots_multiplier([0, 3, 0]), 1);
        assert_eq!(slots_multiplier([0, 1, 2]), 0);
        // В среднем автомат возвращает около 93% ставок — как в bot.py
        let total: u32 = SLOTS.iter().map(|(_, weight, _)| weight).sum();
        let mut returned = 0.0;
        for a in 0..total {
            for b in 0..total {
                for c in 0..total {
                    returned += slots_multiplier([slot_symbol(a), slot_symbol(b), slot_symbol(c)]) as f64;
                }
            }
        }
        let average = returned / (total as f64).powi(3);
        assert!((0.90..0.96).contains(&average), "возврат {average}");
    }

    fn settings() -> EconomySettings {
        EconomySettings::load(&crate::config::Settings::default())
    }

    #[test]
    fn daily_rewards_grow_and_stop() {
        let economy = settings();
        let rewards: Vec<i64> = (1..=9).map(|streak| economy.daily_reward(streak)).collect();
        assert_eq!(rewards, [200, 250, 300, 350, 400, 450, 500, 500, 500]);
    }

    #[test]
    fn streak_rules() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 11).unwrap();
        assert_eq!(next_streak(None, today), Some(1));
        assert_eq!(next_streak(Some(("2026-11-11", 3)), today), None); // сегодня уже забирал
        assert_eq!(next_streak(Some(("2026-11-10", 3)), today), Some(4)); // вчера — серия растёт
        assert_eq!(next_streak(Some(("2026-11-09", 3)), today), Some(1)); // пропустил день
    }

    #[test]
    fn bets() {
        assert_eq!(parse_bet("100", 500, 10), Ok(100));
        assert_eq!(parse_bet(" 1 000 ", 5000, 10), Ok(1000));
        assert_eq!(parse_bet("ALL", 500, 10), Ok(500));
        assert_eq!(parse_bet("всё", 5_000_000, 10), Ok(MAX_BET));
        assert!(parse_bet("all", 5, 10).unwrap_err().starts_with("Ставить нечего"));
        assert!(parse_bet("abc", 500, 10).unwrap_err().starts_with("Ставка — это число"));
        assert!(parse_bet("5", 500, 10).unwrap_err().starts_with("Ставка — от"));
        assert!(parse_bet("-50", 500, 10).is_err());
        assert!(parse_bet("2000000", 500, 10).is_err());
    }

    #[test]
    fn default_settings() {
        let economy = settings();
        assert_eq!((economy.start_balance, economy.work_cooldown, economy.work_min, economy.work_max, economy.min_bet),
                   (500, 8 * 3600, 150, 400, 10));
    }
}
