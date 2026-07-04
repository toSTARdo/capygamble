use std::env;
use sqlx::{PgPool, Pool, Postgres};
use teloxide::{prelude::*, types::DiceEmoji};

#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    log::info!("Starting CapyGamble Bot...");

    // 1. Connect to Neon DB
    let database_url = env::var("postgresql://neondb_owner:npg_mBSfzipZ6J3u@ep-gentle-leaf-at860ut4-pooler.c-9.us-east-1.aws.neon.tech/neondb?sslmode=require&channel_binding=require")
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

            ensure_user_exists(&pool, user_id, &username).await?;

            if let Some(text) = msg.text() {
                match text {
                    "/start" | "/balance" => {
                        let tokens = get_balance(&pool, user_id).await?;
                        bot.send_message(msg.chat.id, format!("Welcome to Capy Casino! 🎰\nYour current balance: {tokens} tokens."))
                            .await?;
                    }
                    "/spin" => {
                        let mut tokens = get_balance(&pool, user_id).await?;
                        if tokens < 10 {
                            bot.send_message(msg.chat.id, "❌ You need at least 10 tokens to spin!").await?;
                            return respond(());
                        }

                        tokens -= 10;
                        update_balance(&pool, user_id, tokens).await?;

                        let dice_msg = bot.send_dice(msg.chat.id)
                            .emoji(DiceEmoji::SlotMachine)
                            .await?;

                        if let Some(teloxide::types::Dice { value, .. }) = dice_msg.dice {
                            // Telegram determines values 1 to 64 for 🎰
                            // Jackpot conditions depend on specific outcomes (e.g., 1=Bar, 22=Grape, 43=Lemon, 64=777)
                            let winnings = match value {
                                64 => 500, // Jackpot (777)
                                1 | 22 | 43 => 100, // Three of a kind 
                                _ => 0,
                            };

                            tokens += winnings;
                            update_balance(&pool, user_id, tokens).await?;

                            tokio::time::sleep(std::time::Duration::from_secs(2)).await; // Wait for spin animation

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
    sqlx::query!(
        "INSERT INTO players (user_id, username) VALUES ($1, $2) ON CONFLICT (user_id) DO NOTHING",
        user_id,
        username
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn get_balance(pool: &Pool<Postgres>, user_id: i64) -> Result<i32, sqlx::Error> {
    let row = sqlx::query!("SELECT tokens FROM players WHERE user_id = $1", user_id)
        .fetch_one(pool)
        .await?;
    Ok(row.tokens)
}

async fn update_balance(pool: &Pool<Postgres>, user_id: i64, new_balance: i32) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE players SET tokens = $1 WHERE user_id = $2",
        new_balance,
        user_id
    )
    .execute(pool)
    .await?;
    Ok(())
}