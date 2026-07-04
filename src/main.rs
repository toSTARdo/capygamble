use std::env;
use sqlx::{PgPool, Pool, Postgres};
use teloxide::{prelude::*, types::DiceEmoji};

#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    log::info!("Starting CapyGamble Bot...");

    // 1. Connect to Neon DB
    let database_url = env::var("DATABASE_URL")
        .expect("DATABASE_URL environment variable must be set");
    
    let pool = PgPool::connect(&database_url)
        .await
        .expect("Failed to connect to Neon DB");

    // 2. Initialize Bot
    let bot = Bot::from_env();

    teloxide::repl(bot, move |bot: Bot, msg: Message| {
        let pool = pool.clone();
        async move {
            let user = match msg.from() {
                Some(u) => u,
                None => return respond(()),
            };

            let user_id = user.id.0 as i64;
            let username = user.username.clone().unwrap_or_else(|| "Anonymous".to_string());

            // 1. Explicitly catch and log SQLx errors so they don't crash the Teloxide pipeline
            if let Err(e) = ensure_user_exists(&pool, user_id, &username).await {
                log::error!("Database error ensuring user exists: {:?}", e);
                return respond(());
            }

            if let Some(text) = msg.text() {
                match text {
                    "/start" | "/balance" => {
                        let tokens = match get_balance(&pool, user_id).await {
                            Ok(t) => t,
                            Err(e) => {
                                log::error!("Database error fetching balance: {:?}", e);
                                return respond(());
                            }
                        };
                        bot.send_message(msg.chat.id, format!("Welcome to Capy Casino! 🎰\nYour current balance: {tokens} tokens."))
                            .await?;
                    }
                    "/spin" => {
                        let mut tokens = match get_balance(&pool, user_id).await {
                            Ok(t) => t,
                            Err(e) => {
                                log::error!("Database error fetching balance for spin: {:?}", e);
                                return respond(());
                            }
                        };
                        
                        if tokens < 10 {
                            bot.send_message(msg.chat.id, "❌ You need at least 10 tokens to spin!").await?;
                            return respond(());
                        }

                        tokens -= 10;
                        if let Err(e) = update_balance(&pool, user_id, tokens).await {
                            log::error!("Database error deducting tokens: {:?}", e);
                            return respond(());
                        }

                        let dice_msg = bot.send_dice(msg.chat.id)
                            .emoji(DiceEmoji::SlotMachine)
                            .await?;

                        // Fix: Changed dice_msg.dice to dice_msg.dice() method call
                        if let Some(teloxide::types::Dice { value, .. }) = dice_msg.dice() {
                            let winnings = match value {
                                64 => 500, // Jackpot 777!
                                1 | 22 | 43 => 100, // Three of a kind
                                _ => 0,
                            };

                            tokens += winnings;
                            
                            if let Err(e) = update_balance(&pool, user_id, tokens).await {
                                log::error!("Database error awarding winnings: {:?}", e);
                                return respond(());
                            }

                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

                            if winnings > 0 {
                                bot.send_message(msg.chat.id, format!("🎉 JACKPOT! You won {winnings} tokens!\nNew balance: {tokens} tokens."))
                                    .await?;
                            } else {
                                bot.send_message(msg.chat.id, format!("😢 Better luck next time! (Lost 10 tokens)\nNew balance: {tokens} tokens."))
                                    .await?;
                            }
                        }
                    }
                    _ => {
                        bot.send_message(msg.chat.id, "Use /spin to play or /balance to check tokens.").await?;
                    }
                }
            }
            respond(())
        }
    })
    .await;
}

async fn ensure_user_exists(pool: &Pool<Postgres>, user_id: i64, username: &str) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO players (user_id, username) VALUES ($1, $2) ON CONFLICT (user_id) DO NOTHING")
        .bind(user_id)
        .bind(username)
        .execute(pool)
        .await?;
    Ok(())
}

async fn get_balance(pool: &Pool<Postgres>, user_id: i64) -> Result<i32, sqlx::Error> {
    use sqlx::Row;
    
    let row = sqlx::query("SELECT tokens FROM players WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await?;
        
    Ok(row.get("tokens"))
}

async fn update_balance(pool: &Pool<Postgres>, user_id: i64, new_balance: i32) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE players SET tokens = $1 WHERE user_id = $2")
        .bind(new_balance)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}