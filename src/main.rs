use std::env;
use rand::{thread_rng, Rng};
use rand::seq::SliceRandom;
use sqlx::{PgPool, Pool, Postgres, Row};
use teloxide::{
    prelude::*,
    types::{DiceEmoji, InlineKeyboardButton, InlineKeyboardMarkup, ParseMode, Update},
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Card {
    suit: String,
    value: String,
    weight: i32,
}

impl Card {
    fn new(suit: &str, value: &str, weight: i32) -> Self {
        Self {
            suit: suit.to_string(),
            value: value.to_string(),
            weight,
        }
    }
}

fn create_deck() -> Vec<Card> {
    let suits = vec!["♠️", "♥️", "♦️", "♣️"];
    let values = vec![
        ("2", 2), ("3", 3), ("4", 4), ("5", 5), ("6", 6), ("7", 7),
        ("8", 8), ("9", 9), ("10", 10), ("J", 10), ("Q", 10), ("K", 10), ("A", 11),
    ];
    let mut deck = Vec::new();
    for suit in &suits {
        for (val, weight) in &values {
            deck.push(Card::new(suit, val, *weight));
        }
    }
    deck.shuffle(&mut thread_rng());
    deck
}

#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    let database_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = PgPool::connect(&database_url).await.expect("Failed to connect to DB");
    let bot = Bot::from_env();

    // Use dptree::entry() and branch your handlers
    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_callback_query().endpoint(handle_callback));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![pool])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

