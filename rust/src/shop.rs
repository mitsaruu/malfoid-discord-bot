//! Магазин ролей: /shop, /shop_buy, для админов — /shop_add и /shop_remove.
//! Админ выставляет роль на продажу: цена и на сколько дней (0 — навсегда). Роль на время бот снимает сам.
//! Боту нужно право «Управлять ролями», и его роль должна быть выше продаваемых.

use crate::db::{self, Database, Wallet};
use crate::reply::whisper;
use crate::text::{money_text, plural};
use crate::{Context, Error, Shared};
use poise::serenity_prelude as serenity;
use rusqlite::{params, OptionalExtension};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Duration;

type Key = (serenity::GuildId, serenity::UserId, serenity::RoleId);

#[derive(Default)]
pub struct State {
    operations: [tokio::sync::Mutex<()>; 32],
}

impl State {
    fn operation(&self, key: Key) -> &tokio::sync::Mutex<()> {
        let mut hash = DefaultHasher::new();
        key.hash(&mut hash);
        &self.operations[hash.finish() as usize % self.operations.len()]
    }
}

#[derive(Clone, Copy)]
struct Purchase {
    price: i64,
    expires: Option<f64>,
}

pub fn prepare(database: &Database) -> rusqlite::Result<()> {
    database.lock().execute_batch(
        "CREATE TABLE IF NOT EXISTS shop_pending (
            guild_id INTEGER NOT NULL,
            user_id INTEGER NOT NULL,
            role_id INTEGER NOT NULL,
            price INTEGER NOT NULL CHECK(price > 0),
            expires REAL,
            retry_at REAL NOT NULL DEFAULT 0,
            PRIMARY KEY (guild_id, user_id, role_id)
         );
         CREATE INDEX IF NOT EXISTS shop_expiry ON shop_purchases(expires);",
    )
}

fn ids(key: Key) -> [i64; 3] {
    [key.0.get() as i64, key.1.get() as i64, key.2.get() as i64]
}

fn pending(database: &Database, key: Key) -> rusqlite::Result<Option<Purchase>> {
    database.lock().query_row(
        "SELECT price, expires FROM shop_pending WHERE guild_id=?1 AND user_id=?2 AND role_id=?3",
        ids(key),
        |row| Ok(Purchase { price: row.get(0)?, expires: row.get(1)? }),
    ).optional()
}

/// A successful Discord grant and the durable intent are reconciled together.
fn finish_purchase(database: &Database, key: Key, purchase: Purchase, granted: bool) -> rusqlite::Result<()> {
    let [guild, user, role] = ids(key);
    database.transaction(|tx| {
        let removed = tx.execute(
            "DELETE FROM shop_pending WHERE guild_id=?1 AND user_id=?2 AND role_id=?3", [guild, user, role],
        )?;
        if removed == 0 {
            return Ok(());
        }
        if !granted {
            db::change_balance(tx, guild as u64, user as u64, purchase.price)?;
        } else if let Some(expires) = purchase.expires {
            tx.execute(
                "INSERT INTO shop_purchases(guild_id,user_id,role_id,expires) VALUES(?1,?2,?3,?4)
                 ON CONFLICT(guild_id,user_id,role_id) DO UPDATE SET expires=excluded.expires",
                params![guild, user, role, expires],
            )?;
        } else {
            tx.execute("DELETE FROM shop_purchases WHERE guild_id=?1 AND user_id=?2 AND role_id=?3", [guild, user, role])?;
        }
        Ok(())
    })
}

pub fn days_text(days: i64) -> String {
    if days == 0 {
        "навсегда".to_string()
    } else {
        format!("на {days} {}", plural(days, "день", "дня", "дней"))
    }
}

/// До какого времени действует роль: если она уже куплена и срок не вышел — продлеваем от конца срока.
pub fn new_expiry(current: Option<f64>, now: f64, days: i64) -> f64 {
    current.map_or(now, |expires| expires.max(now)) + days as f64 * 86400.0
}

