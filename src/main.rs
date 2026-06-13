// satwatch-discord-bot: Discord bot for satellite pass predictions
// Uses satkit for SGP4 propagation + SatNOGS for live TLE data.

mod db;
mod discord_interactions;
mod passes;
mod satnogs;

use db::Database;
use discord_interactions::{DatabaseKey, Handler, HttpKey, run_new_tle_check, run_strikethrough_check};

use anyhow::Result;
use serenity::all::GatewayIntents;
use serenity::prelude::*;
use std::env;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};

const NEW_TLE_POLLING_INTERVAL: Duration = Duration::from_secs(300);
const STRIKETHROUGH_CHECK_INTERVAL: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if present.
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt::init();

    let token = env::var("DISCORD_TOKEN").expect("Expected DISCORD_TOKEN in environment");

    let db_path = env::var("DATABASE_PATH").unwrap_or_else(|_| "satwatch.db".to_string());
    let db = Arc::new(Database::open(&db_path)?);
    info!("Database opened at {db_path}");

    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES;

    let mut client = Client::builder(&token, intents)
        .event_handler(Handler)
        .await?;

    {
        let mut data = client.data.write().await;
        data.insert::<DatabaseKey>(db.clone());
    }

    // Background pass-check loop.
    // Waits for the bot to connect (ready() stores HttpKey), then runs
    // immediately and repeats every 10 minutes.
    let data_handle = client.data.clone();
    let db_bg = db.clone();
    tokio::spawn(async move {
        // Poll until ready() has stored the Http handle.
        let http = loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let data = data_handle.read().await;
            if let Some(h) = data.get::<HttpKey>() {
                break h.clone();
            }
        };
        info!("Background fresh TLE checker started");

        loop {
            match run_new_tle_check(&http, &db_bg).await {
                Ok(0) => {}
                Ok(n) => info!("New TLE check: announced {n} new overpasses"),
                Err(e) => error!("New TLE check error: {e:#}"),
            }
            tokio::time::sleep(NEW_TLE_POLLING_INTERVAL).await;
        }
    });

    // Background strikethrough loop.
    // Edits pass messages to add strikethrough once LOS has passed.
    let data_handle2 = client.data.clone();
    let db_bg2 = db.clone();
    tokio::spawn(async move {
        let http = loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let data = data_handle2.read().await;
            if let Some(h) = data.get::<HttpKey>() {
                break h.clone();
            }
        };
        info!("Background strikethrough checker started");

        loop {
            match run_strikethrough_check(&http, &db_bg2).await {
                Ok(0) => {}
                Ok(n) => info!("Strikethrough check: struck out {n} expired pass message(s)"),
                Err(e) => error!("Strikethrough check error: {e:#}"),
            }
            tokio::time::sleep(STRIKETHROUGH_CHECK_INTERVAL).await;
        }
    });

    info!("Starting satwatch-discord-bot…");
    client.start().await?;

    Ok(())
}