// --- MESSAGE HANDLER (Commands) ---
async fn handle_message(bot: Bot, msg: Message, pool: PgPool) -> ResponseResult<()> {
    let user = match msg.from() {
        Some(u) => u,
        None => return Ok(()),
    };
    let user_id = user.id.0 as i64;
    let username = user.username.clone().unwrap_or_else(|| "Anonymous".to_string());

    if let Err(e) = ensure_user_exists(&pool, user_id, &username).await {
        log::error!("DB error: {:?}", e);
        return Ok(());
    }

    let text = match msg.text() {
        Some(t) => t,
        None => return Ok(()),
    };

    let mut args = text.split_whitespace();
    let command = args.next().unwrap_or("");

    match command {
        "/start" | "/balance" => {
            let (tokens, debt) = get_user_stats(&pool, user_id).await.unwrap_or((0, 0));
            let welcome = format!(
                "🎰 *Welcome to Capy Casino\\!* 🎰\n\n💰 Balance: *{} tokens*\n💸 Debt: *{} tokens*\n\n*Commands:*\n/spin <bet> \\- Slots\n/flip <heads\\|tails> <bet> \\- Coin Flip\n/dice <1\\-6> <bet> \\- Dice roll\n/blackjack <bet> \\- Play BJ\n/poker <bet> \\- Video Poker\n/faucet \\- Take a loan\n/networth \\- View Global Metrics",
                tokens, debt
            );
            bot.send_message(msg.chat_id(), welcome).parse_mode(ParseMode::MarkdownV2).await?;
        }

        "/faucet" => {
            let (tokens, _debt) = get_user_stats(&pool, user_id).await.unwrap_or((0, 0));
            if tokens < 50 {
                sqlx::query("UPDATE players SET tokens = tokens + 500, debt = debt + 500 WHERE user_id = $1")
                    .bind(user_id).execute(&pool).await.unwrap();
                bot.send_message(msg.chat_id(), "💸 You ran out of tokens! The Casino issued you a **500 token loan**.\n⚠️ This has been added to your /networth debt tracking!").await?;
            } else {
                bot.send_message(msg.chat_id(), "🛑 You still have tokens! Faucet loans are only for broke capybaras (< 50 tokens).").await?;
            }
        }

        "/networth" => {
            let row = sqlx::query("SELECT SUM(tokens)::BIGINT as total_tokens, SUM(debt)::BIGINT as total_debt FROM players")
                .fetch_one(&pool).await.unwrap();
            let total_tokens: i64 = row.try_get("total_tokens").unwrap_or(0);
            let total_debt: i64 = row.try_get("total_debt").unwrap_or(0);
            
            // Casino Net Worth = Total loans issued + losses collected - cash currently held by players
            let casino_networth = total_debt - total_tokens;

            let response = format!(
                "🏢 **CAPY CASINO BALANCE SHEET** 🏢\n\n🎯 Total Player Liquidity: {} tokens\n📈 Total Outstanding Player Debt: {} tokens\n\n🏦 **Total Casino Net Worth:** {} tokens",
                total_tokens, total_debt, casino_networth
            );
            bot.send_message(msg.chat_id(), response).await?;
        }

        "/spin" => {
            let bet: i32 = args.next().unwrap_or("10").parse().unwrap_or(0);
            let (tokens, _) = get_user_stats(&pool, user_id).await.unwrap_or((0, 0));
            if bet <= 0 || bet > tokens {
                bot.send_message(msg.chat_id(), "❌ Invalid bet or insufficient funds!").await?;
                return Ok(());
            }

            sqlx::query("UPDATE players SET tokens = tokens - $1 WHERE user_id = $2").bind(bet).bind(user_id).execute(&pool).await.unwrap();
            let dice_msg = bot.send_dice(msg.chat_id()).emoji(DiceEmoji::SlotMachine).await?;

            if let Some(teloxide::types::Dice { value, .. }) = dice_msg.dice() {
                let mult = match value { 64 => 10, 1 | 22 | 43 => 3, _ => 0 };
                let winnings = bet * mult;
                sqlx::query("UPDATE players SET tokens = tokens + $1 WHERE user_id = $2").bind(winnings).bind(user_id).execute(&pool).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                if winnings > 0 {
                    bot.send_message(msg.chat_id(), format!("🎉 WINNER! You won {winnings} tokens!")).await?;
                } else {
                    bot.send_message(msg.chat_id(), format!("😢 Lost {bet} tokens.")).await?;
                }
            }
        }

        "/flip" => {
            let choice = args.next().unwrap_or("").to_lowercase();
            let bet: i32 = args.next().unwrap_or("10").parse().unwrap_or(0);
            let (tokens, _) = get_user_stats(&pool, user_id).await.unwrap_or((0, 0));

            if (choice != "heads" && choice != "tails") || bet <= 0 || bet > tokens {
                bot.send_message(msg.chat_id(), "❌ Usage: /flip <heads|tails> <bet>").await?;
                return Ok(());
            }

            sqlx::query("UPDATE players SET tokens = tokens - $1 WHERE user_id = $2").bind(bet).bind(user_id).execute(&pool).await.unwrap();
            let won = (thread_rng().gen_bool(0.5) && choice == "heads") || (!thread_rng().gen_bool(0.5) && choice == "tails");
            
            let winnings = if won { bet * 2 } else { 0 };
            sqlx::query("UPDATE players SET tokens = tokens + $1 WHERE user_id = $2").bind(winnings).bind(user_id).execute(&pool).await.unwrap();
            bot.send_message(msg.chat_id(), if won { format!("🎉 Won {winnings} tokens!") } else { format!("💸 Lost {bet} tokens.") }).await?;
        }

        "/dice" => {
            let guess: i32 = args.next().unwrap_or("0").parse().unwrap_or(0);
            let bet: i32 = args.next().unwrap_or("10").parse().unwrap_or(0);
            let (tokens, _) = get_user_stats(&pool, user_id).await.unwrap_or((0, 0));

            if guess < 1 || guess > 6 || bet <= 0 || bet > tokens {
                bot.send_message(msg.chat_id(), "❌ Usage: /dice <1-6> <bet>").await?;
                return Ok(());
            }

            sqlx::query("UPDATE players SET tokens = tokens - $1 WHERE user_id = $2").bind(bet).bind(user_id).execute(&pool).await.unwrap();
            let dice_msg = bot.send_dice(msg.chat_id()).emoji(DiceEmoji::Dice).await?;

            if let Some(teloxide::types::Dice { value, .. }) = dice_msg.dice() {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                // Fix: Cast value (u8) to i32 to match guess (i32)
                if (*value as i32) == guess {
                    let winnings = bet * 5;
                    sqlx::query("UPDATE players SET tokens = tokens + $1 WHERE user_id = $2").bind(winnings).bind(user_id).execute(&pool).await.unwrap();
                    bot.send_message(msg.chat_id(), format!("🎯 Match! Won {winnings} tokens!")).await?;
                } else {
                    bot.send_message(msg.chat_id(), format!("🎲 Landed on {value}. Lost {bet} tokens.")).await?;
                }
            }
        }

        "/blackjack" => {
            let bet: i32 = args.next().unwrap_or("50").parse().unwrap_or(0);
            let (tokens, _) = get_user_stats(&pool, user_id).await.unwrap_or((0, 0));
            if bet <= 0 || bet > tokens {
                bot.send_message(msg.chat_id(), "❌ Insufficient funds or invalid bet!").await?;
                return Ok(());
            }

            sqlx::query("UPDATE players SET tokens = tokens - $1 WHERE user_id = $2").bind(bet).bind(user_id).execute(&pool).await.unwrap();
            let mut deck = create_deck();
            let p_hand = vec![deck.pop().unwrap(), deck.pop().unwrap()];
            let d_hand = vec![deck.pop().unwrap(), deck.pop().unwrap()];

            sqlx::query("INSERT INTO blackjack_games (user_id, bet, player_hand, dealer_hand, deck) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (user_id) DO UPDATE SET bet=$2, player_hand=$3, dealer_hand=$4, deck=$5")
                .bind(user_id).bind(bet).bind(serde_json::to_string(&p_hand).unwrap()).bind(serde_json::to_string(&d_hand).unwrap()).bind(serde_json::to_string(&deck).unwrap()).execute(&pool).await.unwrap();

            let text = format_bj_status(&p_hand, &d_hand, false);
            let keyboard = InlineKeyboardMarkup::new(vec![vec![
                InlineKeyboardButton::callback("🟢 Hit", "bj_hit"),
                InlineKeyboardButton::callback("🛑 Stand", "bj_stand"),
            ]]);

            bot.send_message(msg.chat_id(), text).reply_markup(keyboard).await?;
        }

        "/poker" => {
            let bet: i32 = args.next().unwrap_or("50").parse().unwrap_or(0);
            let (tokens, _) = get_user_stats(&pool, user_id).await.unwrap_or((0, 0));
            if bet <= 0 || bet > tokens {
                bot.send_message(msg.chat_id(), "❌ Insufficient funds or invalid bet!").await?;
                return Ok(());
            }

            sqlx::query("UPDATE players SET tokens = tokens - $1 WHERE user_id = $2").bind(bet).bind(user_id).execute(&pool).await.unwrap();
            let mut deck = create_deck();
            let hand = vec![deck.pop().unwrap(), deck.pop().unwrap(), deck.pop().unwrap(), deck.pop().unwrap(), deck.pop().unwrap()];
            let holds = vec![false; 5];

            sqlx::query("INSERT INTO poker_games (user_id, bet, hand, holds, deck) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (user_id) DO UPDATE SET bet=$2, hand=$3, holds=$4, deck=$5")
                .bind(user_id).bind(bet).bind(serde_json::to_string(&hand).unwrap()).bind(serde_json::to_string(&holds).unwrap()).bind(serde_json::to_string(&deck).unwrap()).execute(&pool).await.unwrap();

            let text = format_poker_status(&hand, &holds, false, 0);
            let keyboard = make_poker_keyboard(&holds);

            bot.send_message(msg.chat_id(), text).reply_markup(keyboard).await?;
        }

        _ => {}
    }
    Ok(())
}

