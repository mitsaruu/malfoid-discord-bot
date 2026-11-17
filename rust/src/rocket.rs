//! Личная ракетка. Сообщение показывает снимок; время и выплата считаются на сервере.

use crate::reply::{user_name, whisper};
use crate::text::money_text;
use crate::{wagers, Context, Data, Error};
use poise::serenity_prelude as s;
use rand::Rng;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::{sync::Notify, time::Instant};

const COUNTDOWN: Duration = Duration::from_secs(3);
const REFRESH: Duration = Duration::from_secs(2);
const MAX: u32 = 1000;
const DOUBLE_SECONDS: f64 = 6.0;
const BUTTON: &str = "rocket:cash:";

#[derive(Clone, Copy)]
enum FlightStatus {
    Countdown,
    Flying(u32),
    Crashed,
    Paid { multiplier: u32, payout: i64 },
}

struct Flight {
    id: u64,
    channel: s::ChannelId,
    stake: i64,
    crash_at: u32,
    launch: Option<Instant>,
    finished: Option<FlightStatus>,
    changed: Arc<Notify>,
}

type Player = (u64, u64); // сервер, игрок

#[derive(Default)]
pub struct State(Mutex<HashMap<Player, Flight>>);

impl State {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Player, Flight>> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }
}

fn crash_point(random: u32) -> u32 {
    // 3% преимущества игры до округления. random равномерно распределён от 1 до 1 000 000.
    (97_000_000 / random).clamp(100, MAX)
}

fn flight_time(multiplier: u32) -> Duration {
    Duration::from_secs_f64((f64::from(multiplier) / 100.0).log2() * DOUBLE_SECONDS)
}

impl Flight {
    fn status(&self, now: Instant) -> FlightStatus {
        if let Some(status) = self.finished {
            return status;
        }
        let Some(launch) = self.launch.filter(|&launch| launch <= now) else {
            return FlightStatus::Countdown;
        };
        let elapsed = now - launch;
        if elapsed >= flight_time(self.crash_at) {
            return FlightStatus::Crashed;
        }
        let multiplier = (100.0 * (elapsed.as_secs_f64() / DOUBLE_SECONDS).exp2()) as u32;
        FlightStatus::Flying(multiplier.min(self.crash_at - 1))
    }

    fn next_update(&self, now: Instant) -> Instant {
        let launch = self.launch.expect("flight starts after the opening reply");
        if now < launch {
            launch
        } else {
            (now + REFRESH).min(launch + flight_time(self.crash_at))
        }
    }
}

struct RefundOnDrop<'a> {
    data: &'a Data,
    player: Player,
    round: u64,
}

impl Drop for RefundOnDrop<'_> {
    fn drop(&mut self) {
        let mut flights = self.data.rocket.lock();
        if flights
            .get(&self.player)
            .is_some_and(|flight| flight.id == self.round)
        {
            flights.remove(&self.player);
            if let Err(error) = wagers::refund(&self.data.db, Some(self.round)) {
                eprintln!(
                    "Ракетка {}: возврат не удался, ставка сохранена до восстановления: {error}",
                    self.round
                );
            }
        }
    }
}

fn factor(multiplier: u32) -> String {
    format!("{}.{:02}×", multiplier / 100, multiplier % 100)
}

fn card(
    owner: u64,
    id: u64,
    stake: i64,
    status: FlightStatus,
    crash_at: u32,
) -> poise::CreateReply {
    let (title, text, color, active) = match status {
        FlightStatus::Countdown => (
            "🚀 Ракетка",
            "Старт через 3 секунды.".into(),
            0x6366F1,
            true,
        ),
        FlightStatus::Flying(multiplier) => (
            "🚀 Ракетка в полёте",
            format!("Коэффициент **{}**", factor(multiplier)),
            0x6366F1,
            true,
        ),
        FlightStatus::Crashed => (
            "💥 Полёт завершён",
            format!(
                "Ракета взорвалась на **{}**. Ставка проиграна.",
                factor(crash_at)
            ),
            0xDC2626,
            false,
        ),
        FlightStatus::Paid { multiplier, payout } => (
            "🟢 Монеты выведены",
            format!(
                "Вывод на **{}**. Получено **{}**, чистый выигрыш **{}**.",
                factor(multiplier),
                money_text(payout),
                money_text(payout - stake)
            ),
            0x16A34A,
            false,
        ),
    };
    let embed = s::CreateEmbed::new()
        .title(title)
        .color(color)
        .description(format!(
            "<@{owner}> · Ставка **{}**\n{text}",
            money_text(stake)
        ))
        .footer(s::CreateEmbedFooter::new(
            "Вывод считается в момент нажатия. Максимум полёта — 10×.",
        ));
    let buttons = if active {
        vec![s::CreateActionRow::Buttons(vec![s::CreateButton::new(
            format!("{BUTTON}{owner}:{id}"),
        )
        .style(s::ButtonStyle::Success)
        .label("Забрать")])]
    } else {
        Vec::new()
    };
    poise::CreateReply::default()
        .embed(embed)
        .components(buttons)
        .allowed_mentions(s::CreateAllowedMentions::new())
}

