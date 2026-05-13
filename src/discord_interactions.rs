use crate::db::Database;
use crate::passes::{GroundStation, Pass, find_passes};
use crate::satnogs;

use anyhow::{Result, anyhow};
use serenity::all::{
    ChannelId, Command, CommandDataOption, CommandDataOptionValue, CommandInteraction,
    CommandOptionType, CreateCommand, CreateCommandOption, CreateInteractionResponse,
    CreateInteractionResponseFollowup, CreateInteractionResponseMessage, CreateMessage,
    EditInteractionResponse, GuildId, Interaction,
};
use serenity::async_trait;
use serenity::http::Http;
use serenity::model::gateway::Ready;
use serenity::prelude::*;
use std::env;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

/// Minimum elevation used by the background fresh TLE checker.
const CHECK_MIN_ELEV_DEG: f64 = 5.0;

/// Search window for the background fresh TLE checker.
const CHECK_HOURS: u64 = 48;

const SHORT_DELAY_DEBOUCE_DURATION: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// Serenity shared-state keys
// ---------------------------------------------------------------------------

pub struct DatabaseKey;
impl TypeMapKey for DatabaseKey {
    type Value = Arc<Database>;
}

// Populated in `ready()`; the background task waits for this to appear.
pub struct HttpKey;
impl TypeMapKey for HttpKey {
    type Value = Arc<Http>;
}

// ---------------------------------------------------------------------------
// Pass line formatting
// ---------------------------------------------------------------------------

/// Render one pass as a single Discord prose line (timing + elevation only).
///
/// Discord renders `<t:UNIX:f>` as localised date+time and `<t:UNIX:t>` as
/// time-only, both in the viewer's own timezone.
fn format_pass_line(p: &Pass) -> String {
    format!(
        "<t:{aos}:D> **<t:{aos}:t> → <t:{los}:t>** _({dur_m}m{dur_s:02}s, Peak El **{elev:.1}°**)_",
        aos = p.aos_utc.timestamp(),
        los = p.los_utc.timestamp(),
        elev = p.max_elev_deg,
        dur_m = p.duration_secs / 60,
        dur_s = p.duration_secs % 60,
    )
}

/// Render all passes for one satellite × ground-station pair as a single Discord
/// message block.  The group header (`🛰️ Sat @ Station`) is printed once at the
/// top; pass lines below carry only timing/elevation.  `context`, if non-empty,
/// is appended to the pass-count line (e.g. "next 24h | min elev 5°").
/// Truncates with a "…N more" footer if `char_limit` would be exceeded.
fn render_pass_group(
    name: &str,
    norad_id: u64,
    station: &str,
    passes: &[&Pass],
    min_elev: f64,
    char_limit: usize,
) -> String {
    const FOOTER_RESERVE: usize = 60;

    let header = format!("🛰️ **{name}** (NORAD {norad_id}) @ **{station}**\n");

    if passes.is_empty() {
        return format!("{header}No passes above {min_elev:.1}° in the search window.");
    }

    let count_line = format!(
        "**{}** pass{} above {min_elev:.1}° in the next {CHECK_HOURS}h.\n",
        passes.len(),
        if passes.len() != 1 { "es" } else { "" }
    );

    // Group passes by consecutive AOS/LOS intervals, separated by a blank line.
    let mut rendered: Vec<String> = Vec::new();
    for (i, pass) in passes.iter().enumerate() {
        if i > 0 && pass.aos_utc - passes[i - 1].los_utc > chrono::Duration::hours(3) {
            rendered.push(String::new());
        }
        rendered.push(format_pass_line(pass));
    }

    let mut out = format!("{header}{count_line}\n");
    let mut included = 0;

    for line in &rendered {
        let projected = out.len() + line.len() + 1 + FOOTER_RESERVE;
        if projected > char_limit {
            break;
        }
        out.push_str(line);
        out.push('\n');
        included += 1;
    }

    let remaining = rendered.len() - included;
    if remaining > 0 {
        out.push_str(&format!(
            "*…{remaining} more — narrow the window or raise min elev*"
        ));
    }

    // TODO: Add the TLE footer here.

    out
}

// ---------------------------------------------------------------------------
// Event handler
// ---------------------------------------------------------------------------

pub struct Handler;

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
            .description("Manage saved ground stations for this channel's subscription")
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
            .description("Manage tracked satellites for this channel's subscription")
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
// Helpers – subscription resolution
// ---------------------------------------------------------------------------

