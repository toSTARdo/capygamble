use std::collections::HashMap;
use std::env;
use std::time::Duration;

use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use rand::seq::{IndexedRandom, SliceRandom};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use sqlx::{postgres::PgPoolOptions, PgPool, Row};
use teloxide::prelude::*;
use teloxide::types::{
    ChatId, DiceEmoji, InlineKeyboardButton, InlineKeyboardMarkup, MaybeInaccessibleMessage,
    MessageId, ParseMode,
};
use tokio::net::TcpListener;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---------- Configuration & Limits ----------

const MIN_BET: i32 = 10;
const MAX_BET: i32 = 50_000;
const DAILY_REWARD: i32 = 1_000;
const GLOBAL_COOLDOWN_SECS: i64 = 3;

// ---------- Localization ----------

static LOCALES: Lazy<HashMap<String, HashMap<String, serde_json::Value>>> = Lazy::new(|| {
    // Falls back gracefully if translations.json is missing during development
    let json_str = include_str!("translations.json");
    serde_json::from_str(json_str).expect("Failed to parse translations.json")
});

fn t(lang: &str, key: &str) -> String {
    let value = LOCALES.get(lang).and_then(|m| m.get(key));

    match value {
        Some(serde_json::Value::Array(arr)) if !arr.is_empty() => arr
            .choose(&mut rand::rng())
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        Some(serde_json::Value::String(s)) => s.clone(),
        _ => {
            // Provide intelligent defaults for newly added features to prevent <Missing: key>
            match key {
                "daily_success" => format!("✅ You claimed your daily reward of {} tokens!", DAILY_REWARD),
                "daily_wait" => "⏳ You must wait {hours}h {mins}m before claiming your next daily bonus.".to_string(),
                "cooldown_active" => "⏳ Please slow down! Wait a few seconds between games.".to_string(),
                "bet_out_of_bounds" => format!("⚠️ Bet must be between {} and {} tokens.", MIN_BET, MAX_BET),
                "achievement_unlocked" => "🌟 <b>ACHIEVEMENT UNLOCKED: New Personal Best!</b> 🌟\nCongratulations on your biggest win yet!".to_string(),
                "top_title" => "🏆 <b>Top 10 Richest Players</b> 🏆".to_string(),
                "wrong_user" => "🚫 This isn't your bet!".to_string(),
                "flip_prompt" => "🪙 Coin flip for {bet} 🥮 — choose your side!".to_string(),
                "dice_prompt" => "🎲 Dice roll for {bet} 🥮 — pick a number!".to_string(),
                _ => format!("<Missing: {}>", key),
            }
        }
    }
}

// ---------- Card model ----------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Card {
    suit: String,
    label: String,
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
            deck.push(Card { suit: suit.to_string(), label: label.to_string(), weight });
        }
    }
    deck.shuffle(&mut rand::rng());
    deck
}

// FIX: Changed from Markdown formatting to HTML formatting (<code>) to prevent ParseMode crash bugs
fn card_str(c: &Card) -> String {
    format!("<code>{}{}</code>", c.suit, c.label)
}

// ---------- Entry point ----------

#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    let database_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    
    // FIX: Replaced basic connect with robust connection pooling to resolve "TX is dead" panics
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(60))
        .connect(&database_url)
        .await
        .expect("Failed to connect to DB");

    // Perform automatic database migrations for new features
    init_db(&pool).await;

    // Free hosting tiers (Render, Railway, Fly, etc.) spin the app down after a
    // period of no incoming HTTP traffic. This tiny listener gives external
    // uptime pingers (UptimeRobot, cron-job.org) something to hit every few
    // minutes so the instance never goes idle. It's independent of the bot
    // logic — just says "OK" to any request.
    tokio::spawn(run_keepalive_server());

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

// Automatically apply schema updates for Achievements, Daily Bonuses, and Cooldowns
async fn init_db(pool: &PgPool) {
    let queries = [
        "ALTER TABLE players ADD COLUMN IF NOT EXISTS default_bet INTEGER DEFAULT 0;",
        "ALTER TABLE players ADD COLUMN IF NOT EXISTS last_daily TIMESTAMPTZ;",
        "ALTER TABLE players ADD COLUMN IF NOT EXISTS games_played INTEGER DEFAULT 0;",
        "ALTER TABLE players ADD COLUMN IF NOT EXISTS biggest_win INTEGER DEFAULT 0;",
        "ALTER TABLE players ADD COLUMN IF NOT EXISTS last_action TIMESTAMPTZ;",
    ];

    for query in queries {
        if let Err(e) = sqlx::query(query).execute(pool).await {
            log::warn!("Schema migration warning: {}", e);
        }
    }

    log::info!("Database schema initialized.");

    sqlx::query("INSERT INTO players (user_id, username, tokens) VALUES (0, 'Casino', 1000000) ON CONFLICT (user_id) DO NOTHING;")
        .execute(pool)
        .await
        .ok();
}

