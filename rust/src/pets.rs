//! Питомцы и кейсы: /case, /cases, /pets, /pets_collect, /pet_feed, /pet_sell.
//! Питомец сам приносит монеты в час; корм поднимает уровень (+25% к доходу за уровень).

use crate::db::{self, Database, Pet, Wallet};
use crate::reply::{challenge, display_name, finish_challenge, insufficient_money, say, user_name, whisper};
use crate::text::{hours_text, money_text, number};
use crate::{Context, Error};
use poise::serenity_prelude as serenity;
use rand::Rng;
use rusqlite::params;

/// Настройки из settings.ini, раздел [pets].
pub struct PetSettings {
    pub max_pets: i64,
    pub max_level: i64,
    pub collect_cap_hours: i64,
    pub case_prices: [i64; 4], // дешёвый, средний, дорогой, секретный
}

impl PetSettings {
    pub fn load(settings: &crate::config::Settings) -> Self {
        Self {
            max_pets: settings.int("pets", "max_pets", 10).max(1),
            max_level: settings.int("pets", "max_level", 20).max(1),
            collect_cap_hours: settings.int("pets", "collect_cap_hours", 24).max(1),
            case_prices: [
                settings.int("pets", "cheap_case_price", 500).max(1),
                settings.int("pets", "medium_case_price", 2000).max(1),
                settings.int("pets", "expensive_case_price", 7500).max(1),
                settings.int("pets", "secret_case_price", 25000).max(1),
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
    const ALL: [Case; 4] = [Case::Cheap, Case::Medium, Case::Expensive, Case::Secret];

    fn emoji(self) -> &'static str {
        ["📦", "🎁", "💎", "🔮"][self as usize]
    }

    fn title(self) -> &'static str {
        ["Дешёвый", "Средний", "Дорогой", "Секретный"][self as usize]
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
    Rarity { key: "mythic", name: "Мифический", color: 0xEF4444, chances: [0.01, 0.2, 1.5, 5.0] },
    Rarity { key: "secret", name: "Секретный", color: 0x22D3EE, chances: [0.0, 0.0, 0.0, 0.05] },
];

/// Вид питомца: ключ из базы, эмодзи, название, редкость, базовый доход и цена продажи.
pub struct Species {
    pub key: &'static str,
    pub emoji: &'static str,
    pub name: &'static str,
    pub rarity: &'static str,
    pub income: i64,
    pub sale_price: i64,
}

const fn species(
    key: &'static str, emoji: &'static str, name: &'static str, rarity: &'static str, income: i64, sale_price: i64,
) -> Species {
    Species { key, emoji, name, rarity, income, sale_price }
}

// Средняя цена продажи выпавшего питомца ниже стандартной цены каждого кейса.
pub const SPECIES: [Species; 17] = [
    species("valera", "🐱", "котик", "common", 10, 200),
    species("gena", "🐹", "хомяк", "common", 12, 240),
    species("tolik", "🐦", "голубь", "common", 8, 160),
    species("frog", "🐸", "лягушка", "common", 9, 180),
    species("arkady", "🦊", "лисичка", "rare", 25, 600),
    species("tamara", "🦉", "сова", "rare", 25, 650),
    species("boris", "🐢", "черепаха", "rare", 22, 500),
    species("igor", "🐺", "волк", "epic", 60, 1700),
    species("edik", "🐧", "пингвин", "epic", 55, 1500),
    species("zhorik", "🦝", "енот", "epic", 65, 1900),
    species("vasily", "🐉", "дракон", "legendary", 150, 4800),
    species("lyudmila", "🦄", "единорог", "legendary", 140, 4300),
    species("lion", "🦁", "лев", "legendary", 160, 5300),
    species("danya_dog", "🐕", "Даня собака", "legendary", 170, 5800),
    species("sergey", "🐶", "собака Сергей", "mythic", 500, 10000),
    species("foidka_romana", "🧚", "Фоидка романа", "mythic", 550, 12000),
    species("stellochka", "🌟", "Стеллочка", "secret", 1500, 20000),
];

const STAGES: [(i64, &str); 4] = [(1, "малыш"), (5, "подросток"), (10, "взрослый"), (15, "легенда района")];

pub fn find_species(key: &str) -> Option<&'static Species> {
    SPECIES.iter().find(|species| species.key == key)
}

fn rarity_of(species: &Species) -> &'static Rarity {
    RARITIES.iter().find(|rarity| rarity.key == species.rarity).unwrap()
}