/// Return `guild_id` from the interaction, or an error if used outside a server.
fn require_guild(command: &CommandInteraction) -> Result<u64> {
    command
        .guild_id
        .map(|g| g.get())
        .ok_or_else(|| anyhow!("This command can only be used inside a server, not a DM."))
}

/// Get or create the subscription for the current (guild, channel).
async fn ensure_subscription(db: &Arc<Database>, guild_id: u64, channel_id: u64) -> Result<i64> {
    let db2 = db.clone();
    tokio::task::spawn_blocking(move || db2.ensure_subscription(guild_id, channel_id)).await?
}

/// Return the subscription id for the current channel, or `None`.
async fn get_subscription_id(db: &Arc<Database>, channel_id: u64) -> Result<Option<i64>> {
    let db2 = db.clone();
    tokio::task::spawn_blocking(move || db2.get_subscription_id(channel_id)).await?
}

// ---------------------------------------------------------------------------
// /tle handler
// ---------------------------------------------------------------------------

async fn handle_tle(ctx: &Context, command: &CommandInteraction) -> Result<()> {
    let norad_id = get_option_i64(command, "norad_id").unwrap_or(0) as u64;

    // Defer the reply so Discord doesn't time out while we fetch.
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

    // Defer immediately — orbit propagation + HTTP fetch may take a moment.
    command
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new()),
        )
        .await?;

    // Fetch TLE from SatNOGS.
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

    // Run pass prediction (CPU-bound; offloaded so we don't block the runtime).
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

    // ── Build reply ──────────────────────────────────────────────────────────
    let pass_refs: Vec<&Pass> = passes.iter().collect();
    let reply = render_pass_group(
        &tle_info.name,
        norad_id,
        &station_name,
        &pass_refs,
        min_elev,
        2000,
    );

    command
        .edit_response(&ctx.http, EditInteractionResponse::new().content(&reply))
        .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// /station handler
// ---------------------------------------------------------------------------

