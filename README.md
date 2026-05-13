# SatWatch-Discord-Bot

A Discord bot to send CubeSat updates to Discord (overpass forecasts, more coming soon).

- **TLE source**: [SatNOGS DB](https://db.satnogs.org/) public API (always fresh)
- **Propagator**: [satkit](https://docs.rs/satkit) SGP4 (`satkit::sgp4`)
- **Frame transforms**: TEME → ITRF → ENU via `satkit::frametransform`
- **Discord**: [Serenity](https://github.com/serenity-rs/serenity) with slash commands

---

## Usage

If you are interested in using this bot, feel free to open a GitHub Issue, and I can help you install the bot in your Discord server.

Otherwise, run the bot on your own machine following the setup instructions in [Local Dev](docs/Local_Dev.md).

## Slash commands

* `/satellite add/remove/list` - Manage the watchlist of satellites.
* `/station add/remove/list` - Manage the watchlist of ground stations.
* `/set-notify-channel` - Subscribe a channel to receive notifications for satellite passes.
* `/upcoming-passes` - Query upcoming passes for all subscribed satellites and ground stations.

### Direct-usage commands

* `/tle` - Query TLE data for a satellite by NORAD ID.
* `/passes` - Query passes for a specific satellite over a ground station.

## Future Features

* Decode and share beacon data, as collected via SatNOGS.