fn shop_admin_guild(ctx: Context<'_>) -> Option<serenity::GuildId> {
    let poise::Context::Application(app) = ctx else {
        return None;
    };
    let guild_id = app.interaction.guild_id?;
    // Discord присылает права вызвавшего команду участника, включая права владельца сервера.
    let permissions = app.interaction.member.as_ref()?.permissions?;
    permissions.administrator().then_some(guild_id)
}

fn sale_rejection(guild_id: serenity::GuildId, role: &serenity::Role) -> Option<&'static str> {
    if role.id.get() == guild_id.get() || role.managed {
        return Some("Эту роль продавать нельзя (она служебная).");
    }
    if role.permissions.intersects(privileged_permissions()) {
        return Some("Эту роль продавать нельзя: у неё права админа или модератора.");
    }
    None
}

fn privileged_permissions() -> serenity::Permissions {
    use serenity::Permissions as P;
    P::ADMINISTRATOR | P::MANAGE_GUILD | P::MANAGE_ROLES | P::MANAGE_CHANNELS
        | P::KICK_MEMBERS | P::BAN_MEMBERS | P::MODERATE_MEMBERS
        | P::MANAGE_MESSAGES | P::MANAGE_THREADS | P::MANAGE_WEBHOOKS
        | P::MUTE_MEMBERS | P::DEAFEN_MEMBERS | P::MOVE_MEMBERS
        | P::MANAGE_EVENTS | P::MENTION_EVERYONE | P::VIEW_AUDIT_LOG
}

fn unsafe_channel_grant(guild: &serenity::Guild, role: serenity::RoleId) -> bool {
    guild.channels.values().any(|channel| channel.permission_overwrites.iter().any(|overwrite| {
        matches!(overwrite.kind, serenity::PermissionOverwriteType::Role(id) if id == role)
            && overwrite.allow.intersects(privileged_permissions())
    }))
}

/// Может ли бот выдавать эту роль: есть право «Управлять ролями», и роль бота выше неё.
fn bot_can_manage(guild: &serenity::Guild, bot: serenity::UserId, role: &serenity::Role) -> bool {
    let Some(member) = guild.members.get(&bot) else { return false };
    let roles: Vec<&serenity::Role> = member.roles.iter().filter_map(|id| guild.roles.get(id)).collect();
    let everyone = guild.roles.get(&serenity::RoleId::new(guild.id.get()));
    let allowed = guild.owner_id == bot
        || roles.iter().chain(everyone.iter()).any(|role| role.permissions.manage_roles() || role.permissions.administrator());
    let top = roles.iter().map(|role| role.position).max().unwrap_or(0);
    allowed && role.position < top
}

/// Роли в магазине сервера: (роль, цена, дней), от дешёвых к дорогим. Удалённые роли пропускаем.
fn shop_items(ctx: Context<'_>) -> Result<Vec<(serenity::Role, i64, i64)>, Error> {
    let guild_id = ctx.guild_id().unwrap();
    let rows = ctx.data().db.shop_roles(guild_id.get())?;
    let Some(guild) = ctx.guild() else { return Ok(Vec::new()) };
    Ok(rows
        .into_iter()
        .filter_map(|(role, price, days)| guild.roles.get(&serenity::RoleId::new(role)).map(|role| (role.clone(), price, days)))
        .collect())
}

async fn shop_autocomplete(ctx: Context<'_>, partial: &str) -> serenity::CreateAutocompleteResponse {
    let partial = partial.to_lowercase();
    let choices = shop_items(ctx)
        .unwrap_or_default()
        .into_iter()
        .filter(|(role, _, _)| role.name.to_lowercase().contains(&partial))
        .take(25)
        .map(|(role, price, days)| {
            let label: String = format!("{} — {}, {}", role.name, money_text(price), days_text(days)).chars().take(100).collect();
            serenity::AutocompleteChoice::new(label, role.id.to_string())
        })
        .collect();
    serenity::CreateAutocompleteResponse::new().set_choices(choices)
}