async fn handle_station(ctx: &Context, command: &CommandInteraction) -> Result<()> {
    let guild_id = require_guild(command)?;
    let channel_id = command.channel_id.get();
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

            let sub_id = ensure_subscription(&db, guild_id, channel_id).await?;
            let name2 = name.clone();
            match tokio::task::spawn_blocking(move || {
                db.add_station(sub_id, &name2, lat, lon, elevation_m, altitude_m)
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

            let sub_id = match get_subscription_id(&db, channel_id).await? {
                Some(id) => id,
                None => {
                    send_reply(ctx, command, "❌ No stations configured in this channel.").await;
                    return Ok(());
                }
            };

            let name2 = name.clone();
            let removed =
                tokio::task::spawn_blocking(move || db.remove_station(sub_id, &name2)).await??;

            if removed {
                send_reply(ctx, command, &format!("✅ Removed station **{name}**.")).await;
            } else {
                send_reply(
                    ctx,
                    command,
                    &format!("❌ No station named **{name}** found in this channel."),
                )
                .await;
            }
        }

        "list" => {
            let sub_id = match get_subscription_id(&db, channel_id).await? {
                Some(id) => id,
                None => {
                    send_reply(
                        ctx,
                        command,
                        "No ground stations saved in this channel. Use `/station add` to add one.",
                    )
                    .await;
                    return Ok(());
                }
            };

            let stations = tokio::task::spawn_blocking(move || db.list_stations(sub_id)).await??;

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
    let guild_id = require_guild(command)?;
    let channel_id = command.channel_id.get();
    let db = get_db(ctx).await;
    let (sub, opts) = get_subcommand(command)?;

    match sub.as_str() {
        "add" => {
            let norad_id = get_sub_i64(&opts, "norad_id")
                .ok_or_else(|| anyhow!("Missing required option: norad_id"))?
                as u64;
            let label = get_sub_str(&opts, "label");
            let label_ref = label.as_deref().map(String::from);

            let sub_id = ensure_subscription(&db, guild_id, channel_id).await?;
            match tokio::task::spawn_blocking(move || {
                db.add_satellite(sub_id, norad_id, label_ref.as_deref())
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

            let sub_id = match get_subscription_id(&db, channel_id).await? {
                Some(id) => id,
                None => {
                    send_reply(ctx, command, "❌ No satellites tracked in this channel.").await;
                    return Ok(());
                }
            };

            let removed =
                tokio::task::spawn_blocking(move || db.remove_satellite(sub_id, norad_id))
                    .await??;

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
                    &format!(
                        "❌ NORAD **{norad_id}** is not in the tracking list for this channel."
                    ),
                )
                .await;
            }
        }

        "list" => {
            let sub_id = match get_subscription_id(&db, channel_id).await? {
                Some(id) => id,
                None => {
                    send_reply(
                        ctx,
                        command,
                        "No satellites tracked in this channel. Use `/satellite add` to add one.",
                    )
                    .await;
                    return Ok(());
                }
            };

            let sats = tokio::task::spawn_blocking(move || db.list_satellites(sub_id)).await??;

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
    let guild_id = require_guild(command)?;
    let channel_id = command.channel_id;

    let db = get_db(ctx).await;
    ensure_subscription(&db, guild_id, channel_id.get()).await?;

    send_reply(
        ctx,
        command,
        &format!(
            "✅ <#{channel_id}> is set up for pass notifications. \
             Use `/station add` and `/satellite add` to configure what this channel watches."
        ),
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

    let channel_id = command.channel_id.get();
    let db = get_db(ctx).await;

    let sub_id = match get_subscription_id(&db, channel_id).await? {
        Some(id) => id,
        None => {
            command
                .edit_response(
                    &ctx.http,
                    EditInteractionResponse::new().content(
                        "This channel has no subscription. \
                         Use `/set-notify-channel` to set one up.",
                    ),
                )
                .await?;
            return Ok(());
        }
    };

    let (stations, satellites) = {
        let db2 = db.clone();
        tokio::task::spawn_blocking(move || -> Result<_> {
            Ok((db2.list_stations(sub_id)?, db2.list_satellites(sub_id)?))
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

    // Sort all passes chronologically by AOS across all satellites and stations.
    all_passes.sort_by_key(|(_, _, p)| p.aos_utc);

    // Group by (sat_name, norad_id, station_name), preserving AOS order within each group.
    let mut groups: Vec<(String, u64, String, Vec<usize>)> = Vec::new();
    for (i, (name, norad_id, pass)) in all_passes.iter().enumerate() {
        match groups
            .iter()
            .position(|(n, nid, s, _)| n == name && nid == norad_id && s == &pass.station_name)
        {
            Some(idx) => groups[idx].3.push(i),
            None => groups.push((name.clone(), *norad_id, pass.station_name.clone(), vec![i])),
        }
    }

    // ── Build reply ──────────────────────────────────────────────────────────
    if groups.is_empty() {
        command
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new().content(format!(
                    "No passes above {CHECK_MIN_ELEV_DEG:.1}° found in the next \
                     {CHECK_HOURS}h for any tracked satellite / saved station combination."
                )),
            )
            .await?;
        return Ok(());
    }

    // Edit the deferred reply with the first group; follow-up for the rest.
    let mut first = true;
    for (name, norad_id, station, indices) in &groups {
        let pass_refs: Vec<&Pass> = indices.iter().map(|&i| &all_passes[i].2).collect();
        let msg = render_pass_group(
            name,
            *norad_id,
            station,
            &pass_refs,
            CHECK_MIN_ELEV_DEG,
            2000,
        );
        if first {
            command
                .edit_response(&ctx.http, EditInteractionResponse::new().content(&msg))
                .await?;
            first = false;
        } else {
            command
                .create_followup(
                    &ctx.http,
                    CreateInteractionResponseFollowup::new().content(msg),
                )
                .await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// TLE footer formatting
// ---------------------------------------------------------------------------

/// Render the TLE footer appended to every new-TLE pass notification.
/// Includes the SatNOGS update timestamp as a Discord localised datetime
/// (`<t:UNIX:f>`) and the raw TLE lines in a code block.
fn format_tle_footer(tle: &satnogs::TleInfo) -> String {
    let ts_str = chrono::DateTime::parse_from_str(&tle.updated, "%Y-%m-%dT%H:%M:%S%.f%z")
        .map(|dt| {
            let unix = dt.timestamp();
            // <t:UNIX:f> = localised full date+time, <t:UNIX:R> = relative ("3 minutes ago")
            format!("<t:{unix}:f> (<t:{unix}:R>)")
        })
        .unwrap_or_else(|_| tle.updated.clone());

    format!(
        "\nTLE Updated: {ts_str}\n```\n{}\n{}\n```",
        tle.line1, tle.line2
    )
}

// ---------------------------------------------------------------------------
// Background fresh TLE checker
// ---------------------------------------------------------------------------

/// Core logic shared by the background loop.
///
/// Iterates every channel subscription, fetches fresh TLEs for its tracked
/// satellites, and — when the TLE's `updated` timestamp is new — computes
/// passes for the next [`CHECK_HOURS`] hours and sends one message per
/// satellite × station pair.  Each message includes the TLE lines and update
/// date at the bottom.
///
/// The TLE is written to the cache before messages are sent; a failed Discord
/// send is logged but not retried (avoids duplicate spam on transient errors).
///
/// Returns the total number of passes included in notifications sent.
pub async fn run_new_tle_check(http: &Http, db: &Arc<Database>) -> Result<usize> {
    // Load all subscriptions.
    let subscriptions = {
        let db2 = db.clone();
        tokio::task::spawn_blocking(move || db2.list_subscriptions()).await??
    };

    if subscriptions.is_empty() {
        return Ok(0);
    }

    let mut announced = 0_usize;

    for sub in &subscriptions {
        let sub_id = sub.id;
        let channel_id = ChannelId::new(sub.channel_id);

        // Load this subscription's stations and satellites.
        let (stations, satellites) = {
            let db2 = db.clone();
            tokio::task::spawn_blocking(move || -> Result<_> {
                Ok((db2.list_stations(sub_id)?, db2.list_satellites(sub_id)?))
            })
            .await??
        };

        if stations.is_empty() || satellites.is_empty() {
            continue;
        }

        for sat in &satellites {
            let norad_id = sat.norad_id;

            // Fetch fresh TLE from SatNOGS.
            let tle = match satnogs::fetch_tle(norad_id).await {
                Ok(t) => t,
                Err(e) => {
                    warn!(
                        "Failed to fetch TLE for NORAD {} (sub {}): {e:#}",
                        norad_id, sub_id
                    );
                    tokio::time::sleep(SHORT_DELAY_DEBOUCE_DURATION).await;
                    continue;
                }
            };

            // Skip if this TLE has already been announced for this subscription.
            let cached_updated = {
                let db2 = db.clone();
                tokio::task::spawn_blocking(move || db2.get_cached_tle_updated(sub_id, norad_id))
                    .await??
            };
            if cached_updated.as_deref() == Some(tle.updated.as_str()) {
                tokio::time::sleep(SHORT_DELAY_DEBOUCE_DURATION).await;
                continue;
            }

            info!(
                "New TLE for NORAD {norad_id} (sub {sub_id}): updated {}",
                tle.updated
            );

            // Cache the TLE before sending — a failed Discord delivery won't
            // cause a retry, but that is preferable to spamming on transient errors.
            {
                let db2 = db.clone();
                let updated = tle.updated.clone();
                let line1 = tle.line1.clone();
                let line2 = tle.line2.clone();
                tokio::task::spawn_blocking(move || {
                    db2.upsert_tle_cache(sub_id, norad_id, &updated, &line1, &line2)
                })
                .await??;
            }

            let display_name = sat.label.as_deref().unwrap_or(&tle.name).to_string();

            // Compute passes (CPU-bound).
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
                    warn!("Pass computation failed for NORAD {norad_id} (sub {sub_id}): {e:#}");
                    tokio::time::sleep(SHORT_DELAY_DEBOUCE_DURATION).await;
                    continue;
                }
                Err(e) => {
                    warn!("spawn_blocking panicked for NORAD {norad_id} (sub {sub_id}): {e:#}");
                    tokio::time::sleep(SHORT_DELAY_DEBOUCE_DURATION).await;
                    continue;
                }
            };

            // Group passes by station.
            let mut by_station: Vec<(String, Vec<usize>)> = Vec::new();
            for (i, pass) in passes.iter().enumerate() {
                let station = pass.station_name.clone();
                match by_station.iter().position(|(s, _)| s == &station) {
                    Some(idx) => by_station[idx].1.push(i),
                    None => by_station.push((station, vec![i])),
                }
            }

            // Send one message per sat×station group, with TLE footer appended.
            for (station, indices) in &by_station {
                let pass_refs: Vec<&Pass> = indices.iter().map(|&i| &passes[i]).collect();
                let mut msg = render_pass_group(
                    &display_name,
                    norad_id,
                    station,
                    &pass_refs,
                    CHECK_MIN_ELEV_DEG,
                    1800,
                );
                msg.push_str(&format_tle_footer(&tle));

                match channel_id
                    .send_message(http, CreateMessage::new().content(msg))
                    .await
                {
                    Ok(_) => announced += indices.len(),
                    Err(e) => {
                        error!("Failed to send pass notification to channel {channel_id}: {e:#}");
                    }
                }
                // Be polite to Discord's rate limiter between messages.
                tokio::time::sleep(SHORT_DELAY_DEBOUCE_DURATION).await;
            }

            // Be polite to SatNOGS between TLE fetches.
            tokio::time::sleep(SHORT_DELAY_DEBOUCE_DURATION).await;
        }
    }

    Ok(announced)
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