// --- CALLBACK HANDLER (Button interactions) ---
async fn handle_callback(bot: Bot, q: CallbackQuery, pool: PgPool) -> ResponseResult<()> {
    let user_id = q.from.id.0 as i64;
    // Fix: Use .as_ref() to avoid moving q.data
    let data = q.data.as_deref().unwrap_or_default();
    
    // Fix: Access the message without moving q
    let msg = match q.message.as_ref() {
        Some(m) => m,
        None => return Ok(()),
    };

    if data.starts_with("bj_") {
        let game_row = match sqlx::query("SELECT * FROM blackjack_games WHERE user_id = $1").bind(user_id).fetch_optional(&pool).await.unwrap() {
            Some(r) => r,
            None => return Ok(()),
        };
        let bet: i32 = game_row.get("bet");
        let mut p_hand: Vec<Card> = serde_json::from_str(game_row.get("player_hand")).unwrap();
        let mut d_hand: Vec<Card> = serde_json::from_str(game_row.get("dealer_hand")).unwrap();
        let mut deck: Vec<Card> = serde_json::from_str(game_row.get("deck")).unwrap();

        if data == "bj_hit" {
            p_hand.push(deck.pop().unwrap());
            let score = calc_bj_score(&p_hand);
            if score > 21 {
                sqlx::query("DELETE FROM blackjack_games WHERE user_id = $1").bind(user_id).execute(&pool).await.unwrap();
                let txt = format!("{}\n\n💥 **BUST! You went over 21. Lost {} tokens.**", format_bj_status(&p_hand, &d_hand, true), bet);
                bot.edit_message_text(msg.chat_id(), msg.id, txt).await?;
            } else {
                sqlx::query("UPDATE blackjack_games SET player_hand=$1, deck=$2 WHERE user_id=$3")
                    .bind(serde_json::to_string(&p_hand).unwrap()).bind(serde_json::to_string(&deck).unwrap()).bind(user_id).execute(&pool).await.unwrap();
                let keyboard = InlineKeyboardMarkup::new(vec![vec![
                    InlineKeyboardButton::callback("🟢 Hit", "bj_hit"),
                    InlineKeyboardButton::callback("🛑 Stand", "bj_stand"),
                ]]);
                bot.edit_message_text(msg.chat_id(), msg.id, format_bj_status(&p_hand, &d_hand, false)).reply_markup(keyboard).await?;
            }
        } else if data == "bj_stand" {
            while calc_bj_score(&d_hand) < 17 {
                d_hand.push(deck.pop().unwrap());
            }
            let p_score = calc_bj_score(&p_hand);
            let d_score = calc_bj_score(&d_hand);
            sqlx::query("DELETE FROM blackjack_games WHERE user_id = $1").bind(user_id).execute(&pool).await.unwrap();

            let outcome = if d_score > 21 || p_score > d_score {
                sqlx::query("UPDATE players SET tokens = tokens + $1 WHERE user_id = $2").bind(bet * 2).bind(user_id).execute(&pool).await.unwrap();
                format!("🎉 **YOU WIN! Won {} tokens!**", bet)
            } else if p_score == d_score {
                sqlx::query("UPDATE players SET tokens = tokens + $1 WHERE user_id = $2").bind(bet).bind(user_id).execute(&pool).await.unwrap();
                "🤝 **Push! Bet returned.**".to_string()
            } else {
                format!("💸 **Dealer wins. Lost {} tokens.**", bet)
            };

            let txt = format!("{}\n\n{}", format_bj_status(&p_hand, &d_hand, true), outcome);
            bot.edit_message_text(msg.chat_id(), msg.id, txt).await?;
        }
    } else if data.starts_with("p_") {
        let game_row = match sqlx::query("SELECT * FROM poker_games WHERE user_id = $1").bind(user_id).fetch_optional(&pool).await.unwrap() {
            Some(r) => r,
            None => return Ok(()),
        };
        let bet: i32 = game_row.get("bet");
        let mut hand: Vec<Card> = serde_json::from_str(game_row.get("hand")).unwrap();
        let mut holds: Vec<bool> = serde_json::from_str(game_row.get("holds")).unwrap();
        let mut deck: Vec<Card> = serde_json::from_str(game_row.get("deck")).unwrap();

        if data.starts_with("p_hold_") {
            let idx: usize = data.split('_').last().unwrap().parse().unwrap();
            holds[idx] = !holds[idx];
            sqlx::query("UPDATE poker_games SET holds = $1 WHERE user_id = $2").bind(serde_json::to_string(&holds).unwrap()).bind(user_id).execute(&pool).await.unwrap();
            bot.edit_message_text(msg.chat_id(), msg.id, format_poker_status(&hand, &holds, false, 0)).reply_markup(make_poker_keyboard(&holds)).await?;
        } else if data == "p_draw" {
            sqlx::query("DELETE FROM poker_games WHERE user_id = $1").bind(user_id).execute(&pool).await.unwrap();
            for i in 0..5 {
                if !holds[i] { hand[i] = deck.pop().unwrap(); }
            }
            let rank = evaluate_poker_hand(&hand);
            let mult = match rank {
                "Royal Flush" => 250, "Straight Flush" => 50, "Four of a Kind" => 25,
                "Full House" => 9, "Flush" => 6, "Straight" => 4, "Three of a Kind" => 3,
                "Two Pair" => 2, "Jacks or Better" => 1, _ => 0,
            };
            let winnings = bet * mult;
            sqlx::query("UPDATE players SET tokens = tokens + $1 WHERE user_id = $2").bind(winnings).bind(user_id).execute(&pool).await.unwrap();
            let mut txt = format_poker_status(&hand, &holds, true, winnings);
            if winnings > 0 { txt.push_str(&format!("\n\n🎉 **🏆 {}! Won {} tokens!**", rank, winnings)); }
            else { txt.push_str(&format!("\n\n💸 **{}! Lost {} tokens.**", rank, bet)); }
            bot.edit_message_text(msg.chat_id(), msg.id, txt).await?;
        }
    }
    bot.answer_callback_query(q.id).await?;
    Ok(())
}

