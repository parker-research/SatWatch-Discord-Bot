// sat-discord-bot: Discord bot for satellite pass predictions
// Uses satkit for SGP4 propagation + SatNOGS for live TLE data.

mod passes;
mod satnogs;

use anyhow::Result;
use serenity::all::{
    Command, CommandDataOptionValue, CommandInteraction, CommandOptionType, CreateCommand,
    CreateCommandOption, CreateInteractionResponse, CreateInteractionResponseMessage,
    EditInteractionResponse, GuildId, Interaction,
};
use serenity::async_trait;
use serenity::model::gateway::Ready;
use serenity::prelude::*;
use std::env;
use tracing::{error, info};

use passes::{GroundStation, find_passes};

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!("{} is connected!", ready.user.name);

        // Register slash commands globally (or guild-scoped for testing).
        // Guild-scoped is instant; global can take up to 1 hour to propagate.
        // Set DISCORD_GUILD_ID in .env for fast testing, leave unset for global.
        if let Ok(guild_id_str) = env::var("DISCORD_GUILD_ID") {
            let guild_id = GuildId::new(
                guild_id_str
                    .parse()
                    .expect("DISCORD_GUILD_ID must be a u64"),
            );
            guild_id
                .set_commands(&ctx.http, build_commands())
                .await
                .expect("Failed to register guild commands");
            info!("Slash commands registered for guild {guild_id}");
        } else {
            Command::set_global_commands(&ctx.http, build_commands())
                .await
                .expect("Failed to register global commands");
            info!("Global slash commands registered (may take up to 1 hour to appear)");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        if let Interaction::Command(command) = interaction {
            let result = match command.data.name.as_str() {
                "passes" => handle_passes(&ctx, &command).await,
                "tle" => handle_tle(&ctx, &command).await,
                _ => {
                    send_reply(&ctx, &command, "❓ Unknown command.").await;
                    return;
                }
            };

            if let Err(e) = result {
                error!("Error handling command '{}': {e:#}", command.data.name);
                send_reply(&ctx, &command, &format!("⚠️ Error: {e}")).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Slash command registration
// ---------------------------------------------------------------------------

fn build_commands() -> Vec<CreateCommand> {
    vec![
        CreateCommand::new("passes")
            .description("Predict upcoming satellite passes over a ground station")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::Integer,
                    "norad_id",
                    "NORAD catalog number (e.g. 69015 for FrontierSat)",
                )
                .required(true),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::Number,
                    "lat",
                    "Ground station latitude in decimal degrees (e.g. 51.5)",
                )
                .required(true),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::Number,
                    "lon",
                    "Ground station longitude in decimal degrees (e.g. -114.0)",
                )
                .required(true),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::Number,
                    "alt_m",
                    "Ground station altitude in metres above ellipsoid (default 0)",
                )
                .required(false),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::Integer,
                    "hours",
                    "How many hours ahead to search (default 24, max 72)",
                )
                .required(false),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::Number,
                    "min_elev",
                    "Minimum peak elevation in degrees to report (default 5)",
                )
                .required(false),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "station_name",
                    "Label for the ground station (optional)",
                )
                .required(false),
            ),
        CreateCommand::new("tle")
            .description("Fetch and display the current TLE from SatNOGS for a satellite")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::Integer,
                    "norad_id",
                    "NORAD catalog number (e.g. 69015)",
                )
                .required(true),
            ),
    ]
}

// ---------------------------------------------------------------------------
// Helper – send a plain text reply (deferred not needed for fast responses)
// ---------------------------------------------------------------------------

async fn send_reply(ctx: &Context, command: &CommandInteraction, content: &str) {
    let response = CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new().content(content),
    );
    if let Err(e) = command.create_response(&ctx.http, response).await {
        error!("Failed to send reply: {e}");
    }
}

// ---------------------------------------------------------------------------
// /tle handler
// ---------------------------------------------------------------------------

