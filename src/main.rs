use std::env;

use rand::seq::SliceRandom;
use rand::{thread_rng, Rng};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use teloxide::prelude::*;
use teloxide::types::{
    ChatId, DiceEmoji, InlineKeyboardButton, InlineKeyboardMarkup, MaybeInaccessibleMessage,
    MessageId, ParseMode,
};

// ---------- Card model ----------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Card {
    suit: &'static str,
    label: &'static str,
    weight: i32,
}

const SUITS: [&str; 4] = ["♠️", "♥️", "♦️", "♣️"];
const RANKS: [(&str, i32); 13] = [
    ("2", 2), ("3", 3), ("4", 4), ("5", 5), ("6", 6), ("7", 7),
    ("8", 8), ("9", 9), ("10", 10), ("J", 10), ("Q", 10), ("K", 10), ("A", 11),
];

fn new_deck() -> Vec<Card> {
    let mut deck = Vec::with_capacity(52);
    for &suit in SUITS.iter() {
        for &(label, weight) in RANKS.iter() {
            deck.push(Card { suit, label, weight });
        }
    }
    deck.shuffle(&mut thread_rng());
    deck
}

fn card_str(c: &Card) -> String {
    format!("`{}{}`", c.suit, c.label)
}

// ---------- Entry point ----------

#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    let database_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = PgPool::connect(&database_url)
        .await
        .expect("failed to connect to DB");
    let bot = Bot::from_env();

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(on_message))
        .branch(Update::filter_callback_query().endpoint(on_callback));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![pool])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

// ---------- Message handler ----------

