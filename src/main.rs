// sat-discord-bot: Discord bot for satellite pass predictions
// Uses satkit for SGP4 propagation + SatNOGS for live TLE data.

mod db;
mod passes;
mod satnogs;

use db::Database;
use passes::{GroundStation, Pass, find_passes};

use anyhow::{Result, anyhow};
use serenity::all::{
    ChannelId, Command, CommandDataOption, CommandDataOptionValue, CommandInteraction,
    CommandOptionType, CreateCommand, CreateCommandOption, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateMessage, EditInteractionResponse, GuildId, Interaction,
};
use serenity::async_trait;
use serenity::http::Http;
use serenity::model::gateway::Ready;
use serenity::prelude::*;
use std::env;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

/// Minimum elevation used by the background pass checker.
const CHECK_MIN_ELEV_DEG: f64 = 5.0;

/// Search window for the background pass checker.
const CHECK_HOURS: u64 = 72;

/// How long to keep notified-pass records before pruning them.
const NOTIFIED_PASS_TTL_DAYS: i64 = 7;

// ---------------------------------------------------------------------------
// Serenity shared-state keys
// ---------------------------------------------------------------------------

struct DatabaseKey;
impl TypeMapKey for DatabaseKey {
    type Value = Arc<Database>;
}

// Populated in `ready()`; the background task waits for this to appear.
struct HttpKey;
impl TypeMapKey for HttpKey {
    type Value = Arc<Http>;
}

// ---------------------------------------------------------------------------
// Event handler
// ---------------------------------------------------------------------------

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!("{} is connected!", ready.user.name);

        // Store Http so the background task can send messages.
        {
            let mut data = ctx.data.write().await;
            data.insert::<HttpKey>(ctx.http.clone());
        }

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
                "station" => handle_station(&ctx, &command).await,
                "satellite" => handle_satellite(&ctx, &command).await,
                "set-notify-channel" => handle_set_notify_channel(&ctx, &command).await,
                "upcoming-passes" => handle_upcoming_passes(&ctx, &command).await,
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
        // /passes – ad-hoc pass prediction
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
                    "elevation_m",
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
        // /tle – show raw TLE for a satellite
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
        // /station – manage saved ground stations
        CreateCommand::new("station")
            .description("Manage saved ground stations")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "add",
                    "Save a new ground station",
                )
                .add_sub_option(
                    CreateCommandOption::new(CommandOptionType::String, "name", "Station name")
                        .required(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::Number,
                        "lat",
                        "Latitude in decimal degrees (positive = North)",
                    )
                    .required(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::Number,
                        "lon",
                        "Longitude in decimal degrees (positive = East)",
                    )
                    .required(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::Number,
                        "elevation_m",
                        "Ground elevation above ellipsoid in metres",
                    )
                    .required(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::Number,
                        "altitude_m",
                        "Height above ground in metres",
                    )
                    .required(true),
                ),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "remove",
                    "Delete a saved ground station",
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "name",
                        "Name of the station to remove",
                    )
                    .required(true),
                ),
            )
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "list",
                "List all saved ground stations",
            )),
        // /satellite – manage tracked satellites
        CreateCommand::new("satellite")
            .description("Manage tracked satellites")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "add",
                    "Track a satellite by NORAD ID",
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::Integer,
                        "norad_id",
                        "NORAD catalog number",
                    )
                    .required(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "label",
                        "Friendly name (optional; defaults to name from SatNOGS)",
                    )
                    .required(false),
                ),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "remove",
                    "Stop tracking a satellite",
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::Integer,
                        "norad_id",
                        "NORAD catalog number",
                    )
                    .required(true),
                ),
            )
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "list",
                "List all tracked satellites",
            )),
        // /set-notify-channel – configure where pass alerts go
        CreateCommand::new("set-notify-channel")
            .description("Set this channel as the destination for automatic pass notifications"),
        // /upcoming-passes – show all passes for tracked sats × saved stations
        CreateCommand::new("upcoming-passes")
            .description("Show upcoming passes for all tracked satellites over all saved stations"),
    ]
}