async fn handle_tle(ctx: &Context, command: &CommandInteraction) -> Result<()> {
    let norad_id = get_option_i64(command, "norad_id").unwrap_or(0) as u64;

    // Defer the reply so Discord doesn't time out while we fetch
    command
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new()),
        )
        .await?;

    match satnogs::fetch_tle(norad_id).await {
        Ok(tle_info) => {
            let msg = format!(
                "**{}** (NORAD {norad_id}) — TLE from SatNOGS\n\
                 Updated: {}\n\
                 ```\n{}\n{}\n```",
                tle_info.name, tle_info.updated, tle_info.line1, tle_info.line2,
            );
            command
                .edit_response(&ctx.http, EditInteractionResponse::new().content(msg))
                .await?;
        }
        Err(e) => {
            command
                .edit_response(
                    &ctx.http,
                    EditInteractionResponse::new()
                        .content(format!("⚠️ Could not fetch TLE for NORAD {norad_id}: {e}")),
                )
                .await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// /passes handler
// ---------------------------------------------------------------------------

async fn handle_passes(ctx: &Context, command: &CommandInteraction) -> Result<()> {
    let norad_id = get_option_i64(command, "norad_id").unwrap_or(0) as u64;
    let lat = get_option_f64(command, "lat").unwrap_or(0.0);
    let lon = get_option_f64(command, "lon").unwrap_or(0.0);
    let alt_m = get_option_f64(command, "alt_m").unwrap_or(0.0);
    let hours = get_option_i64(command, "hours").unwrap_or(24).clamp(1, 72) as u64;
    let min_elev = get_option_f64(command, "min_elev").unwrap_or(5.0);
    let station_name = get_option_str(command, "station_name")
        .unwrap_or_else(|| format!("{lat:.2}°N, {lon:.2}°E"));

    // Defer immediately — orbit propagation + HTTP fetch may take a second
    command
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new()),
        )
        .await?;

    // Fetch TLE from SatNOGS
    let tle_info = match satnogs::fetch_tle(norad_id).await {
        Ok(t) => t,
        Err(e) => {
            command
                .edit_response(
                    &ctx.http,
                    EditInteractionResponse::new()
                        .content(format!("⚠️ Could not fetch TLE for NORAD {norad_id}: {e}")),
                )
                .await?;
            return Ok(());
        }
    };

    let gs = GroundStation {
        name: station_name.clone(),
        lat_deg: lat,
        lon_deg: lon,
        alt_m,
    };

    // Run pass prediction (sync, but fast enough for tokio::task::spawn_blocking)
    let tle_clone = tle_info.clone();
    let gs_clone = gs.clone();
    let passes = tokio::task::spawn_blocking(move || {
        find_passes(
            &tle_clone.line1,
            &tle_clone.line2,
            &[gs_clone],
            hours,
            min_elev,
        )
    })
    .await??;

    // Build reply
    let mut reply = format!(
        "🛰️ **{}** (NORAD {norad_id})\n📡 Station: **{}** ({lat:.4}°, {lon:.4}°, {alt_m:.0} m)\n\
         🕐 Next **{hours}h** | min elevation **{min_elev:.0}°**\n\
         TLE updated: {}\n\n",
        tle_info.name, station_name, tle_info.updated
    );

    if passes.is_empty() {
        reply.push_str("No passes above the minimum elevation threshold in the search window.");
    } else {
        reply.push_str(&format!("Found **{}** pass(es):\n\n", passes.len()));
        for (i, p) in passes.iter().enumerate() {
            reply.push_str(&format!(
                "**Pass {}** over {} — {}\n\
                 • AOS: {} (elev {:.1}°, az {:.0}°)\n\
                 • MAX: {} (elev **{:.1}°**, az {:.0}°)\n\
                 • LOS: {} (elev {:.1}°, az {:.0}°)\n\
                 • Duration: **{}m {}s** above {}°\n\n",
                i + 1,
                p.station_name,
                p.aos_utc.format("%Y-%m-%d"),
                p.aos_utc.format("%H:%M:%S UTC"),
                p.aos_elev_deg,
                p.aos_az_deg,
                p.max_utc.format("%H:%M:%S UTC"),
                p.max_elev_deg,
                p.max_az_deg,
                p.los_utc.format("%H:%M:%S UTC"),
                p.los_elev_deg,
                p.los_az_deg,
                p.duration_secs / 60,
                p.duration_secs % 60,
                min_elev,
            ));

            // Discord message limit is 2000 characters
            if reply.len() > 1700 && i + 1 < passes.len() {
                reply.push_str(&format!(
                    "*…and {} more pass(es). Narrow your search window or raise min elevation.*",
                    passes.len() - i - 1
                ));
                break;
            }
        }
    }

    command
        .edit_response(&ctx.http, EditInteractionResponse::new().content(&reply))
        .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Option extraction helpers
// ---------------------------------------------------------------------------

fn get_option_i64(cmd: &CommandInteraction, name: &str) -> Option<i64> {
    cmd.data
        .options
        .iter()
        .find(|o| o.name == name)
        .and_then(|o| {
            if let CommandDataOptionValue::Integer(v) = &o.value {
                Some(*v)
            } else {
                None
            }
        })
}

fn get_option_f64(cmd: &CommandInteraction, name: &str) -> Option<f64> {
    cmd.data
        .options
        .iter()
        .find(|o| o.name == name)
        .and_then(|o| {
            if let CommandDataOptionValue::Number(v) = &o.value {
                Some(*v)
            } else {
                None
            }
        })
}

fn get_option_str(cmd: &CommandInteraction, name: &str) -> Option<String> {
    cmd.data
        .options
        .iter()
        .find(|o| o.name == name)
        .and_then(|o| {
            if let CommandDataOptionValue::String(v) = &o.value {
                Some(v.clone())
            } else {
                None
            }
        })
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if present
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt::init();

    let token = env::var("DISCORD_TOKEN").expect("Expected DISCORD_TOKEN in environment");

    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES;

    let mut client = Client::builder(&token, intents)
        .event_handler(Handler)
        .await?;

    info!("Starting sat-discord-bot…");
    client.start().await?;

    Ok(())
}