async fn on_message(bot: Bot, msg: Message, pool: PgPool) -> ResponseResult<()> {
    let Some(user) = msg.from() else { return Ok(()) };
    let user_id = user.id.0 as i64;
    let username = user.username.clone().unwrap_or_else(|| "Anonymous".into());
    let chat_id = msg.chat.id;

    if ensure_user(&pool, user_id, &username).await.is_err() {
        return Ok(());
    }

    let Some(text) = msg.text() else { return Ok(()) };
    let mut parts = text.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();

    match cmd {
        "/start" | "/balance" => {
            let (tokens, debt) = stats(&pool, user_id).await.unwrap_or((0, 0));
            let text = format!(
                "🎰 *Welcome to Capy Casino\\!* 🎰\n\n💰 Balance: *{tokens} tokens*\n💸 Debt: *{debt} tokens*\n\n\
                *Commands:*\n/spin <bet>\n/flip <heads\\|tails> <bet>\n/dice <1\\-6> <bet>\n\
                /blackjack <bet>\n/poker <bet>\n/faucet\n/networth"
            );
            bot.send_message(chat_id, text)
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
        }

        "/faucet" => {
            let (tokens, _) = stats(&pool, user_id).await.unwrap_or((0, 0));
            if tokens < 50 {
                sqlx::query(
                    "UPDATE players SET tokens = tokens + 500, debt = debt + 500 WHERE user_id = $1",
                )
                .bind(user_id)
                .execute(&pool)
                .await
                .ok();
                bot.send_message(chat_id, "💸 Casino issued you a 500 token loan (added to debt).")
                    .await?;
            } else {
                bot.send_message(chat_id, "🛑 Faucet is only for broke capybaras (< 50 tokens).")
                    .await?;
            }
        }

        "/networth" => {
            let row = sqlx::query(
                "SELECT COALESCE(SUM(tokens),0)::BIGINT AS t, COALESCE(SUM(debt),0)::BIGINT AS d FROM players",
            )
            .fetch_one(&pool)
            .await;
            let (total_tokens, total_debt): (i64, i64) = match row {
                Ok(r) => (r.get("t"), r.get("d")),
                Err(_) => (0, 0),
            };
            let casino_net = total_debt - total_tokens;
            bot.send_message(
                chat_id,
                format!(
                    "🏢 Casino Balance Sheet\n\nPlayer liquidity: {total_tokens}\nOutstanding debt: {total_debt}\nCasino net worth: {casino_net}"
                ),
            )
            .await?;
        }

        "/spin" => {
            let Some(bet) = valid_bet(&pool, user_id, rest.first()).await else {
                bot.send_message(chat_id, "❌ Invalid bet or insufficient funds!").await?;
                return Ok(());
            };
            charge(&pool, user_id, bet).await;

            let dice = bot.send_dice(chat_id).emoji(DiceEmoji::SlotMachine).await?;
            let value = dice.dice().map(|d| d.value).unwrap_or(0);
            let mult = match value {
                64 => 10,
                1 | 22 | 43 => 3,
                _ => 0,
            };
            let winnings = bet * mult;
            pay(&pool, user_id, winnings).await;

            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let text = if winnings > 0 {
                format!("🎉 WINNER! You won {winnings} tokens!")
            } else {
                format!("😢 Lost {bet} tokens.")
            };
            bot.send_message(chat_id, text).await?;
        }

        "/flip" => {
            let choice = rest.first().map(|s| s.to_lowercase()).unwrap_or_default();
            let bet_arg = rest.get(1).copied();
            if choice != "heads" && choice != "tails" {
                bot.send_message(chat_id, "❌ Usage: /flip <heads|tails> <bet>").await?;
                return Ok(());
            }
            let Some(bet) = valid_bet(&pool, user_id, bet_arg).await else {
                bot.send_message(chat_id, "❌ Invalid bet or insufficient funds!").await?;
                return Ok(());
            };
            charge(&pool, user_id, bet).await;

            let heads = thread_rng().gen_bool(0.5);
            let won = (heads && choice == "heads") || (!heads && choice == "tails");
            let winnings = if won { bet * 2 } else { 0 };
            pay(&pool, user_id, winnings).await;

            let text = if won {
                format!("🎉 It's {}! Won {winnings} tokens!", if heads { "heads" } else { "tails" })
            } else {
                format!("💸 It's {}! Lost {bet} tokens.", if heads { "heads" } else { "tails" })
            };
            bot.send_message(chat_id, text).await?;
        }

        "/dice" => {
            let guess: i32 = rest.first().and_then(|s| s.parse().ok()).unwrap_or(0);
            let bet_arg = rest.get(1).copied();
            if !(1..=6).contains(&guess) {
                bot.send_message(chat_id, "❌ Usage: /dice <1-6> <bet>").await?;
                return Ok(());
            }
            let Some(bet) = valid_bet(&pool, user_id, bet_arg).await else {
                bot.send_message(chat_id, "❌ Invalid bet or insufficient funds!").await?;
                return Ok(());
            };
            charge(&pool, user_id, bet).await;

            let dice = bot.send_dice(chat_id).emoji(DiceEmoji::Dice).await?;
            let value = dice.dice().map(|d| d.value as i32).unwrap_or(0);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            if value == guess {
                let winnings = bet * 5;
                pay(&pool, user_id, winnings).await;
                bot.send_message(chat_id, format!("🎯 Match! Won {winnings} tokens!")).await?;
            } else {
                bot.send_message(chat_id, format!("🎲 Landed on {value}. Lost {bet} tokens.")).await?;
            }
        }

        "/blackjack" => {
            let Some(bet) = valid_bet(&pool, user_id, rest.first()).await else {
                bot.send_message(chat_id, "❌ Invalid bet or insufficient funds!").await?;
                return Ok(());
            };
            charge(&pool, user_id, bet).await;

            let mut deck = new_deck();
            let player = vec![deck.pop().unwrap(), deck.pop().unwrap()];
            let dealer = vec![deck.pop().unwrap(), deck.pop().unwrap()];

            save_blackjack(&pool, user_id, bet, &player, &dealer, &deck).await;

            let text = bj_status(&player, &dealer, false);
            let kb = bj_keyboard();
            bot.send_message(chat_id, text).reply_markup(kb).await?;
        }

        "/poker" => {
            let Some(bet) = valid_bet(&pool, user_id, rest.first()).await else {
                bot.send_message(chat_id, "❌ Invalid bet or insufficient funds!").await?;
                return Ok(());
            };
            charge(&pool, user_id, bet).await;

            let mut deck = new_deck();
            let hand: Vec<Card> = (0..5).map(|_| deck.pop().unwrap()).collect();
            let holds = vec![false; 5];

            save_poker(&pool, user_id, bet, &hand, &holds, &deck).await;

            let text = poker_status(&hand, &holds, false);
            let kb = poker_keyboard(&holds);
            bot.send_message(chat_id, text).reply_markup(kb).await?;
        }

        _ => {}
    }

    Ok(())
}

// ---------- Callback handler ----------