// --- UTILITY LOGIC HELPER FUNCTIONS ---
fn calc_bj_score(hand: &[Card]) -> i32 {
    let mut score = 0; let mut aces = 0;
    for c in hand { score += c.weight; if c.value == "A" { aces += 1; } }
    while score > 21 && aces > 0 { score -= 10; aces -= 1; }
    score
}

fn format_bj_status(p_hand: &[Card], d_hand: &[Card], show_all: bool) -> String {
    let p_cards: Vec<String> = p_hand.iter().map(|c| format!("`{}{}`", c.suit, c.value)).collect();
    let p_score = calc_bj_score(p_hand);
    if show_all {
        let d_cards: Vec<String> = d_hand.iter().map(|c| format!("`{}{}`", c.suit, c.value)).collect();
        format!("🃏 **CASINO BLACKJACK** 🃏\n\n👤 **Your Hand:** {} (Total: {})\n🤖 **Dealer Hand:** {} (Total: {})", p_cards.join(" "), p_score, d_cards.join(" "), calc_bj_score(d_hand))
    } else {
        format!("🃏 **CASINO BLACKJACK** 🃏\n\n👤 **Your Hand:** {} (Total: {})\n🤖 **Dealer Hand:** `{}{}` `📇` (Total: {} + ?)", p_cards.join(" "), p_score, d_hand[0].suit, d_hand[0].value, d_hand[0].weight)
    }
}