// ---------------------------------------------------------------------------
// Helper – send a plain text reply
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
    let latitude_deg = get_option_f64(command, "lat").unwrap_or(0.0);
    let longitude_deg = get_option_f64(command, "lon").unwrap_or(0.0);
    let elevation_m = get_option_f64(command, "elevation_m").unwrap_or(0.0);
    let altitude_m = get_option_f64(command, "altitude_m").unwrap_or(0.0);
    let hours = get_option_i64(command, "hours").unwrap_or(24).clamp(1, 72) as u64;
    let min_elev = get_option_f64(command, "min_elev").unwrap_or(5.0);
    let station_name = get_option_str(command, "station_name")
        .unwrap_or_else(|| format!("{latitude_deg:.2}°N, {longitude_deg:.2}°E"));

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

    let gs = GroundStation::new(
        station_name.clone(),
        latitude_deg,
        longitude_deg,
        elevation_m,
        altitude_m,
    );

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
        "🛰️ **{}** (NORAD {norad_id})\n\
         📡 Station: **{}** ({latitude_deg:.4}°, {longitude_deg:.4}°, {elevation_m:.0} m)\n\
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
// /station handler
// ---------------------------------------------------------------------------

async fn handle_station(ctx: &Context, command: &CommandInteraction) -> Result<()> {
    let db = get_db(ctx).await;
    let (sub, opts) = get_subcommand(command)?;

    match sub.as_str() {
        "add" => {
            let name = get_sub_str(&opts, "name")
                .ok_or_else(|| anyhow!("Missing required option: name"))?;
            let lat =
                get_sub_f64(&opts, "lat").ok_or_else(|| anyhow!("Missing required option: lat"))?;
            let lon =
                get_sub_f64(&opts, "lon").ok_or_else(|| anyhow!("Missing required option: lon"))?;
            let elevation_m = get_sub_f64(&opts, "elevation_m")
                .ok_or_else(|| anyhow!("Missing required option: elevation_m"))?;
            let altitude_m = get_sub_f64(&opts, "altitude_m")
                .ok_or_else(|| anyhow!("Missing required option: altitude_m"))?;

            let name2 = name.clone();
            match tokio::task::spawn_blocking(move || {
                db.add_station(&name2, lat, lon, elevation_m, altitude_m)
            })
            .await?
            {
                Ok(()) => {
                    send_reply(
                        ctx,
                        command,
                        &format!(
                            "✅ Saved station **{name}** ({lat:.4}°, {lon:.4}°, {elevation_m:.0} m)."
                        ),
                    )
                    .await;
                }
                Err(e) => {
                    send_reply(ctx, command, &format!("❌ {e}")).await;
                }
            }
        }

        "remove" => {
            let name = get_sub_str(&opts, "name")
                .ok_or_else(|| anyhow!("Missing required option: name"))?;

            let name2 = name.clone();
            let removed = tokio::task::spawn_blocking(move || db.remove_station(&name2)).await??;

            if removed {
                send_reply(ctx, command, &format!("✅ Removed station **{name}**.")).await;
            } else {
                send_reply(
                    ctx,
                    command,
                    &format!("❌ No station named **{name}** found."),
                )
                .await;
            }
        }

        "list" => {
            let stations = tokio::task::spawn_blocking(move || db.list_stations()).await??;

            if stations.is_empty() {
                send_reply(
                    ctx,
                    command,
                    "No ground stations saved. Use `/station add` to add one.",
                )
                .await;
            } else {
                let mut msg = format!("**Ground Stations ({}):**\n", stations.len());
                for s in &stations {
                    msg.push_str(&format!(
                        "• **{}** — {:.4}°, {:.4}°, {:.0} m\n",
                        s.name, s.latitude_deg, s.longitude_deg, s.altitude_m
                    ));
                }
                send_reply(ctx, command, &msg).await;
            }
        }

        _ => send_reply(ctx, command, "❓ Unknown subcommand.").await,
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// /satellite handler
// ---------------------------------------------------------------------------

async fn handle_satellite(ctx: &Context, command: &CommandInteraction) -> Result<()> {
    let db = get_db(ctx).await;
    let (sub, opts) = get_subcommand(command)?;

    match sub.as_str() {
        "add" => {
            let norad_id = get_sub_i64(&opts, "norad_id")
                .ok_or_else(|| anyhow!("Missing required option: norad_id"))?
                as u64;
            let label = get_sub_str(&opts, "label");
            let label_ref = label.as_deref().map(String::from);

            match tokio::task::spawn_blocking(move || {
                db.add_satellite(norad_id, label_ref.as_deref())
            })
            .await?
            {
                Ok(()) => {
                    let label_display = label.map_or(String::new(), |l| format!(" (**{l}**)"));
                    send_reply(
                        ctx,
                        command,
                        &format!("✅ Now tracking NORAD **{norad_id}**{label_display}."),
                    )
                    .await;
                }
                Err(e) => {
                    send_reply(ctx, command, &format!("❌ {e}")).await;
                }
            }
        }

        "remove" => {
            let norad_id = get_sub_i64(&opts, "norad_id")
                .ok_or_else(|| anyhow!("Missing required option: norad_id"))?
                as u64;

            let removed =
                tokio::task::spawn_blocking(move || db.remove_satellite(norad_id)).await??;

            if removed {
                send_reply(
                    ctx,
                    command,
                    &format!("✅ Stopped tracking NORAD **{norad_id}**."),
                )
                .await;
            } else {
                send_reply(
                    ctx,
                    command,
                    &format!("❌ NORAD **{norad_id}** is not in the tracking list."),
                )
                .await;
            }
        }

        "list" => {
            let sats = tokio::task::spawn_blocking(move || db.list_satellites()).await??;

            if sats.is_empty() {
                send_reply(
                    ctx,
                    command,
                    "No satellites tracked. Use `/satellite add` to add one.",
                )
                .await;
            } else {
                let mut msg = format!("**Tracked Satellites ({}):**\n", sats.len());
                for s in &sats {
                    let label = s.label.as_deref().unwrap_or("—");
                    msg.push_str(&format!("• NORAD **{}** — {}\n", s.norad_id, label));
                }
                send_reply(ctx, command, &msg).await;
            }
        }

        _ => send_reply(ctx, command, "❓ Unknown subcommand.").await,
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// /set-notify-channel handler
// ---------------------------------------------------------------------------

async fn handle_set_notify_channel(ctx: &Context, command: &CommandInteraction) -> Result<()> {
    let channel_id = command.channel_id;
    let value = channel_id.get().to_string();

    let db = get_db(ctx).await;
    tokio::task::spawn_blocking(move || db.set_setting("notify_channel_id", &value)).await??;

    send_reply(
        ctx,
        command,
        &format!("✅ Pass notifications will be sent to <#{channel_id}>."),
    )
    .await;
    Ok(())
}

// ---------------------------------------------------------------------------
// /upcoming-passes handler
// ---------------------------------------------------------------------------

async fn handle_upcoming_passes(ctx: &Context, command: &CommandInteraction) -> Result<()> {
    command
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new()),
        )
        .await?;

    let db = get_db(ctx).await;

    let (stations, satellites) = {
        let db2 = db.clone();
        tokio::task::spawn_blocking(move || -> Result<_> {
            Ok((db2.list_stations()?, db2.list_satellites()?))
        })
        .await??
    };

    if stations.is_empty() {
        command
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new()
                    .content("No ground stations configured. Use `/station add` to add one."),
            )
            .await?;
        return Ok(());
    }
    if satellites.is_empty() {
        command
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new()
                    .content("No satellites tracked. Use `/satellite add` to add one."),
            )
            .await?;
        return Ok(());
    }

    // Fetch TLEs and compute passes for every satellite, then merge and sort.
    // Does NOT touch the notified_passes table.
    let mut all_passes: Vec<(String, u64, Pass)> = Vec::new();

    for sat in &satellites {
        let tle = match satnogs::fetch_tle(sat.norad_id).await {
            Ok(t) => t,
            Err(e) => {
                warn!("Failed to fetch TLE for NORAD {}: {e:#}", sat.norad_id);
                continue;
            }
        };

        let display_name = sat.label.as_deref().unwrap_or(&tle.name).to_string();
        let norad_id = sat.norad_id;
        let tle2 = tle.clone();
        let stations2 = stations.clone();

        let passes = match tokio::task::spawn_blocking(move || {
            find_passes(
                &tle2.line1,
                &tle2.line2,
                &stations2,
                CHECK_HOURS,
                CHECK_MIN_ELEV_DEG,
            )
        })
        .await
        {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                warn!("Pass computation failed for NORAD {norad_id}: {e:#}");
                continue;
            }
            Err(e) => {
                warn!("spawn_blocking panicked for NORAD {norad_id}: {e:#}");
                continue;
            }
        };

        for pass in passes {
            all_passes.push((display_name.clone(), norad_id, pass));
        }
    }

    if all_passes.is_empty() {
        command
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new().content(format!(
                    "No passes above {CHECK_MIN_ELEV_DEG:.0}° found in the next \
                     {CHECK_HOURS}h for any tracked satellite / saved station combination."
                )),
            )
            .await?;
        return Ok(());
    }

    // Sort all passes by AOS time across all satellites and stations.
    all_passes.sort_by_key(|(_, _, p)| p.aos_utc);

    let mut reply = format!(
        "🔭 **Upcoming passes** — next **{CHECK_HOURS}h** | min elev **{CHECK_MIN_ELEV_DEG:.0}°**\n\
         {} satellite(s) × {} station(s) — **{}** pass(es) found\n\n",
        satellites.len(),
        stations.len(),
        all_passes.len(),
    );

    for (i, (name, norad_id, p)) in all_passes.iter().enumerate() {
        reply.push_str(&format!(
            "**Pass {}** — 🛰️ **{name}** (NORAD {norad_id}) over **{}** on {}\n\
             • AOS: {} (elev {:.1}°, az {:.0}°)\n\
             • MAX: {} (elev **{:.1}°**, az {:.0}°)\n\
             • LOS: {} (elev {:.1}°, az {:.0}°)\n\
             • Duration: **{}m {}s**\n\n",
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
        ));

        if reply.len() > 1700 && i + 1 < all_passes.len() {
            reply.push_str(&format!(
                "*…and {} more pass(es). Use `/passes` with a narrower window to see more.*",
                all_passes.len() - i - 1
            ));
            break;
        }
    }

    command
        .edit_response(&ctx.http, EditInteractionResponse::new().content(&reply))
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Background pass checker
// ---------------------------------------------------------------------------