async fn on_callback(bot: Bot, q: CallbackQuery, pool: PgPool) -> ResponseResult<()> {
    let user_id = q.from.id.0 as i64;
    let data = q.data.clone().unwrap_or_default();

    let Some(msg_ref) = q.message.as_ref() else {
        bot.answer_callback_query(q.id).await?;
        return Ok(());
    };
    let (chat_id, message_id) = chat_and_message_id(msg_ref);

    if let Some(action) = data.strip_prefix("bj_") {
        handle_blackjack(&bot, &pool, user_id, chat_id, message_id, action).await?;
    } else if let Some(action) = data.strip_prefix("p_") {
        handle_poker(&bot, &pool, user_id, chat_id, message_id, action).await?;
    }

    bot.answer_callback_query(q.id).await?;
    Ok(())
}

/// MaybeInaccessibleMessage wraps either a full Message or a stub —
/// pull out chat_id/message_id from whichever variant we got.
fn chat_and_message_id(msg: &MaybeInaccessibleMessage) -> (ChatId, MessageId) {
    match msg {
        MaybeInaccessibleMessage::Regular(m) => (m.chat.id, m.id),
        MaybeInaccessibleMessage::Inaccessible(m) => (m.chat.id, m.message_id),
    }
}

async fn handle_blackjack(
    bot: &Bot,
    pool: &PgPool,
    user_id: i64,
    chat_id: ChatId,
    message_id: MessageId,
    action: &str,
) -> ResponseResult<()> {
    let Some(row) = sqlx::query("SELECT * FROM blackjack_games WHERE user_id = $1")
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .unwrap_or(None)
    else {
        return Ok(());
    };

    let bet: i32 = row.get("bet");
    let mut player: Vec<Card> = serde_json::from_str(row.get("player_hand")).unwrap_or_default();
    let mut dealer: Vec<Card> = serde_json::from_str(row.get("dealer_hand")).unwrap_or_default();
    let mut deck: Vec<Card> = serde_json::from_str(row.get("deck")).unwrap_or_default();

    match action {
        "hit" => {
            player.push(deck.pop().unwrap());
            if bj_score(&player) > 21 {
                clear_blackjack(pool, user_id).await;
                let text = format!(
                    "{}\n\n💥 BUST! Lost {bet} tokens.",
                    bj_status(&player, &dealer, true)
                );
                bot.edit_message_text(chat_id, message_id, text).await?;
            } else {
                save_blackjack(pool, user_id, bet, &player, &dealer, &deck).await;
                bot.edit_message_text(chat_id, message_id, bj_status(&player, &dealer, false))
                    .reply_markup(bj_keyboard())
                    .await?;
            }
        }
        "stand" => {
            while bj_score(&dealer) < 17 {
                dealer.push(deck.pop().unwrap());
            }
            let p_score = bj_score(&player);
            let d_score = bj_score(&dealer);
            clear_blackjack(pool, user_id).await;

            let outcome = if d_score > 21 || p_score > d_score {
                pay(pool, user_id, bet * 2).await;
                format!("🎉 YOU WIN! Won {bet} tokens!")
            } else if p_score == d_score {
                pay(pool, user_id, bet).await;
                "🤝 Push! Bet returned.".to_string()
            } else {
                format!("💸 Dealer wins. Lost {bet} tokens.")
            };

            let text = format!("{}\n\n{}", bj_status(&player, &dealer, true), outcome);
            bot.edit_message_text(chat_id, message_id, text).await?;
        }
        _ => {}
    }

    Ok(())
}