fn format_poker_status(hand: &[Card], holds: &[bool], finished: bool, _winnings: i32) -> String {
    let mut display = "🃏 **VIDEO POKER (FIVE-CARD DRAW)** 🃏\n\n".to_string();
    for i in 0..5 {
        let card_str = format!("`{}{}`", hand[i].suit, hand[i].value);
        let hold_label = if holds[i] && !finished { " [HELD]" } else { "" };
        display.push_str(&format!("Card {}: {}{}\n", i + 1, card_str, hold_label));
    }
    if !finished { display.push_str("\nSelect cards to hold, then click Draw!"); }
    display
}

fn make_poker_keyboard(holds: &[bool]) -> InlineKeyboardMarkup {
    let mut row = vec![];
    for i in 0..5 {
        let txt = if holds[i] { format!("🔒 C{}", i+1) } else { format!("🔓 C{}", i+1) };
        row.push(InlineKeyboardButton::callback(txt, format!("p_hold_{}", i)));
    }
    InlineKeyboardMarkup::new(vec![row, vec![InlineKeyboardButton::callback("💥 DRAW CARDS", "p_draw")]])
}

fn evaluate_poker_hand(hand: &[Card]) -> &'static str {
    let mut values: Vec<i32> = hand.iter().map(|c| match c.value.as_str() { "J" => 11, "Q" => 12, "K" => 13, "A" => 14, _ => c.weight }).collect();
    values.sort_unstable();
    let is_flush = hand.iter().all(|c| c.suit == hand[0].suit);
    let is_straight = values.windows(2).all(|w| w[1] == w[0] + 1) || (values == vec![2, 3, 4, 5, 14]); // Handle low ace straight

    let mut counts = std::collections::HashMap::new();
    for &v in &values { *counts.entry(v).or_insert(0) += 1; }
    let mut counts_vec: Vec<i32> = counts.values().copied().collect();
    counts_vec.sort_unstable_by(|a, b| b.cmp(a));

    if is_flush && is_straight && values[4] == 14 && values[0] == 10 { return "Royal Flush"; }
    if is_flush && is_straight { return "Straight Flush"; }
    if counts_vec[0] == 4 { return "Four of a Kind"; }
    if counts_vec[0] == 3 && counts_vec[1] == 2 { return "Full House"; }
    if is_flush { return "Flush"; }
    if is_straight { return "Straight"; }
    if counts_vec[0] == 3 { return "Three of a Kind"; }
    if counts_vec[0] == 2 && counts_vec[1] == 2 { return "Two Pair"; }
    if counts_vec[0] == 2 {
        let pair_val = *counts.iter().find(|&(_, &c)| c == 2).unwrap().0;
        if pair_val >= 11 { return "Jacks or Better"; }
    }
    "High Card"
}

async fn ensure_user_exists(pool: &Pool<Postgres>, user_id: i64, username: &str) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO players (user_id, username, tokens) VALUES ($1, $2, 1000) ON CONFLICT (user_id) DO NOTHING").bind(user_id).bind(username).execute(pool).await?;
    Ok(())
}

async fn get_user_stats(pool: &Pool<Postgres>, user_id: i64) -> Result<(i32, i32), sqlx::Error> {
    let row = sqlx::query("SELECT tokens, debt FROM players WHERE user_id = $1").bind(user_id).fetch_one(pool).await?;
    Ok((row.get("tokens"), row.get("debt")))
}