/// Магазин ролей: что можно купить за монеты
#[poise::command(slash_command, guild_only)]
pub async fn shop(ctx: Context<'_>) -> Result<(), Error> {
    let items = shop_items(ctx)?;
    if items.is_empty() {
        return whisper(ctx, "🛒 В магазине пока пусто. Админ может выставить роль командой /shop_add.").await;
    }
    let lines: Vec<String> = items
        .iter()
        .map(|(role, price, days)| format!("<@&{}> — **{}**, {}", role.id, money_text(*price), days_text(*days)))
        .collect();
    let embed = serenity::CreateEmbed::new()
        .color(0x7C6CF2)
        .title("🛒 Магазин ролей")
        .description(lines.join("\n"))
        .footer(serenity::CreateEmbedFooter::new("Купить — /shop_buy"));
    ctx.send(poise::CreateReply::default().embed(embed).allowed_mentions(serenity::CreateAllowedMentions::new())).await?;
    Ok(())
}

/// Купить роль из магазина за монеты
#[poise::command(slash_command, guild_only)]
pub async fn shop_buy(
    ctx: Context<'_>,
    #[description = "Какую роль купить"]
    #[autocomplete = "shop_autocomplete"]
    role: String,
) -> Result<(), Error> {
    let (data, guild_id, user) = (ctx.data(), ctx.guild_id().unwrap(), ctx.author().id);
    let Some((role, price, days)) = shop_items(ctx)?.into_iter().find(|(item, _, _)| item.id.to_string() == role) else {
        return whisper(ctx, "Такой роли в магазине нет — выбери из списка.").await;
    };
    ctx.defer_ephemeral().await?;
    let key = (guild_id, user, role.id);
    let _operation = data.shop.operation(key).lock().await;
    if pending(&data.db, key)?.is_some() {
        return whisper(ctx, "Предыдущая покупка ещё проверяется. Повторное списание не требуется.").await;
    }
    let bought_until = data.db.shop_purchase(guild_id.get(), user.get(), role.id.get())?;
    let member = tokio::time::timeout(Duration::from_secs(12), ctx.http().get_member(guild_id, user)).await??;
    let has_role = member.roles.contains(&role.id);
    if has_role && (days == 0 || bought_until.is_none()) {
        return whisper(ctx, "Эта роль у тебя уже есть.").await;
    }
    // Права роли могли измениться после выставления на продажу, а кэш мог ещё не обновиться.
    let role = tokio::time::timeout(Duration::from_secs(12), ctx.http().get_guild_role(guild_id, role.id)).await??;
    if let Some(reason) = sale_rejection(guild_id, &role) {
        return whisper(ctx, reason).await;
    }
    let bot = ctx.framework().bot_id();
    if !ctx.guild().is_some_and(|guild| bot_can_manage(&guild, bot, &role) && !unsafe_channel_grant(&guild, role.id)) {
        return whisper(ctx,
            "Не могу выдать эту роль: мне нужно право «Управлять ролями», и моя роль должна быть выше неё. Скажи админу.").await;
    }
    let name = member.display_name();
    let [guild, user_id, role_id] = ids(key);
    let wallet = Wallet { guild: guild_id.get(), user: user.get(), name, start: data.economy.start_balance };
    let purchase = Purchase { price, expires: (days > 0).then(|| new_expiry(bought_until, db::now(), days)) };
    let reserved = data.db.transaction(|tx| {
        let listed = tx.query_row(
            "SELECT price, days FROM shop_roles WHERE guild_id=?1 AND role_id=?2", [guild, role_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        ).optional()?;
        if listed != Some((price, days)) || price <= 0 {
            return Ok(Err("Условия покупки изменились. Выбери роль ещё раз.".into()));
        }
        let (balance, _) = wallet.open(tx)?;
        if balance < price {
            return Ok(Err(format!("Для покупки нужно {}, у тебя {}.", money_text(price), money_text(balance))));
        }
        db::change_balance(tx, wallet.guild, wallet.user, -price)?;
        tx.execute(
            "INSERT INTO shop_pending(guild_id,user_id,role_id,price,expires) VALUES(?1,?2,?3,?4,?5)",
            params![guild, user_id, role_id, purchase.price, purchase.expires],
        )?;
        Ok(Ok(()))
    })?;
    if let Err(message) = reserved {
        return whisper(ctx, message).await;
    }
    if !has_role {
        match grant_role(ctx.http(), key).await {
            Some(true) => {},
            Some(false) => {
                finish_purchase(&data.db, key, purchase, false)?;
                return whisper(ctx, "Discord отказал в выдаче роли. Монеты возвращены.").await;
            },
            None => return whisper(ctx, "Discord пока не подтвердил выдачу. Бот повторно проверит заказ.").await,
        }
    }
    finish_purchase(&data.db, key, purchase, true)?;
    let mut text = format!("🛒 <@{user}> покупает роль <@&{}> за **{}**", role.id, money_text(price));
    if let Some(expires) = purchase.expires {
        text += &format!(" — до <t:{}:f>", expires as i64);
    }
    whisper(ctx, text + ".").await
}

/// Выставить роль на продажу в магазине (или поменять цену)
#[poise::command(slash_command, guild_only, default_member_permissions = "ADMINISTRATOR")]
pub async fn shop_add(
    ctx: Context<'_>,
    #[description = "Какую роль продавать"] role: serenity::Role,
    #[description = "Цена в монетах"]
    #[min = 1]
    #[max = 100_000_000]
    price: i64,
    #[description = "На сколько дней даётся роль (0 — навсегда)"]
    #[min = 0]
    #[max = 365]
    days: Option<i64>,
) -> Result<(), Error> {
    let Some(guild_id) = shop_admin_guild(ctx) else {
        return whisper(ctx, "Управлять магазином могут только администраторы сервера.").await;
    };
    let days = days.unwrap_or(0);
    if let Some(reason) = sale_rejection(guild_id, &role) {
        return whisper(ctx, reason).await;
    }
    let bot = ctx.framework().bot_id();
    if !ctx.guild().is_some_and(|guild| bot_can_manage(&guild, bot, &role) && !unsafe_channel_grant(&guild, role.id)) {
        return whisper(ctx,
            "Я не смогу выдавать эту роль: мне нужно право «Управлять ролями», и моя роль должна быть выше неё.").await;
    }
    ctx.data().db.set_shop_role(guild_id.get(), role.id.get(), price, days)?;
    whisper(ctx, format!("✅ <@&{}> в магазине: {}, {}.", role.id, money_text(price), days_text(days))).await
}

/// Убрать роль из магазина
#[poise::command(slash_command, guild_only, default_member_permissions = "ADMINISTRATOR")]
pub async fn shop_remove(ctx: Context<'_>, #[description = "Какую роль убрать"] role: serenity::Role) -> Result<(), Error> {
    let Some(guild_id) = shop_admin_guild(ctx) else {
        return whisper(ctx, "Управлять магазином могут только администраторы сервера.").await;
    };
    let removed = ctx.data().db.remove_shop_role(guild_id.get(), role.id.get())?;
    whisper(ctx, if removed {
        format!("✅ <@&{}> убрана из магазина. У кого она уже куплена — остаётся.", role.id)
    } else {
        "Этой роли и так нет в магазине.".to_string()
    }).await
}

fn discord_status(error: &serenity::Error) -> Option<u16> {
    match error {
        serenity::Error::Http(serenity::HttpError::UnsuccessfulRequest(response)) => Some(response.status_code.as_u16()),
        _ => None,
    }
}

async fn grant_role(http: &serenity::Http, key: Key) -> Option<bool> {
    match tokio::time::timeout(Duration::from_secs(12),
        http.add_member_role(key.0, key.1, key.2, Some("Покупка в магазине ролей"))).await
    {
        Ok(Ok(())) => Some(true),
        Ok(Err(error)) if matches!(discord_status(&error), Some(400 | 403 | 404)) => Some(false),
        _ => None,
    }
}

async fn reconcile(ctx: &serenity::Context, data: &crate::Data, key: Key) -> Result<(), Error> {
    let _operation = data.shop.operation(key).lock().await;
    let Some(purchase) = pending(&data.db, key)? else { return Ok(()) };
    let http = &ctx.http;
    let member = match tokio::time::timeout(Duration::from_secs(12), http.get_member(key.0, key.1)).await? {
        Ok(member) => member,
        Err(error) if discord_status(&error) == Some(404) => {
            finish_purchase(&data.db, key, purchase, false)?;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    if member.roles.contains(&key.2) {
        finish_purchase(&data.db, key, purchase, true)?;
        return Ok(());
    }
    let role = match tokio::time::timeout(Duration::from_secs(12), http.get_guild_role(key.0, key.2)).await? {
        Ok(role) => role,
        Err(error) if discord_status(&error) == Some(404) => {
            finish_purchase(&data.db, key, purchase, false)?;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    if sale_rejection(key.0, &role).is_some()
        || purchase.expires.is_some_and(|expires| expires <= db::now())
    {
        finish_purchase(&data.db, key, purchase, false)?;
        return Ok(());
    }
    let may_grant = ctx.cache.guild(key.0).map(|guild| {
        bot_can_manage(&guild, ctx.cache.current_user().id, &role) && !unsafe_channel_grant(&guild, role.id)
    });
    if may_grant == Some(false) {
        finish_purchase(&data.db, key, purchase, false)?;
        return Ok(());
    }
    if may_grant.is_none() {
        return Ok(());
    }
    if let Some(granted) = grant_role(http, key).await {
        finish_purchase(&data.db, key, purchase, granted)?;
    }
    Ok(())
}

fn pending_batch(database: &Database) -> rusqlite::Result<Vec<(u64, u64, u64)>> {
    let connection = database.lock();
    let mut query = connection.prepare(
        "SELECT guild_id,user_id,role_id FROM shop_pending WHERE retry_at<=?1 ORDER BY retry_at,rowid LIMIT 50",
    )?;
    let rows = query.query_map([db::now()], |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64, row.get::<_, i64>(2)? as u64)))?;
    rows.collect()
}

fn key_from_ids(ids: (u64, u64, u64)) -> Option<Key> {
    let (guild, user, role) = ids;
    (guild != 0 && user != 0 && role != 0)
        .then(|| (serenity::GuildId::new(guild), serenity::UserId::new(user), serenity::RoleId::new(role)))
}

/// Снимает роли, срок которых вышел, и восстанавливает незавершённые покупки.
pub async fn expire_roles(ctx: &serenity::Context, data: &crate::Data) -> Result<(), Error> {
    let database = &data.db;
    for key in pending_batch(database)?.into_iter().filter_map(key_from_ids) {
        if let Err(error) = reconcile(ctx, data, key).await {
            eprintln!("Магазин ролей: покупка {key:?} ожидает сверки: {error}");
        }
        let [guild, user, role] = ids(key);
        database.lock().execute(
            "UPDATE shop_pending SET retry_at=?4 WHERE guild_id=?1 AND user_id=?2 AND role_id=?3",
            params![guild, user, role, db::now() + 90.0],
        )?;
    }
    let expired = match database.expired_shop_purchases(db::now()) {
        Ok(expired) => expired,
        Err(error) => return Err(error.into()),
    };
    for (guild, user, role) in expired.into_iter().take(50) {
        let Some(key) = key_from_ids((guild, user, role)) else { continue };
        let _operation = data.shop.operation(key).lock().await;
        if pending(database, key)?.is_some()
            || !database.shop_purchase(guild, user, role)?.is_some_and(|expires| expires <= db::now())
        {
            continue;
        }
        let result = tokio::time::timeout(Duration::from_secs(12),
            ctx.http.remove_member_role(key.0, key.1, key.2, Some("Закончился срок роли из магазина"))).await;
        match result {
            Ok(Ok(())) => {}
            // 404 — человек ушёл с сервера или роль удалили: снимать нечего, запись убираем
            Ok(Err(error)) if discord_status(&error) == Some(404) => {}
            Ok(Err(error)) => {
                eprintln!("Магазин ролей: не получилось снять роль: {error}");
                continue;
            }
            Err(error) => {
                eprintln!("Магазин ролей: Discord не подтвердил снятие роли: {error}");
                continue;
            }
        }
        database.remove_shop_purchase(guild, user, role)?;
    }
    Ok(())
}

/// Запускает проверку сроков ролей в фоне.
pub fn start_expiry_loop(ctx: serenity::Context, data: Shared) {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(60));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            if let Err(error) = expire_roles(&ctx, &data).await {
                eprintln!("Магазин ролей: {error}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pending_role_refunds_once_and_a_permanent_grant_clears_the_old_expiry() {
        let db = Database::in_memory().unwrap();
        let key = (serenity::GuildId::new(1), serenity::UserId::new(2), serenity::RoleId::new(3));
        db.wallet(1, 2, "Buyer", 300).unwrap();
        db.set_shop_purchase(1, 2, 3, 100.0).unwrap();
        let reserve = || db.transaction(|tx| {
            db::change_balance(tx, 1, 2, -50)?;
            tx.execute("INSERT INTO shop_pending(guild_id,user_id,role_id,price) VALUES(1,2,3,50)", [])?;
            Ok(())
        }).unwrap();
        reserve();
        let purchase = pending(&db, key).unwrap().unwrap();
        finish_purchase(&db, key, purchase, false).unwrap();
        finish_purchase(&db, key, purchase, false).unwrap();
        assert_eq!(db.wallet(1, 2, "Buyer", 300).unwrap().0, 300);
        assert_eq!(db.shop_purchase(1, 2, 3).unwrap(), Some(100.0));
        reserve();
        finish_purchase(&db, key, purchase, true).unwrap();
        assert_eq!(db.shop_purchase(1, 2, 3).unwrap(), None);
        assert_eq!(db.wallet(1, 2, "Buyer", 300).unwrap().0, 250);
    }

    #[test]
    fn shop_command_registration() {
        for command in [shop_add(), shop_remove()] {
            assert_eq!(command.default_member_permissions, serenity::Permissions::ADMINISTRATOR);
        }
        for command in [shop(), shop_buy()] {
            assert!(command.default_member_permissions.is_empty());
            assert!(command.required_permissions.is_empty());
        }
    }

    #[test]
    fn sale_filter_rechecks_role_permissions() {
        use serenity::Permissions;

        let guild_id = serenity::GuildId::new(1);
        let mut role = serenity::Role::default();
        role.id = serenity::RoleId::new(2);
        role.permissions = Permissions::VIEW_CHANNEL;
        assert!(sale_rejection(guild_id, &role).is_none());

        for permission in [
            Permissions::ADMINISTRATOR,
            Permissions::MANAGE_GUILD,
            Permissions::MODERATE_MEMBERS,
            Permissions::MANAGE_ROLES,
            Permissions::BAN_MEMBERS,
            Permissions::MANAGE_CHANNELS,
        ] {
            role.permissions = Permissions::VIEW_CHANNEL | permission;
            assert!(sale_rejection(guild_id, &role).is_some());
        }

        role.permissions = Permissions::empty();
        role.managed = true;
        assert!(sale_rejection(guild_id, &role).is_some());

        role.managed = false;
        role.id = serenity::RoleId::new(guild_id.get());
        assert!(sale_rejection(guild_id, &role).is_some());
    }

    #[test]
    fn texts_and_expiry() {
        assert_eq!([days_text(0), days_text(1), days_text(3), days_text(7), days_text(21)],
                   ["навсегда", "на 1 день", "на 3 дня", "на 7 дней", "на 21 день"]);
        assert_eq!(new_expiry(None, 1000.0, 1), 1000.0 + 86400.0);
        assert_eq!(new_expiry(Some(5000.0), 1000.0, 1), 5000.0 + 86400.0); // ещё действует — продлеваем от конца
        assert_eq!(new_expiry(Some(500.0), 1000.0, 2), 1000.0 + 2.0 * 86400.0); // срок давно вышел — от сейчас
    }
}