async fn handle_poker(
    bot: &Bot,
    pool: &PgPool,
    user_id: i64,
    chat_id: ChatId,
    message_id: MessageId,
    action: &str,
) -> ResponseResult<()> {
    let Some(row) = sqlx::query("SELECT * FROM poker_games WHERE user_id = $1")
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .unwrap_or(None)
    else {
        return Ok(());
    };

    let bet: i32 = row.get("bet");
    let mut hand: Vec<Card> = serde_json::from_str(row.get("hand")).unwrap_or_default();
    let mut holds: Vec<bool> = serde_json::from_str(row.get("holds")).unwrap_or_default();
    let mut deck: Vec<Card> = serde_json::from_str(row.get("deck")).unwrap_or_default();

    if let Some(idx_str) = action.strip_prefix("hold_") {
        let Ok(idx) = idx_str.parse::<usize>() else { return Ok(()) };
        if idx < holds.len() {
            holds[idx] = !holds[idx];
        }
        save_poker(pool, user_id, bet, &hand, &holds, &deck).await;
        bot.edit_message_text(chat_id, message_id, poker_status(&hand, &holds, false))
            .reply_markup(poker_keyboard(&holds))
            .await?;
    } else if action == "draw" {
        clear_poker(pool, user_id).await;
        for i in 0..hand.len().min(holds.len()) {
            if !holds[i] {
                hand[i] = deck.pop().unwrap();
            }
        }
        let rank = poker_rank(&hand);
        let mult = match rank {
            "Royal Flush" => 250,
            "Straight Flush" => 50,
            "Four of a Kind" => 25,
            "Full House" => 9,
            "Flush" => 6,
            "Straight" => 4,
            "Three of a Kind" => 3,
            "Two Pair" => 2,
            "Jacks or Better" => 1,
            _ => 0,
        };
        let winnings = bet * mult;
        pay(pool, user_id, winnings).await;

        let mut text = poker_status(&hand, &holds, true);
        if winnings > 0 {
            text.push_str(&format!("\n\n🎉 {rank}! Won {winnings} tokens!"));
        } else {
            text.push_str(&format!("\n\n💸 {rank}. Lost {bet} tokens."));
        }
        bot.edit_message_text(chat_id, message_id, text).await?;
    }

    Ok(())
}

// ---------- Blackjack helpers ----------

fn bj_score(hand: &[Card]) -> i32 {
    let mut score = 0;
    let mut aces = 0;
    for c in hand {
        score += c.weight;
        if c.label == "A" {
            aces += 1;
        }
    }
    while score > 21 && aces > 0 {
        score -= 10;
        aces -= 1;
    }
    score
}

fn bj_status(player: &[Card], dealer: &[Card], show_all: bool) -> String {
    let p_cards: Vec<String> = player.iter().map(card_str).collect();
    let p_score = bj_score(player);
    if show_all {
        let d_cards: Vec<String> = dealer.iter().map(card_str).collect();
        format!(
            "🃏 BLACKJACK 🃏\n\n👤 You: {} (Total: {})\n🤖 Dealer: {} (Total: {})",
            p_cards.join(" "),
            p_score,
            d_cards.join(" "),
            bj_score(dealer)
        )
    } else {
        format!(
            "🃏 BLACKJACK 🃏\n\n👤 You: {} (Total: {})\n🤖 Dealer: {} 📇 (Total: {} + ?)",
            p_cards.join(" "),
            p_score,
            card_str(&dealer[0]),
            dealer[0].weight
        )
    }
}

fn bj_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("🟢 Hit", "bj_hit"),
        InlineKeyboardButton::callback("🛑 Stand", "bj_stand"),
    ]])
}

async fn save_blackjack(pool: &PgPool, user_id: i64, bet: i32, player: &[Card], dealer: &[Card], deck: &[Card]) {
    sqlx::query(
        "INSERT INTO blackjack_games (user_id, bet, player_hand, dealer_hand, deck) VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (user_id) DO UPDATE SET bet=$2, player_hand=$3, dealer_hand=$4, deck=$5",
    )
    .bind(user_id)
    .bind(bet)
    .bind(serde_json::to_string(player).unwrap())
    .bind(serde_json::to_string(dealer).unwrap())
    .bind(serde_json::to_string(deck).unwrap())
    .execute(pool)
    .await
    .ok();
}