/// Minimal HTTP server for keep-alive pings. Binds to $PORT (defaults to 8080,
/// which most free hosting platforms expect) and replies 200 OK to any request,
/// on any path. Point an external uptime pinger (UptimeRobot, cron-job.org,
/// even a GitHub Actions cron hitting `curl your-app-url`) at this every
/// 5-10 minutes to stop the platform from spinning the instance down.
async fn run_keepalive_server() {
    let port = env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let addr = format!("0.0.0.0:{port}");

    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            log::warn!("Keep-alive server failed to bind {addr}: {e}");
            return;
        }
    };
    log::info!("Keep-alive server listening on {addr}");

    loop {
        let (mut socket, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                log::warn!("Keep-alive accept error: {e}");
                continue;
            }
        };

        tokio::spawn(async move {
            // Drain whatever the client sent (we don't care about the request)
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;

            let body = "OK";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });
    }
}

// ---------- Message handler ----------

async fn on_message(bot: Bot, msg: Message, pool: PgPool) -> ResponseResult<()> {
    let Some(user) = msg.from.as_ref() else { return Ok(()) };
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

    // Fetch comprehensive user stats including limits and cooldowns
    let player_data = stats(&pool, user_id).await.unwrap_or_default();
    let tokens = player_data.tokens;
    let debt = player_data.debt;
    let lang = player_data.lang.clone();
    let default_bet = player_data.default_bet;
    let biggest_win = player_data.biggest_win;

    // Command Router
    match cmd {
        "/start" | "/balance" => {
            let text = t(&lang, "welcome")
                .replace("{tokens}", &tokens.to_string())
                .replace("{debt}", &debt.to_string());
            // FIX: Using HTML ParseMode to prevent unescaped Markdown from crashing the API
            bot.send_message(chat_id, text)
                .parse_mode(ParseMode::Html)
                .await?;
        }

        "/help" => {
            bot.send_message(chat_id, t(&lang, "help"))
                .parse_mode(ParseMode::Html)
                .await?;
        }

        "/change_lang" => {
            let new_lang = match rest.first().copied() {
                Some("uk") => "uk",
                _ => "en",
            };
            sqlx::query("UPDATE players SET lang = $1 WHERE user_id = $2")
                .bind(new_lang)
                .bind(user_id)
                .execute(&pool)
                .await
                .ok();
            bot.send_message(chat_id, t(new_lang, "lang_set")).await?;
        }

        "/setbet" => {
            let Some(new_bet) = rest.first().and_then(|s| s.parse::<i32>().ok()) else {
                bot.send_message(chat_id, t(&lang, "setbet_usage")).await?;
                return Ok(());
            };
            
            if new_bet < MIN_BET || new_bet > MAX_BET {
                bot.send_message(chat_id, t(&lang, "bet_out_of_bounds")).await?;
                return Ok(());
            }

            sqlx::query("UPDATE players SET default_bet = $1 WHERE user_id = $2")
                .bind(new_bet)
                .bind(user_id)
                .execute(&pool)
                .await
                .ok();
            
            let text = t(&lang, "setbet_success").replace("{bet}", &new_bet.to_string());
            bot.send_message(chat_id, text).await?;
        }

        // ADDITION: Daily Rewards system
        "/daily" => {
            let now = Utc::now();
            let can_claim = match player_data.last_daily {
                Some(last) => now.signed_duration_since(last).num_hours() >= 24,
                None => true,
            };

            if can_claim {
                sqlx::query("UPDATE players SET tokens = tokens + $1, last_daily = $2 WHERE user_id = $3")
                    .bind(DAILY_REWARD)
                    .bind(now)
                    .bind(user_id)
                    .execute(&pool)
                    .await
                    .ok();
                bot.send_message(chat_id, t(&lang, "daily_success")).await?;
            } else {
                let last = player_data.last_daily.unwrap();
                let duration_left = chrono::Duration::hours(24) - now.signed_duration_since(last);
                let hours = duration_left.num_hours();
                let mins = duration_left.num_minutes() % 60;
                
                let msg = t(&lang, "daily_wait")
                    .replace("{hours}", &hours.to_string())
                    .replace("{mins}", &mins.to_string());
                bot.send_message(chat_id, msg).await?;
            }
        }

        // ADDITION: Leaderboard logic
        "/top" => {
            let rows = sqlx::query("SELECT username, tokens FROM players ORDER BY tokens DESC LIMIT 10")
                .fetch_all(&pool)
                .await;
            
            if let Ok(records) = rows {
                let mut lb = format!("{}\n\n", t(&lang, "top_title"));
                for (i, row) in records.iter().enumerate() {
                    let name: String = row.try_get("username").unwrap_or_else(|_| "Anonymous".to_string());
                    let user_tokens: i32 = row.try_get("tokens").unwrap_or(0);
                    lb.push_str(&format!("{}. <b>{}</b> - {} tokens\n", i + 1, name, user_tokens));
                }
                bot.send_message(chat_id, lb)
                    .parse_mode(ParseMode::Html)
                    .await?;
            }
        }

        "/faucet" => {
            let casino_tokens: i32 = sqlx::query_scalar("SELECT tokens FROM players WHERE user_id = 0")
                .fetch_one(&pool).await.unwrap_or(0);

            if tokens < 50 && casino_tokens >= 500 {
                sqlx::query("UPDATE players SET tokens = tokens + 500, debt = debt + 500 WHERE user_id = $1")
                    .bind(user_id).execute(&pool).await.ok();
                sqlx::query("UPDATE players SET tokens = tokens - 500 WHERE user_id = 0")
                    .execute(&pool).await.ok();
                    
                bot.send_message(chat_id, t(&lang, "faucet_granted")).await?;
            } else {
                bot.send_message(chat_id, "⚠️ The casino is out of funds or you are too rich!").await?;
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

            let text = t(&lang, "networth")
                .replace("{tokens}", &total_tokens.to_string())
                .replace("{debt}", &total_debt.to_string())
                .replace("{net}", &casino_net.to_string());

            bot.send_message(chat_id, text).parse_mode(ParseMode::Html).await?;
        }

        "/spin" => {
            if !check_cooldown(&pool, user_id).await {
                bot.send_message(chat_id, t(&lang, "cooldown_active")).await?;
                return Ok(());
            }

            let bet = match resolve_bet(&pool, user_id, rest.first().copied(), default_bet).await {
                Ok(b) => b,
                Err(err_key) => {
                    bot.send_message(chat_id, t(&lang, err_key)).await?;
                    return Ok(());
                }
            };
            charge(&pool, user_id, bet).await;
            increment_games_played(&pool, user_id).await;

            let dice = bot.send_dice(chat_id).emoji(DiceEmoji::SlotMachine).await?;
            let value = dice.dice().map(|d| d.value).unwrap_or(0);
            let mult = match value {
                64 => 10,
                1 | 22 | 43 => 3,
                _ => 0,
            };
            let winnings = bet * mult;

            let is_new_record = pay(&pool, user_id, winnings, biggest_win).await;

            tokio::time::sleep(Duration::from_secs(2)).await;
            let mut text = if winnings > 0 {
                t(&lang, "spin_win").replace("{winnings}", &winnings.to_string())
            } else {
                t(&lang, "spin_lose").replace("{bet}", &bet.to_string())
            };

            if is_new_record {
                text.push_str(&format!("\n\n{}", t(&lang, "achievement_unlocked")));
            }

            bot.send_message(chat_id, text).parse_mode(ParseMode::Html).await?;
        }

        "/flip" => {
            if !check_cooldown(&pool, user_id).await {
                bot.send_message(chat_id, t(&lang, "cooldown_active")).await?;
                return Ok(());
            }

            let bet = match resolve_bet(&pool, user_id, rest.first().copied(), default_bet).await {
                Ok(b) => b,
                Err(err_key) => {
                    bot.send_message(chat_id, t(&lang, err_key)).await?;
                    return Ok(());
                }
            };

            // Keyboard format: flip_<side>_<bet>_<user_id>
            let kb = InlineKeyboardMarkup::new(vec![vec![
                InlineKeyboardButton::callback("🦅 Heads", format!("flip_h_{}_{}", bet, user_id)),
                InlineKeyboardButton::callback("🪙 Tails", format!("flip_t_{}_{}", bet, user_id)),
            ]]);

            let text = t(&lang, "flip_prompt").replace("{bet}", &bet.to_string());
            bot.send_message(chat_id, text).reply_markup(kb).await?;
        }

        "/dice" => {
            if !check_cooldown(&pool, user_id).await {
                bot.send_message(chat_id, t(&lang, "cooldown_active")).await?;
                return Ok(());
            }

            let bet = match resolve_bet(&pool, user_id, rest.first().copied(), default_bet).await {
                Ok(b) => b,
                Err(err_key) => {
                    bot.send_message(chat_id, t(&lang, err_key)).await?;
                    return Ok(());
                }
            };

            // Keyboard format: dice_<guess>_<bet>_<user_id>
            let kb = InlineKeyboardMarkup::new(vec![
                vec![
                    InlineKeyboardButton::callback("⚀ 1", format!("dice_1_{}_{}", bet, user_id)),
                    InlineKeyboardButton::callback("⚁ 2", format!("dice_2_{}_{}", bet, user_id)),
                    InlineKeyboardButton::callback("⚂ 3", format!("dice_3_{}_{}", bet, user_id)),
                ],
                vec![
                    InlineKeyboardButton::callback("⚃ 4", format!("dice_4_{}_{}", bet, user_id)),
                    InlineKeyboardButton::callback("⚄ 5", format!("dice_5_{}_{}", bet, user_id)),
                    InlineKeyboardButton::callback("⚅ 6", format!("dice_6_{}_{}", bet, user_id)),
                ]
            ]);

            let text = t(&lang, "dice_prompt").replace("{bet}", &bet.to_string());
            bot.send_message(chat_id, text).reply_markup(kb).await?;
        }

        "/blackjack" => {
            if !check_cooldown(&pool, user_id).await {
                bot.send_message(chat_id, t(&lang, "cooldown_active")).await?;
                return Ok(());
            }

            let bet = match resolve_bet(&pool, user_id, rest.first().copied(), default_bet).await {
                Ok(b) => b,
                Err(err_key) => {
                    bot.send_message(chat_id, t(&lang, err_key)).await?;
                    return Ok(());
                }
            };
            charge(&pool, user_id, bet).await;
            increment_games_played(&pool, user_id).await;

            let mut deck = new_deck();
            let player = vec![deck.pop().unwrap(), deck.pop().unwrap()];
            let dealer = vec![deck.pop().unwrap(), deck.pop().unwrap()];

            save_blackjack(&pool, user_id, bet, &player, &dealer, &deck).await;

            let text = bj_status(&lang, &player, &dealer, false);
            let kb = bj_keyboard(&lang);
            bot.send_message(chat_id, text).reply_markup(kb).parse_mode(ParseMode::Html).await?;
        }

        "/poker" => {
            if !check_cooldown(&pool, user_id).await {
                bot.send_message(chat_id, t(&lang, "cooldown_active")).await?;
                return Ok(());
            }

            let bet = match resolve_bet(&pool, user_id, rest.first().copied(), default_bet).await {
                Ok(b) => b,
                Err(err_key) => {
                    bot.send_message(chat_id, t(&lang, err_key)).await?;
                    return Ok(());
                }
            };
            charge(&pool, user_id, bet).await;
            increment_games_played(&pool, user_id).await;

            let mut deck = new_deck();
            let hand: Vec<Card> = (0..5).map(|_| deck.pop().unwrap()).collect();
            let holds = vec![false; 5];

            save_poker(&pool, user_id, bet, &hand, &holds, &deck).await;

            let text = poker_status(&lang, &hand, &holds, false);
            let kb = poker_keyboard(&lang, &holds);
            bot.send_message(chat_id, text).reply_markup(kb).parse_mode(ParseMode::Html).await?;
        }

        "/payloan" => {
            let amount = rest.first().and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
            
            if amount <= 0 || amount > tokens {
                bot.send_message(chat_id, "⚠️ Invalid amount.").await?;
                return Ok(());
            }

            sqlx::query("UPDATE players SET tokens = tokens - $1, debt = debt - $1 WHERE user_id = $2")
                .bind(amount)
                .bind(user_id)
                .execute(&pool).await.ok();
                
            sqlx::query("UPDATE players SET tokens = tokens + $1 WHERE user_id = 0")
                .bind(amount)
                .execute(&pool).await.ok();

            bot.send_message(chat_id, format!("✅ Repaid {} tokens to the bank.", amount)).await?;
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
    
    let player_data = stats(&pool, user_id).await.unwrap_or_default();
    let lang = player_data.lang;
    let biggest_win = player_data.biggest_win;

    if let Some(action) = data.strip_prefix("bj_") {
        handle_blackjack(&bot, &pool, &lang, user_id, chat_id, message_id, action, biggest_win).await?;
    } else if let Some(action) = data.strip_prefix("p_") {
        handle_poker(&bot, &pool, &lang, user_id, chat_id, message_id, action, biggest_win).await?;
    } else if let Some(action) = data.strip_prefix("flip_") {
        handle_flip(&bot, &pool, &lang, user_id, chat_id, message_id, action, biggest_win, q.id.clone()).await?;
    } else if let Some(action) = data.strip_prefix("dice_") {
        handle_dice(&bot, &pool, &lang, user_id, chat_id, message_id, action, biggest_win, q.id.clone()).await?;
    }

    let _ = bot.answer_callback_query(q.id).await;
    Ok(())
}

fn chat_and_message_id(msg: &MaybeInaccessibleMessage) -> (ChatId, MessageId) {
    match msg {
        MaybeInaccessibleMessage::Regular(m) => (m.chat.id, m.id),
        MaybeInaccessibleMessage::Inaccessible(m) => (m.chat.id, m.message_id),
    }
}

async fn handle_blackjack(
    bot: &Bot,
    pool: &PgPool,
    lang: &str,
    user_id: i64,
    chat_id: ChatId,
    message_id: MessageId,
    action: &str,
    biggest_win: i32,
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
    let player_json: String = row.get("player_hand");
    let dealer_json: String = row.get("dealer_hand");
    let deck_json: String = row.get("deck");
    drop(row);

    let mut player: Vec<Card> = serde_json::from_str(&player_json).unwrap_or_default();
    let mut dealer: Vec<Card> = serde_json::from_str(&dealer_json).unwrap_or_default();
    let mut deck: Vec<Card> = serde_json::from_str(&deck_json).unwrap_or_default();

    match action {
        "hit" => {
            player.push(deck.pop().unwrap());
            if bj_score(&player) > 21 {
                clear_blackjack(pool, user_id).await;
                let bust_msg = t(lang, "bj_bust").replace("{bet}", &bet.to_string());
                let text = format!("{}\n\n{}", bj_status(lang, &player, &dealer, true), bust_msg);
                bot.edit_message_text(chat_id, message_id, text).parse_mode(ParseMode::Html).await?;
            } else {
                save_blackjack(pool, user_id, bet, &player, &dealer, &deck).await;
                bot.edit_message_text(chat_id, message_id, bj_status(lang, &player, &dealer, false))
                    .reply_markup(bj_keyboard(lang))
                    .parse_mode(ParseMode::Html)
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

            let mut is_new_record = false;
            let outcome = if d_score > 21 || p_score > d_score {
                is_new_record = pay(pool, user_id, bet * 2, biggest_win).await;
                t(lang, "bj_win").replace("{bet}", &bet.to_string())
            } else if p_score == d_score {
                pay(pool, user_id, bet, biggest_win).await;
                t(lang, "bj_push")
            } else {
                t(lang, "bj_lose").replace("{bet}", &bet.to_string())
            };

            let mut text = format!("{}\n\n{}", bj_status(lang, &player, &dealer, true), outcome);
            
            if is_new_record {
                text.push_str(&format!("\n\n{}", t(lang, "achievement_unlocked")));
            }

            bot.edit_message_text(chat_id, message_id, text).parse_mode(ParseMode::Html).await?;
        }
        _ => {}
    }

    Ok(())
}

async fn handle_poker(
    bot: &Bot,
    pool: &PgPool,
    lang: &str,
    user_id: i64,
    chat_id: ChatId,
    message_id: MessageId,
    action: &str,
    biggest_win: i32,
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
    let hand_json: String = row.get("hand");
    let holds_json: String = row.get("holds");
    let deck_json: String = row.get("deck");
    drop(row);

    let mut hand: Vec<Card> = serde_json::from_str(&hand_json).unwrap_or_default();
    let mut holds: Vec<bool> = serde_json::from_str(&holds_json).unwrap_or_default();
    let mut deck: Vec<Card> = serde_json::from_str(&deck_json).unwrap_or_default();

    if let Some(idx_str) = action.strip_prefix("hold_") {
        let Ok(idx) = idx_str.parse::<usize>() else { return Ok(()) };
        if idx < holds.len() {
            holds[idx] = !holds[idx];
        }
        save_poker(pool, user_id, bet, &hand, &holds, &deck).await;
        bot.edit_message_text(chat_id, message_id, poker_status(lang, &hand, &holds, false))
            .reply_markup(poker_keyboard(lang, &holds))
            .parse_mode(ParseMode::Html)
            .await?;
    } else if action == "draw" {
        clear_poker(pool, user_id).await;
        for i in 0..hand.len().min(holds.len()) {
            if !holds[i] {
                hand[i] = deck.pop().unwrap();
            }
        }
        let rank_key = poker_rank_key(&hand);
        let mult = match rank_key {
            "rank_royal_flush" => 250,
            "rank_straight_flush" => 50,
            "rank_four_of_a_kind" => 25,
            "rank_full_house" => 9,
            "rank_flush" => 6,
            "rank_straight" => 4,
            "rank_three_of_a_kind" => 3,
            "rank_two_pair" => 2,
            "rank_jacks_or_better" => 1,
            _ => 0,
        };
        let winnings = bet * mult;
        let is_new_record = pay(pool, user_id, winnings, biggest_win).await;

        let mut text = poker_status(lang, &hand, &holds, true);
        let rank_str = t(lang, rank_key);
        if winnings > 0 {
            let win_msg = t(lang, "poker_win")
                .replace("{rank}", &rank_str)
                .replace("{winnings}", &winnings.to_string());
            text.push_str(&format!("\n\n{}", win_msg));
            
            if is_new_record {
                text.push_str(&format!("\n\n{}", t(lang, "achievement_unlocked")));
            }
        } else {
            let lose_msg = t(lang, "poker_lose")
                .replace("{rank}", &rank_str)
                .replace("{bet}", &bet.to_string());
            text.push_str(&format!("\n\n{}", lose_msg));
        }
        bot.edit_message_text(chat_id, message_id, text).parse_mode(ParseMode::Html).await?;
    }

    Ok(())
}

async fn handle_flip(
    bot: &Bot,
    pool: &PgPool,
    lang: &str,
    user_id: i64,
    chat_id: ChatId,
    message_id: MessageId,
    action: &str,
    biggest_win: i32,
    callback_id: teloxide::types::CallbackQueryId,
) -> ResponseResult<()> {
    let parts: Vec<&str> = action.split('_').collect();
    if parts.len() != 3 { return Ok(()); }
    
    let choice = parts[0];
    let bet: i32 = parts[1].parse().unwrap_or(0);
    let expected_user: i64 = parts[2].parse().unwrap_or(0);

    if user_id != expected_user {
        bot.answer_callback_query(callback_id).text(t(lang, "wrong_user")).show_alert(true).await?;
        return Ok(());
    }

    let player_data = stats(pool, user_id).await.unwrap_or_default();
    if bet > player_data.tokens {
        bot.edit_message_text(chat_id, message_id, t(lang, "invalid_bet")).await?;
        return Ok(());
    }

    charge(pool, user_id, bet).await;
    increment_games_played(pool, user_id).await;

    let heads = rand::rng().random_bool(0.5);
    let won = (heads && choice == "h") || (!heads && choice == "t");
    let winnings = if won { bet * 2 } else { 0 };

    let is_new_record = pay(pool, user_id, winnings, biggest_win).await;

    let side_str = t(lang, if heads { "coin_heads" } else { "coin_tails" });
    let mut text = if won {
        t(lang, "flip_win")
            .replace("{side}", &side_str)
            .replace("{winnings}", &winnings.to_string())
    } else {
        t(lang, "flip_lose")
            .replace("{side}", &side_str)
            .replace("{bet}", &bet.to_string())
    };

    if is_new_record {
        text.push_str(&format!("\n\n{}", t(lang, "achievement_unlocked")));
    }

    bot.edit_message_text(chat_id, message_id, text).parse_mode(ParseMode::Html).await?;
    Ok(())
}

async fn handle_dice(
    bot: &Bot,
    pool: &PgPool,
    lang: &str,
    user_id: i64,
    chat_id: ChatId,
    message_id: MessageId,
    action: &str,
    biggest_win: i32,
    callback_id: teloxide::types::CallbackQueryId,
) -> ResponseResult<()> {
    let parts: Vec<&str> = action.split('_').collect();
    if parts.len() != 3 { return Ok(()); }
    
    let guess: i32 = parts[0].parse().unwrap_or(0);
    let bet: i32 = parts[1].parse().unwrap_or(0);
    let expected_user: i64 = parts[2].parse().unwrap_or(0);

    if user_id != expected_user {
        bot.answer_callback_query(callback_id).text(t(lang, "wrong_user")).show_alert(true).await?;
        return Ok(());
    }

    let player_data = stats(pool, user_id).await.unwrap_or_default();
    if bet > player_data.tokens {
        bot.edit_message_text(chat_id, message_id, t(lang, "invalid_bet")).await?;
        return Ok(());
    }

    charge(pool, user_id, bet).await;
    increment_games_played(pool, user_id).await;

    bot.edit_message_text(chat_id, message_id, "🎲 Rolling...").await?;

    let dice = bot.send_dice(chat_id).emoji(DiceEmoji::Dice).await?;
    let value = dice.dice().map(|d| d.value as i32).unwrap_or(0);
    
    tokio::time::sleep(Duration::from_secs(2)).await;

    if value == guess {
        let winnings = bet * 5;
        let is_new_record = pay(pool, user_id, winnings, biggest_win).await;
        let mut text = t(lang, "dice_win").replace("{winnings}", &winnings.to_string());

        if is_new_record {
            text.push_str(&format!("\n\n{}", t(lang, "achievement_unlocked")));
        }

        bot.send_message(chat_id, text).parse_mode(ParseMode::Html).await?;
    } else {
        let text = t(lang, "dice_lose")
            .replace("{value}", &value.to_string())
            .replace("{bet}", &bet.to_string());
        bot.send_message(chat_id, text).parse_mode(ParseMode::Html).await?;
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

fn bj_status(lang: &str, player: &[Card], dealer: &[Card], show_all: bool) -> String {
    let p_cards: Vec<String> = player.iter().map(card_str).collect();
    let p_score = bj_score(player);
    if show_all {
        let d_cards: Vec<String> = dealer.iter().map(card_str).collect();
        t(lang, "bj_status_all")
            .replace("{p_cards}", &p_cards.join(" "))
            .replace("{p_score}", &p_score.to_string())
            .replace("{d_cards}", &d_cards.join(" "))
            .replace("{d_score}", &bj_score(dealer).to_string())
    } else {
        t(lang, "bj_status_hidden")
            .replace("{p_cards}", &p_cards.join(" "))
            .replace("{p_score}", &p_score.to_string())
            .replace("{d_card}", &card_str(&dealer[0]))
            .replace("{d_score}", &dealer[0].weight.to_string())
    }
}

fn bj_keyboard(lang: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback(t(lang, "bj_btn_hit"), "bj_hit"),
        InlineKeyboardButton::callback(t(lang, "bj_btn_stand"), "bj_stand"),
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

fn poker_status(lang: &str, hand: &[Card], holds: &[bool], finished: bool) -> String {
    let mut out = t(lang, "poker_title");
    for (i, c) in hand.iter().enumerate() {
        let held = holds.get(i).copied().unwrap_or(false) && !finished;
        let held_str = if held { t(lang, "poker_held") } else { "".to_string() };
        
        let card_line = t(lang, "poker_card")
            .replace("{i}", &(i + 1).to_string())
            .replace("{card}", &card_str(c))
            .replace("{held}", &held_str);
            
        out.push_str(&card_line);
    }
    if !finished {
        out.push_str(&t(lang, "poker_prompt"));
    }
    out
}

fn poker_keyboard(lang: &str, holds: &[bool]) -> InlineKeyboardMarkup {
    let row: Vec<InlineKeyboardButton> = holds
        .iter()
        .enumerate()
        .map(|(i, held)| {
            let label = if *held { format!("🔒 C{}", i + 1) } else { format!("🔓 C{}", i + 1) };
            InlineKeyboardButton::callback(label, format!("p_hold_{i}"))
        })
        .collect();
    InlineKeyboardMarkup::new(vec![row, vec![InlineKeyboardButton::callback(t(lang, "poker_btn_draw"), "p_draw")]])
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

fn poker_rank_key(hand: &[Card]) -> &'static str {
    let mut values: Vec<i32> = hand
        .iter()
        .map(|c| match c.label.as_str() {
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
        return "rank_royal_flush";
    }
    if is_flush && is_straight {
        return "rank_straight_flush";
    }
    if counts_vec[0] == 4 {
        return "rank_four_of_a_kind";
    }
    if counts_vec[0] == 3 && counts_vec.get(1) == Some(&2) {
        return "rank_full_house";
    }
    if is_flush {
        return "rank_flush";
    }
    if is_straight {
        return "rank_straight";
    }
    if counts_vec[0] == 3 {
        return "rank_three_of_a_kind";
    }
    if counts_vec[0] == 2 && counts_vec.get(1) == Some(&2) {
        return "rank_two_pair";
    }
    if counts_vec[0] == 2 {
        if let Some((&val, _)) = counts.iter().find(|&(_, &c)| c == 2) {
            if val >= 11 {
                return "rank_jacks_or_better";
            }
        }
    }
    "rank_high_card"
}

// ---------- Player/DB helpers ----------

#[derive(Default)]
struct PlayerStats {
    tokens: i32,
    debt: i32,
    lang: String,
    default_bet: i32,
    last_daily: Option<DateTime<Utc>>,
    games_played: i32,
    biggest_win: i32,
}

async fn ensure_user(pool: &PgPool, user_id: i64, username: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO players (user_id, username, tokens, lang, default_bet) \
         VALUES ($1, $2, 1000, 'en', 0) ON CONFLICT (user_id) DO UPDATE SET username = $2",
    )
    .bind(user_id)
    .bind(username)
    .execute(pool)
    .await?;
    Ok(())
}

async fn stats(pool: &PgPool, user_id: i64) -> Result<PlayerStats, sqlx::Error> {
    let row = sqlx::query(
        "SELECT tokens, debt, lang, default_bet, last_daily, games_played, biggest_win \
         FROM players WHERE user_id = $1"
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    
    Ok(PlayerStats {
        tokens: row.try_get("tokens").unwrap_or(0),
        debt: row.try_get("debt").unwrap_or(0),
        lang: row.try_get("lang").unwrap_or_else(|_| "en".to_string()),
        default_bet: row.try_get("default_bet").unwrap_or(0),
        last_daily: row.try_get("last_daily").unwrap_or(None),
        games_played: row.try_get("games_played").unwrap_or(0),
        biggest_win: row.try_get("biggest_win").unwrap_or(0),
    })
}

async fn check_cooldown(pool: &PgPool, user_id: i64) -> bool {
    let now = Utc::now();
    let row = sqlx::query("SELECT last_action FROM players WHERE user_id = $1")
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .unwrap_or(None);

    if let Some(r) = row {
        if let Ok(last) = r.try_get::<DateTime<Utc>, _>("last_action") {
            if now.signed_duration_since(last).num_seconds() < GLOBAL_COOLDOWN_SECS {
                return false;
            }
        }
    }

    sqlx::query("UPDATE players SET last_action = $1 WHERE user_id = $2")
        .bind(now)
        .bind(user_id)
        .execute(pool)
        .await
        .ok();
        
    true
}

async fn increment_games_played(pool: &PgPool, user_id: i64) {
    sqlx::query("UPDATE players SET games_played = games_played + 1 WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .ok();
}

async fn resolve_bet(pool: &PgPool, user_id: i64, arg: Option<&str>, default_bet: i32) -> Result<i32, &'static str> {
    let bet = match arg {
        Some(s) => s.parse::<i32>().unwrap_or(0),
        None => default_bet,
    };

    if bet <= 0 {
        return Err("missing_bet");
    }
    
    if bet < MIN_BET || bet > MAX_BET {
        return Err("bet_out_of_bounds");
    }

    let stats_data = stats(pool, user_id).await.map_err(|_| "invalid_bet")?;
    if bet > stats_data.tokens {
        return Err("invalid_bet");
    }
    
    Ok(bet)
}

async fn charge(pool: &PgPool, user_id: i64, amount: i32) {
    sqlx::query("UPDATE players SET tokens = tokens - $1 WHERE user_id = $2")
        .bind(amount)
        .bind(user_id)
        .execute(pool)
        .await
        .ok();
}

async fn pay(pool: &PgPool, user_id: i64, amount: i32, current_biggest_win: i32) -> bool {
    if amount == 0 {
        return false;
    }
    
    let mut is_record = false;
    
    if amount > current_biggest_win {
        is_record = true;
        sqlx::query("UPDATE players SET tokens = tokens + $1, biggest_win = $1 WHERE user_id = $2")
            .bind(amount)
            .bind(user_id)
            .execute(pool)
            .await
            .ok();
    } else {
        sqlx::query("UPDATE players SET tokens = tokens + $1 WHERE user_id = $2")
            .bind(amount)
            .bind(user_id)
            .execute(pool)
            .await
            .ok();
    }
    
    is_record
}