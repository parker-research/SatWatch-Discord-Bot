# Local Dev

## Setup

### 1. Create a Discord application & bot

1. Go to <https://discord.com/developers/applications> → **New Application**
2. **Bot** tab → **Add Bot** → copy the **Token**
3. **OAuth2 → URL Generator**: check `bot` + `applications.commands`; select permissions: *Send Messages*, *Use Slash Commands*; pick "Guild Install"
4. Open the generated URL in your browser and add the bot to your server

### 2. Configure environment

```bash
cp .env.example .env
# Edit .env — set DISCORD_TOKEN and optionally DISCORD_GUILD_ID
```

`DISCORD_GUILD_ID` (your server's ID) makes slash commands register instantly.
Leave it unset for global registration (up to 1 hour delay).

### 3. Build and run

```bash
cargo build --release
./target/release/satwatch-discord-bot
```

Or for development:
```bash
cargo run
```