async fn clear_blackjack(pool: &PgPool, user_id: i64) {
    sqlx::query("DELETE FROM blackjack_games WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .ok();
}

// ---------- Poker helpers ----------

fn poker_status(hand: &[Card], holds: &[bool], finished: bool) -> String {
    let mut out = "🃏 VIDEO POKER 🃏\n\n".to_string();
    for (i, c) in hand.iter().enumerate() {
        let held = holds.get(i).copied().unwrap_or(false) && !finished;
        out.push_str(&format!(
            "Card {}: {}{}\n",
            i + 1,
            card_str(c),
            if held { " [HELD]" } else { "" }
        ));
    }
    if !finished {
        out.push_str("\nSelect cards to hold, then Draw!");
    }
    out
}

fn poker_keyboard(holds: &[bool]) -> InlineKeyboardMarkup {
    let row: Vec<InlineKeyboardButton> = holds
        .iter()
        .enumerate()
        .map(|(i, held)| {
            let label = if *held { format!("🔒 C{}", i + 1) } else { format!("🔓 C{}", i + 1) };
            InlineKeyboardButton::callback(label, format!("p_hold_{i}"))
        })
        .collect();
    InlineKeyboardMarkup::new(vec![row, vec![InlineKeyboardButton::callback("💥 DRAW", "p_draw")]])
}

async fn save_poker(pool: &PgPool, user_id: i64, bet: i32, hand: &[Card], holds: &[bool], deck: &[Card]) {
    sqlx::query(
        "INSERT INTO poker_games (user_id, bet, hand, holds, deck) VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (user_id) DO UPDATE SET bet=$2, hand=$3, holds=$4, deck=$5",
    )
    .bind(user_id)
    .bind(bet)
    .bind(serde_json::to_string(hand).unwrap())
    .bind(serde_json::to_string(holds).unwrap())
    .bind(serde_json::to_string(deck).unwrap())
    .execute(pool)
    .await
    .ok();
}

async fn clear_poker(pool: &PgPool, user_id: i64) {
    sqlx::query("DELETE FROM poker_games WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .ok();
}

fn poker_rank(hand: &[Card]) -> &'static str {
    let mut values: Vec<i32> = hand
        .iter()
        .map(|c| match c.label {
            "J" => 11,
            "Q" => 12,
            "K" => 13,
            "A" => 14,
            _ => c.weight,
        })
        .collect();
    values.sort_unstable();

    let is_flush = hand.iter().all(|c| c.suit == hand[0].suit);
    let is_straight =
        values.windows(2).all(|w| w[1] == w[0] + 1) || values == [2, 3, 4, 5, 14];

    let mut counts = std::collections::HashMap::new();
    for &v in &values {
        *counts.entry(v).or_insert(0) += 1;
    }
    let mut counts_vec: Vec<i32> = counts.values().copied().collect();
    counts_vec.sort_unstable_by(|a, b| b.cmp(a));

    if is_flush && is_straight && values[4] == 14 && values[0] == 10 {
        return "Royal Flush";
    }
    if is_flush && is_straight {
        return "Straight Flush";
    }
    if counts_vec[0] == 4 {
        return "Four of a Kind";
    }
    if counts_vec[0] == 3 && counts_vec.get(1) == Some(&2) {
        return "Full House";
    }
    if is_flush {
        return "Flush";
    }
    if is_straight {
        return "Straight";
    }
    if counts_vec[0] == 3 {
        return "Three of a Kind";
    }
    if counts_vec[0] == 2 && counts_vec.get(1) == Some(&2) {
        return "Two Pair";
    }
    if counts_vec[0] == 2 {
        if let Some((&val, _)) = counts.iter().find(|&(_, &c)| c == 2) {
            if val >= 11 {
                return "Jacks or Better";
            }
        }
    }
    "High Card"
}

// ---------- Player/DB helpers ----------

async fn ensure_user(pool: &PgPool, user_id: i64, username: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO players (user_id, username, tokens) VALUES ($1, $2, 1000) ON CONFLICT (user_id) DO NOTHING",
    )
    .bind(user_id)
    .bind(username)
    .execute(pool)
    .await?;
    Ok(())
}

async fn stats(pool: &PgPool, user_id: i64) -> Result<(i32, i32), sqlx::Error> {
    let row = sqlx::query("SELECT tokens, debt FROM players WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await?;
    Ok((row.get("tokens"), row.get("debt")))
}

/// Parses a bet argument and checks it's positive and affordable. Returns None if invalid.
async fn valid_bet(pool: &PgPool, user_id: i64, arg: Option<&&str>) -> Option<i32> {
    let bet: i32 = arg?.parse().ok()?;
    let (tokens, _) = stats(pool, user_id).await.ok()?;
    if bet > 0 && bet <= tokens {
        Some(bet)
    } else {
        None
    }
}

async fn charge(pool: &PgPool, user_id: i64, amount: i32) {
    sqlx::query("UPDATE players SET tokens = tokens - $1 WHERE user_id = $2")
        .bind(amount)
        .bind(user_id)
        .execute(pool)
        .await
        .ok();
}

async fn pay(pool: &PgPool, user_id: i64, amount: i32) {
    if amount == 0 {
        return;
    }
    sqlx::query("UPDATE players SET tokens = tokens + $1 WHERE user_id = $2")
        .bind(amount)
        .bind(user_id)
        .execute(pool)
        .await
        .ok();
}