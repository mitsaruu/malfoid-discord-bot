//! Европейская рулетка с одним общим раундом на канал.

use crate::reply::{user_name, whisper};
use crate::text::money_text;
use crate::{wagers, Context, Error, Shared};
use poise::serenity_prelude as s;
use rand::Rng;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::Instant;

const WAIT: Duration = Duration::from_secs(15);
const MAX_ROUND: Duration = Duration::from_secs(60);
const MAX_BETS: usize = 50;
const HELP_BUTTON: &str = "roulette:help";
const RED: [u8; 18] = [
    1, 3, 5, 7, 9, 12, 14, 16, 18, 19, 21, 23, 25, 27, 30, 32, 34, 36,
];
const GROUPS: &[(&str, &str, Space)] = &[
    ("red", "Красное", Space::Color(true)),
    ("black", "Чёрное", Space::Color(false)),
    ("even", "Чётное", Space::Parity(true)),
    ("odd", "Нечётное", Space::Parity(false)),
    ("1-18", "1–18", Space::Range(1, 18)),
    ("19-36", "19–36", Space::Range(19, 36)),
    ("1st", "1-я колонка", Space::Column(1)),
    ("2nd", "2-я колонка", Space::Column(2)),
    ("3rd", "3-я колонка", Space::Column(3)),
    ("1-12", "1–12", Space::Range(1, 12)),
    ("13-24", "13–24", Space::Range(13, 24)),
    ("25-36", "25–36", Space::Range(25, 36)),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Space {
    Color(bool),
    Parity(bool),
    Range(u8, u8),
    Column(u8),
    Number(u8),
}

impl Space {
    fn parse(text: &str) -> Option<(Self, String)> {
        let text = text.trim().to_lowercase().replace('–', "-");
        if let Some(&(_, label, space)) = GROUPS
            .iter()
            .find(|(key, label, _)| *key == text || label.to_lowercase() == text)
        {
            return Some((space, label.into()));
        }
        let number = text.parse::<u8>().ok()?;
        (number <= 36 && number.to_string() == text).then_some((Self::Number(number), text))
    }

    /// Множитель полной выплаты, включая поставленные монеты.
    fn multiplier(self, number: u8) -> i64 {
        if let Self::Number(wanted) = self {
            return if number == wanted { 36 } else { 0 };
        }
        if number == 0 {
            return 0;
        }
        match self {
            Self::Color(red) if RED.contains(&number) == red => 2,
            Self::Parity(even) if (number % 2 == 0) == even => 2,
            Self::Range(first, last) if (first..=last).contains(&number) => {
                if last - first == 11 {
                    3
                } else {
                    2
                }
            }
            Self::Column(column) if (number - 1) % 3 + 1 == column => 3,
            _ => 0,
        }
    }
}

struct Bet {
    ticket: u64,
    user: u64,
    amount: i64,
    space: Space,
}

struct Round {
    id: u64,
    started: Instant,
    deadline: Instant,
    bets: Vec<Bet>,
}

#[derive(Default)]
pub struct State(Mutex<HashMap<s::ChannelId, Round>>);

async fn spaces(_: Context<'_>, partial: &str) -> s::CreateAutocompleteResponse {
    let partial = partial.trim().to_lowercase();
    let groups = GROUPS
        .iter()
        .map(|(key, label, _)| (key.to_string(), label.to_string()));
    let numbers = (0..=36).map(|number| (number.to_string(), number.to_string()));
    let mut matches: Vec<_> = groups
        .chain(numbers)
        .filter(|(key, label)| {
            key.starts_with(&partial) || label.to_lowercase().starts_with(&partial)
        })
        .collect();
    // Discord даёт 25 подсказок; при вводе числа доступны все клетки 0–36.
    matches.sort_by_key(|(key, _)| key != &partial);
    s::CreateAutocompleteResponse::new().set_choices(
        matches
            .into_iter()
            .take(25)
            .map(|(key, label)| s::AutocompleteChoice::new(label, key))
            .collect(),
    )
}

/// Ставка в общем раунде рулетки. Выбери цвет, группу или число от 0 до 36
#[poise::command(slash_command, guild_only)]
pub async fn roulette(
    ctx: Context<'_>,
    #[description = "Сумма монет или all"] bet: String,
    #[description = "Цвет, группа, колонка или число 0–36"]
    #[autocomplete = "spaces"]
    space: String,
) -> Result<(), Error> {
    let Some((space, label)) = Space::parse(&space) else {
        return whisper(
            ctx,
            "Выбери ставку из подсказок или введи число от 0 до 36.",
        )
        .await;
    };
    let data = ctx.data();
    let guild = ctx.guild_id().unwrap().get();
    let user = ctx.author().id.get();
    let channel = ctx.channel_id();
    let ticket = ctx.id();
    data.db.wallet(
        guild,
        user,
        &user_name(ctx.author()),
        data.economy.start_balance,
    )?;
    let placed = {
        let mut rounds = data
            .roulette
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        match rounds.get(&channel) {
            Some(round) if round.deadline <= now => {
                Err("Раунд уже закрыт, подожди результата.".to_string())
            }
            Some(round) if round.bets.len() >= MAX_BETS => {
                Err("В этом раунде уже 50 ставок.".to_string())
            }
            active => {
                let id = active.map_or(ticket, |round| round.id);
                wagers::place(
                    &data.db,
                    id,
                    ticket,
                    guild,
                    user,
                    &bet,
                    data.economy.min_bet,
                )?
                .map(|amount| {
                    let round = rounds.entry(channel).or_insert_with(|| Round {
                        id,
                        started: now,
                        deadline: now + WAIT,
                        bets: Vec::new(),
                    });
                    round.bets.push(Bet {
                        ticket,
                        user,
                        amount,
                        space,
                    });
                    round.deadline = (now + WAIT).min(round.started + MAX_ROUND);
                    (
                        round.id,
                        amount,
                        round.deadline.saturating_duration_since(now).as_secs(),
                    )
                })
            }
        }
    };
    let (id, amount, remaining) = match placed {
        Ok(placed) => placed,
        Err(error) => return whisper(ctx, error).await,
    };
    let first = ticket == id;
    if first {
        tokio::spawn(finish_round(
            ctx.serenity_context().http.clone(),
            data.clone(),
            channel,
            id,
        ));
    }
    let title = if first {
        "🎡 Рулетка открыта"
    } else {
        "🎡 Ставка принята"
    };
    let details = format!("<@{user}> поставил **{}** на **{label}**.\nОсталось **{remaining} сек.** на момент ставки.", money_text(amount));
    let embed = s::CreateEmbed::new()
        .title(title)
        .description(details)
        .color(0x6366F1)
        .footer(s::CreateEmbedFooter::new(
            "/roulette — присоединиться · 15 сек. после последней ставки, максимум минута",
        ));
    let mut reply = poise::CreateReply::default()
        .embed(embed)
        .ephemeral(!first)
        .allowed_mentions(s::CreateAllowedMentions::new());
    if first {
        reply = reply.components(vec![s::CreateActionRow::Buttons(vec![
            s::CreateButton::new(HELP_BUTTON)
                .label("Правила")
                .style(s::ButtonStyle::Secondary),
        ])]);
    }
    ctx.send(reply).await?;
    Ok(())
}

pub async fn pressed(ctx: &s::Context, press: &s::ComponentInteraction) -> Result<bool, s::Error> {
    if press.data.custom_id != HELP_BUTTON {
        return Ok(false);
    }
    let text = "Ставь через `/roulette bet:100 space:red` или используй `all`.\n\n\
        **×2** — цвет, чётность, 1–18 или 19–36.\n\
        **×3** — дюжина или колонка.\n\
        **×36** — точное число от 0 до 36. Зеро выигрывает только по ставке на 0.\n\n\
        Выплаты включают ставку. Каждый принятый взнос оставляет до 15 секунд для новых ставок. Весь раунд длится не больше минуты.";
    press
        .create_response(
            ctx,
            s::CreateInteractionResponse::Message(
                s::CreateInteractionResponseMessage::new()
                    .content(text)
                    .ephemeral(true),
            ),
        )
        .await?;
    Ok(true)
}

async fn finish_round(http: std::sync::Arc<s::Http>, data: Shared, channel: s::ChannelId, id: u64) {
    let number = rand::rng().random_range(0..=36);
    let mut retry = false;
    let embed = loop {
        if retry {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let deadline = {
            let rounds = data
                .roulette
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let Some(round) = rounds.get(&channel).filter(|round| round.id == id) else {
                return;
            };
            round.deadline
        };
        tokio::time::sleep_until(deadline).await;
        let mut rounds = data
            .roulette
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(round) = rounds.get(&channel).filter(|round| round.id == id) else {
            return;
        };
        if Instant::now() < round.deadline {
            continue;
        }
        let payouts: Vec<_> = round
            .bets
            .iter()
            .map(|bet| (bet.ticket, bet.amount * bet.space.multiplier(number)))
            .collect();
        let embed = match wagers::settle(&data.db, id, &payouts) {
            Ok(()) => result(number, round, &payouts),
            Err(error) => {
                eprintln!("Рулетка {id}: расчёт ставок не удался: {error}");
                match wagers::refund(&data.db, Some(id)) {
                    Ok(_) => s::CreateEmbed::new()
                        .title("Раунд отменён")
                        .description("Ставки возвращены."),
                    Err(error) => {
                        eprintln!("Рулетка {id}: возврат ставок не удался: {error}");
                        retry = true;
                        continue;
                    }
                }
            }
        };
        rounds.remove(&channel);
        break embed;
    };
    let message = s::CreateMessage::new()
        .embed(embed)
        .allowed_mentions(s::CreateAllowedMentions::new());
    if let Err(error) = channel.send_message(&http, message).await {
        eprintln!("Рулетка {id}: не удалось отправить итог: {error}");
    }
}

fn result(number: u8, round: &Round, payouts: &[(u64, i64)]) -> s::CreateEmbed {
    let (icon, color, label) = match number {
        0 => ("🟢", 0x16A34A, "зелёное"),
        number if RED.contains(&number) => ("🔴", 0xDC2626, "красное"),
        _ => ("⚫", 0x52525B, "чёрное"),
    };
    let mut nets: BTreeMap<u64, i64> = BTreeMap::new();
    for (bet, &(_, payout)) in round.bets.iter().zip(payouts) {
        *nets.entry(bet.user).or_default() += payout - bet.amount;
    }
    let mut nets: Vec<_> = nets.into_iter().collect();
    nets.sort_by_key(|&(user, net)| (std::cmp::Reverse(net), user));
    let lines = nets
        .into_iter()
        .map(|(user, net)| {
            let sign = if net > 0 { "+" } else { "" };
            format!("<@{user}> · **{sign}{}**", money_text(net))
        })
        .collect::<Vec<_>>()
        .join("\n");
    s::CreateEmbed::new()
        .title(format!("{icon} {number} · {label}"))
        .color(color)
        .description(format!("**Итоги рулетки**\n{lines}"))
        .footer(s::CreateEmbedFooter::new(
            "В итогах показано изменение баланса с учётом ставок.",
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn european_wheel_payouts_and_zero() {
        for &(key, _, space) in GROUPS {
            assert_eq!(space.multiplier(0), 0, "{key}");
            let payouts: i64 = (0..=36).map(|number| space.multiplier(number)).sum();
            assert_eq!(payouts, 36, "{key}");
        }
        assert_eq!(Space::parse("0").unwrap().0.multiplier(0), 36);
        assert_eq!(Space::parse("black").unwrap().0.multiplier(22), 2);
        assert_eq!(Space::parse("1–18").unwrap().0.multiplier(22), 0);
        assert!(Space::parse("00").is_none());
        assert!(Space::parse("37").is_none());
    }
}