fn needs_sale_confirmation(species: &Species) -> bool {
    species.rarity != "common"
}

pub fn stage(level: i64) -> &'static str {
    STAGES.iter().rev().find(|(start, _)| level >= *start).map(|(_, name)| *name).unwrap_or(STAGES[0].1)
}

/// Монет в час у питомца этого вида и уровня (округление — как round() в Python: половинки к чётному).
pub fn income(species: &Species, level: i64) -> i64 {
    (species.income as f64 * (1.0 + 0.25 * (level.max(1) - 1) as f64)).round_ties_even() as i64
}

/// Сколько стоит покормить питомца, чтобы он вырос с level до level + 1.
pub fn feed_cost(species: &Species, level: i64) -> i64 {
    (species.income * 20).saturating_mul(level.max(1))
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
fn sorted_pets(pets: Vec<Pet>) -> Vec<(Pet, &'static Species)> {
    let mut pets: Vec<(Pet, &'static Species)> =
        pets.into_iter().filter_map(|pet| find_species(&pet.species).map(|species| (pet, species))).collect();
    pets.sort_by_key(|(pet, species)| (-income(species, pet.level), pet.id));
    pets
}

fn buy_pet(
    database: &Database,
    wallet: Wallet<'_>,
    species: &Species,
    price: i64,
    max_pets: i64,
) -> rusqlite::Result<Result<(), String>> {
    database.transaction(|tx| {
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM pets WHERE guild_id = ?1 AND user_id = ?2",
            params![wallet.guild as i64, wallet.user as i64],
            |row| row.get(0),
        )?;
        if count >= max_pets {
            return Ok(Err(format!(
                "🏠 У тебя уже {max_pets} питомцев — больше не поместится. Продай кого-нибудь: /pet_sell")));
        }
        let (balance, _) = wallet.open(tx)?;
        if balance < price {
            return Ok(Err(insufficient_money(balance, price, "цена кейса")));
        }
        db::change_balance(tx, wallet.guild, wallet.user, -price)?;
        tx.execute(
            "INSERT INTO pets (guild_id, user_id, species, level, last_collect, spent) VALUES (?1, ?2, ?3, 1, ?4, ?5)",
            params![wallet.guild as i64, wallet.user as i64, species.key, db::now(), price],
        )?;
        Ok(Ok(()))
    })
}

fn collect_pets(
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
            total.checked_add(pending(species, pet.level, pet.last_collect, now, cap_hours))
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
                params![pet.id, now],
            )?;
        }
        Ok(Ok((total, balance)))
    })
}