/// Core logic shared by the background loop and `/check`.
///
/// Fetches fresh TLEs for every tracked satellite, computes passes over every
/// saved station for the next [`CHECK_HOURS`] hours, and sends a Discord
/// message for each pass that hasn't been announced before.
///
/// Returns the number of new pass announcements sent.
async fn run_pass_check(http: &Http, db: &Arc<Database>) -> Result<usize> {
    // Resolve the notification channel.
    let channel_id = {
        let db2 = db.clone();
        let raw =
            tokio::task::spawn_blocking(move || db2.get_setting("notify_channel_id")).await??;
        match raw {
            Some(s) => ChannelId::new(s.parse::<u64>()?),
            None => return Ok(0),
        }
    };

    // Load stations and satellites.
    let (stations, satellites) = {
        let db2 = db.clone();
        tokio::task::spawn_blocking(move || -> Result<_> {
            Ok((db2.list_stations()?, db2.list_satellites()?))
        })
        .await??
    };

    if stations.is_empty() || satellites.is_empty() {
        return Ok(0);
    }

    // Prune stale de-duplication records (passes whose AOS is older than TTL).
    let cutoff = chrono::Utc::now().timestamp() - NOTIFIED_PASS_TTL_DAYS * 86_400;
    {
        let db2 = db.clone();
        let pruned =
            tokio::task::spawn_blocking(move || db2.cleanup_old_notified_passes(cutoff)).await??;
        if pruned > 0 {
            info!("Pruned {pruned} old notified-pass record(s)");
        }
    }

    let mut announced = 0_usize;

    for sat in &satellites {
        // Fetch fresh TLE from SatNOGS.
        let tle = match satnogs::fetch_tle(sat.norad_id).await {
            Ok(t) => t,
            Err(e) => {
                warn!("Failed to fetch TLE for NORAD {}: {e:#}", sat.norad_id);
                continue;
            }
        };

        let display_name = sat.label.as_deref().unwrap_or(&tle.name).to_string();

        // Compute passes (CPU-bound).
        let tle2 = tle.clone();
        let stations2 = stations.clone();
        let norad_id = sat.norad_id;
        let passes = match tokio::task::spawn_blocking(move || {
            find_passes(
                &tle2.line1,
                &tle2.line2,
                &stations2,
                CHECK_HOURS,
                CHECK_MIN_ELEV_DEG,
            )
        })
        .await
        {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                warn!("Pass computation failed for NORAD {norad_id}: {e:#}");
                continue;
            }
            Err(e) => {
                warn!("spawn_blocking panicked for NORAD {norad_id}: {e:#}");
                continue;
            }
        };

        // Announce each pass that hasn't been seen before.
        for pass in &passes {
            let aos_unix = pass.aos_utc.timestamp();
            let station = pass.station_name.clone();

            let already_notified = {
                let db2 = db.clone();
                let station2 = station.clone();
                tokio::task::spawn_blocking(move || {
                    db2.is_pass_notified(norad_id, &station2, aos_unix)
                })
                .await??
            };

            if already_notified {
                continue;
            }

            let msg = format_pass_notification(norad_id, &display_name, pass);
            match channel_id
                .send_message(http, CreateMessage::new().content(msg))
                .await
            {
                Ok(_) => {
                    let db2 = db.clone();
                    let station2 = station.clone();
                    tokio::task::spawn_blocking(move || {
                        db2.mark_pass_notified(norad_id, &station2, aos_unix)
                    })
                    .await??;
                    announced += 1;
                }
                Err(e) => {
                    error!("Failed to send pass notification: {e:#}");
                }
            }

            // Be polite to Discord's rate limiter between messages.
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        // Be polite to SatNOGS between TLE fetches.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    Ok(announced)
}

/// Format one pass as a Discord notification message.
fn format_pass_notification(norad_id: u64, name: &str, p: &Pass) -> String {
    format!(
        "🛰️ **{name}** (NORAD {norad_id}) — upcoming pass over **{}**\n\
         📅 {}\n\
         • AOS: {} (elev {:.1}°, az {:.0}°)\n\
         • MAX: {} (elev **{:.1}°**, az {:.0}°)\n\
         • LOS: {} (elev {:.1}°, az {:.0}°)\n\
         • Duration: **{}m {}s** above {CHECK_MIN_ELEV_DEG:.0}°",
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
    )
}

// ---------------------------------------------------------------------------
// Shared-state helpers
// ---------------------------------------------------------------------------

async fn get_db(ctx: &Context) -> Arc<Database> {
    ctx.data.read().await.get::<DatabaseKey>().unwrap().clone()
}

// ---------------------------------------------------------------------------
// Subcommand option extraction helpers
// ---------------------------------------------------------------------------

/// Extract the subcommand name and its nested options as owned data.
fn get_subcommand(cmd: &CommandInteraction) -> Result<(String, Vec<CommandDataOption>)> {
    cmd.data
        .options
        .first()
        .and_then(|opt| {
            if let CommandDataOptionValue::SubCommand(subopts) = &opt.value {
                Some((opt.name.clone(), subopts.clone()))
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow!("Expected a subcommand"))
}

fn get_sub_i64(opts: &[CommandDataOption], name: &str) -> Option<i64> {
    opts.iter().find(|o| o.name == name).and_then(|o| {
        if let CommandDataOptionValue::Integer(v) = &o.value {
            Some(*v)
        } else {
            None
        }
    })
}

fn get_sub_f64(opts: &[CommandDataOption], name: &str) -> Option<f64> {
    opts.iter().find(|o| o.name == name).and_then(|o| {
        if let CommandDataOptionValue::Number(v) = &o.value {
            Some(*v)
        } else {
            None
        }
    })
}

fn get_sub_str(opts: &[CommandDataOption], name: &str) -> Option<String> {
    opts.iter().find(|o| o.name == name).and_then(|o| {
        if let CommandDataOptionValue::String(v) = &o.value {
            Some(v.clone())
        } else {
            None
        }
    })
}

// ---------------------------------------------------------------------------
// Top-level option extraction helpers (used by /passes and /tle)
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
        info!("Background pass checker started");

        loop {
            match run_pass_check(&http, &db_bg).await {
                Ok(0) => {}
                Ok(n) => info!("Pass check: announced {n} new pass(es)"),
                Err(e) => error!("Pass check error: {e:#}"),
            }
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
    });

    info!("Starting sat-discord-bot…");
    client.start().await?;

    Ok(())
}