/// Поставь монеты и успей забрать их до взрыва ракеты
#[poise::command(slash_command, guild_only)]
pub async fn rocket(
    ctx: Context<'_>,
    #[description = "Сумма монет или all"] bet: String,
) -> Result<(), Error> {
    let data = ctx.data();
    let player = (ctx.guild_id().unwrap().get(), ctx.author().id.get());
    let id = ctx.id();
    let crash_at = crash_point(rand::rng().random_range(1..=1_000_000));
    let changed = Arc::new(Notify::new());
    data.db.wallet(
        player.0,
        player.1,
        &user_name(ctx.author()),
        data.economy.start_balance,
    )?;
    let placed = {
        let mut flights = data.rocket.lock();
        if flights.contains_key(&player) {
            Err("Твоя ракета уже в игре.".into())
        } else {
            wagers::place(
                &data.db,
                id,
                id,
                player.0,
                player.1,
                &bet,
                data.economy.min_bet,
            )?
            .map(|stake| {
                flights.insert(
                    player,
                    Flight {
                        id,
                        channel: ctx.channel_id(),
                        stake,
                        crash_at,
                        launch: None,
                        finished: None,
                        changed: changed.clone(),
                    },
                );
                stake
            })
        }
    };
    let stake = match placed {
        Ok(stake) => stake,
        Err(error) => return whisper(ctx, error).await,
    };
    let _refund = RefundOnDrop {
        data,
        player,
        round: id,
    };
    let reply = ctx
        .send(card(player.1, id, stake, FlightStatus::Countdown, crash_at))
        .await?;
    let mut next = Instant::now() + COUNTDOWN;
    if let Some(flight) = data.rocket.lock().get_mut(&player) {
        flight.launch = Some(next);
    }
    loop {
        tokio::select! { _ = tokio::time::sleep_until(next) => {}, _ = changed.notified() => {} }
        let (status, terminal) = {
            let mut flights = data.rocket.lock();
            let Some(flight) = flights.get_mut(&player) else {
                return Ok(());
            };
            let now = Instant::now();
            let status = flight.status(now);
            let terminal = matches!(status, FlightStatus::Crashed | FlightStatus::Paid { .. });
            if matches!(status, FlightStatus::Crashed) {
                wagers::settle(&data.db, id, &[(id, 0)])?;
                flight.finished = Some(status);
            }
            if !terminal {
                next = flight.next_update(now);
            }
            (status, terminal)
        };
        if let Err(error) = reply
            .edit(ctx, card(player.1, id, stake, status, crash_at))
            .await
        {
            eprintln!(
                "Ракетка {id}: не удалось обновить карточку: {}",
                crate::interactions::sanitize_error(&error.to_string())
            );
        }
        if terminal {
            return Ok(());
        }
    }
}

pub async fn pressed(ctx: &s::Context, data: &Data, press: &s::ComponentInteraction) -> bool {
    let Some(parameters) = press.data.custom_id.strip_prefix(BUTTON) else {
        return false;
    };
    let Some((owner, id)) = parameters
        .split_once(':')
        .and_then(|(owner, id)| Some((owner.parse::<u64>().ok()?, id.parse::<u64>().ok()?)))
    else {
        return false;
    };
    let text = if press.user.id.get() != owner {
        "Забрать монеты может только игрок, запустивший ракету.".into()
    } else if let Some(guild) = press.guild_id {
        let mut flights = data.rocket.lock();
        let flight = flights
            .get_mut(&(guild.get(), owner))
            .filter(|flight| flight.id == id && flight.channel == press.channel_id);
        match flight {
            Some(flight) => cash_out(data, flight, Instant::now()),
            None => {
                "Этот раунд уже закрыт. Если бот перезапускался, незавершённая ставка возвращена."
                    .into()
            }
        }
    } else {
        "Этот раунд уже закрыт.".into()
    };
    let answer = s::CreateInteractionResponse::Message(
        s::CreateInteractionResponseMessage::new()
            .content(text)
            .ephemeral(true),
    );
    if let Err(error) = press.create_response(ctx, answer).await {
        eprintln!(
            "Ракетка {id}: не удалось подтвердить нажатие: {}",
            crate::interactions::sanitize_error(&error.to_string())
        );
    }
    true
}

fn cash_out(data: &Data, flight: &mut Flight, now: Instant) -> String {
    let multiplier = match flight.status(now) {
        FlightStatus::Countdown => return "Ракета ещё не стартовала.".into(),
        FlightStatus::Crashed => return "Ракета уже взорвалась.".into(),
        FlightStatus::Paid { .. } => return "Монеты уже выведены.".into(),
        FlightStatus::Flying(multiplier) => multiplier,
    };
    let payout = flight.stake * i64::from(multiplier) / 100;
    match wagers::settle(&data.db, flight.id, &[(flight.id, payout)]) {
        Ok(()) => {
            flight.finished = Some(FlightStatus::Paid { multiplier, payout });
            flight.changed.notify_one();
            format!(
                "Забрал {} на **{}**.",
                money_text(payout),
                factor(multiplier)
            )
        }
        Err(error) => {
            eprintln!(
                "Ракетка {}: не удалось зачислить выигрыш: {error}",
                flight.id
            );
            "Не удалось вывести монеты. Попробуй ещё раз, если ракета в полёте.".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_clock_blocks_cashout_at_crash_even_before_card_refresh() {
        let launch = Instant::now() + COUNTDOWN;
        let flight = Flight {
            id: 1,
            channel: s::ChannelId::new(1),
            stake: 100,
            crash_at: 200,
            launch: Some(launch),
            finished: None,
            changed: Arc::new(Notify::new()),
        };
        assert!(matches!(
            flight.status(launch - Duration::from_millis(1)),
            FlightStatus::Countdown
        ));
        assert!(matches!(flight.status(launch), FlightStatus::Flying(100)));
        assert!(matches!(
            flight.status(launch + Duration::from_millis(5999)),
            FlightStatus::Flying(199)
        ));
        assert!(matches!(
            flight.status(launch + Duration::from_secs(6)),
            FlightStatus::Crashed
        ));
        assert_eq!(crash_point(1), MAX);
        assert_eq!(crash_point(1_000_000), 100);
    }
}