type Feeding = (Pet, &'static Species, i64);

fn feed_owned_pet(
    database: &Database,
    wallet: Wallet<'_>,
    id: i64,
    settings: &PetSettings,
) -> rusqlite::Result<Result<Feeding, String>> {
    database.transaction(|tx| {
        let found = Database::own_pet_on(tx, id, wallet.guild, wallet.user)?
            .and_then(|pet| find_species(&pet.species).map(|species| (pet, species)));
        let Some((pet, species)) = found else {
            return Ok(Err("Такого питомца у тебя нет — выбери из списка.".into()));
        };
        if pet.level >= settings.max_level {
            return Ok(Err(format!("{} уже вырос до максимума!", title(species, pet.level))));
        }
        let cost = feed_cost(species, pet.level);
        let (balance, _) = wallet.open(tx)?;
        if balance < cost {
            return Ok(Err(insufficient_money(balance, cost, "корм")));
        }
        let spent = pet.spent.checked_add(cost).ok_or(rusqlite::Error::IntegralValueOutOfRange(0, pet.spent))?;
        let now = db::now();
        let saved = pending(species, pet.level, pet.last_collect, now, settings.collect_cap_hours);
        let new_level = pet.level.max(1) + 1;
        let last_collect = now - saved as f64 / income(species, new_level) as f64 * 3600.0;
        db::change_balance(tx, wallet.guild, wallet.user, -cost)?;
        tx.execute(
            "UPDATE pets SET level = ?2, spent = ?3, last_collect = ?4 WHERE id = ?1",
            params![pet.id, new_level, spent, last_collect],
        )?;
        Ok(Ok((pet, species, cost)))
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
        .map(|(index, (pet, species))| (format!("{}. {}", index + 1, title(species, pet.level)), pet.id, species))
        .filter(|(label, _, _)| label.to_lowercase().contains(&partial))
        .take(25)
        .map(|(mut label, id, species)| {
            if show_sale_price {
                label.push_str(&format!(" — {}", money_text(species.sale_price)));
            }
            serenity::AutocompleteChoice::new(label, id)
        })
        .collect();
    response.set_choices(choices)
}

/// Открыть кейс с питомцем: дешёвый, средний, дорогой или секретный
#[poise::command(slash_command, guild_only, rename = "case")]
pub async fn open_case(ctx: Context<'_>, #[description = "Какой кейс открыть"] kind: Case) -> Result<(), Error> {
    let (data, guild, user) = (ctx.data(), ctx.guild_id().unwrap().get(), ctx.author().id.get());
    let price = data.pets.case_prices[kind as usize];
    let (roll, pick) = {
        let mut rng = rand::rng();
        (rng.random_range(0.0..100.0), rng.random_range(0..usize::MAX))
    };
    let species = roll_pet(kind, roll, pick);
    let name = display_name(ctx).await;
    let wallet = Wallet { guild, user, name: &name, start: data.economy.start_balance };
    if let Err(reason) = buy_pet(&data.db, wallet, species, price, data.pets.max_pets)? {
        return whisper(ctx, reason).await;
    }

    let no_pings = serenity::CreateAllowedMentions::new();
    let message = ctx
        .send(poise::CreateReply::default()
            .content(format!("{} <@{user}> открывает {} кейс...", kind.emoji(), kind.title().to_lowercase()))
            .allowed_mentions(no_pings.clone()))
        .await?;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await; // немного интриги
    let rarity = rarity_of(species);
    let ending = if rarity.key == "secret" { "!!! Самый редкий в игре!" } else { "!" };
    let embed = serenity::CreateEmbed::new()
        .color(rarity.color)
        .title(format!("{} {}", species.emoji, species.name))
        .description(format!(
            "**{}** питомец{ending}\nПриносит **{} в час**.", rarity.name, money_text(income(species, 1))))
        .author(serenity::CreateEmbedAuthor::new(name).icon_url(ctx.author().face()))
        .footer(serenity::CreateEmbedFooter::new("Забрать доход — /pets_collect · вырастить — /pet_feed"));
    message.edit(ctx, poise::CreateReply::default().content("").embed(embed).allowed_mentions(no_pings)).await?;
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
        let (hourly, saved) = (income(species, pet.level), pending(species, pet.level, pet.last_collect, now, cap));
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
    let embed = serenity::CreateEmbed::new()
        .color(0x22C55E)
        .title(format!("🐾 Питомцы {owner_name} ({}/{})", pets.len(), data.pets.max_pets))
        .description(lines.join("\n"))
        .field("Доход", format!("{} в час", money_text(total_income)), true)
        .field("Можно забрать", money_text(total_pending), true)
        .footer(serenity::CreateEmbedFooter::new(format!(
            "Доход копится до {} — потом питомцы ждут, пока заберёшь", hours_text(cap * 3600))));
    ctx.send(poise::CreateReply::default().embed(embed)).await?;
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
    let (pet, species, cost) = match feed_owned_pet(&data.db, wallet, pet, &data.pets)? {
        Ok(result) => result,
        Err(reason) => return whisper(ctx, reason).await,
    };
    let max_level = data.pets.max_level;
    let new_level = pet.level.max(1) + 1;
    let new_income = income(species, new_level);
    let grown = if stage(new_level) != stage(pet.level) {
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
        let earned = pending(species, pet.level, pet.last_collect, now(), cap_hours);
        let payout = species.sale_price.checked_add(earned)
            .ok_or(rusqlite::Error::IntegralValueOutOfRange(0, earned))?;
        wallet.open(tx)?;
        tx.execute("DELETE FROM pets WHERE id = ?1", [pet.id])?;
        let balance = db::change_balance(tx, wallet.guild, wallet.user, payout)?;
        Ok(Some((earned, balance)))
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
            title(species, pet.level), money_text(species.sale_price));
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
        title(species, pet.level), money_text(species.sale_price), money_text(earned), money_text(balance))).await
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
    fn secret_case_price_choices_and_catalogue() {
        let defaults = PetSettings::load(&crate::config::Settings::default());
        assert_eq!(defaults.case_prices, [500, 2000, 7500, 25000]);
        let custom = PetSettings::load(&crate::config::Settings::parse("[pets]\nsecret_case_price = 30000\n"));
        assert_eq!(custom.case_prices[Case::Secret as usize], 30000);
        let command = open_case();
        let choices = &command.parameters[0].choices;
        assert_eq!(choices.iter().map(|choice| choice.name.as_ref()).collect::<Vec<_>>(),
                   ["📦 Дешёвый", "🎁 Средний", "💎 Дорогой", "🔮 Секретный"]);
        let catalogue = case_catalogue(defaults.case_prices);
        for text in ["Секретный", "Стеллочка", "Фоидка романа", "Даня собака", "0.05%", "25 000"] {
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
        assert_eq!(roll_pet(Case::Secret, 99.949, 0).rarity, "mythic");
        for pick in 0..25 { assert_eq!(roll_pet(Case::Secret, 99.95, pick).key, "stellochka"); }
        assert_eq!(roll_pet(Case::Secret, 99.99999, 3).key, "stellochka");
        let secret = RARITIES.iter().find(|rarity| rarity.key == "secret").unwrap();
        assert_eq!(secret.chances, [0.0, 0.0, 0.0, 0.05]);
        assert_eq!(SPECIES.iter().filter(|species| species.rarity == "secret").count(), 1);
        for case in [Case::Cheap, Case::Medium, Case::Expensive] {
            for step in 0..10000 { assert_ne!(roll_pet(case, step as f64 / 100.0, step).rarity, "secret"); }
        }
    }

    #[test]
    fn new_pet_economy_and_existing_keys_remain_compatible() {
        for (key, rarity, hourly) in [("stellochka", "secret", 1500), ("foidka_romana", "mythic", 550), ("danya_dog", "legendary", 170)] {
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
    fn sale_pays_fixed_species_price_and_unpaid_income_once() {
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
        assert_eq!(sell(&first), Some((20, 320)));
        assert_eq!(sell(&second), Some((24, 544)));
        assert_eq!(sell(&second), None);
        assert!(db.user_pets(7, 1).unwrap().is_empty());
        assert_eq!(db.wallet(7, 1, "Alice", 100).unwrap().0, 544);
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
    fn sale_prices_follow_income_and_keep_resale_below_case_cost_on_average() {
        let prices = SPECIES.iter().map(|species| species.sale_price).collect::<std::collections::HashSet<_>>();
        assert_eq!(prices.len(), SPECIES.len());
        assert!(prices.iter().all(|&price| price > 0));

        let mut expected = [0.0; Case::ALL.len()];
        for rarity in &RARITIES {
            let mut kinds: Vec<_> = SPECIES.iter().filter(|species| species.rarity == rarity.key).collect();
            kinds.sort_by_key(|species| (species.income, species.sale_price));
            assert!(kinds.windows(2).all(|pair| pair[0].sale_price < pair[1].sale_price), "{}", rarity.key);
            let average = kinds.iter().map(|species| species.sale_price).sum::<i64>() as f64 / kinds.len() as f64;
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
        assert_eq!(roll_pet(Case::Cheap, 99.995, 0).key, "sergey");
        assert_eq!(roll_pet(Case::Expensive, 0.0, 0).rarity, "rare"); // обычных в дорогом кейсе нет
        assert_eq!(roll_pet(Case::Expensive, 99.0, 5).rarity, "mythic");
        assert_eq!(roll_pet(Case::Medium, 50.0, 1).key, "tamara");
    }

    #[test]
    fn strongest_first() {
        let pet = |id, species: &str, level| Pet { id, species: species.to_string(), level, last_collect: 0.0, spent: 0 };
        let pets = sorted_pets(vec![pet(1, "valera", 1), pet(2, "sergey", 1), pet(3, "igor", 3), pet(4, "нет такого", 1), pet(5, "tolik", 2)]);
        let order: Vec<i64> = pets.iter().map(|(pet, _)| pet.id).collect();
        assert_eq!(order, [2, 3, 1, 5]); // у котика и голубя доход 10 — раньше тот, кто раньше появился
    }
}
