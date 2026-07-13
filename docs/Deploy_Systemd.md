# Running as a systemd service

## Guide

To keep the bot running persistently on a Linux server, a sample unit file is provided at
[docs/satwatch-discord-bot.service](docs/satwatch-discord-bot.service).

```bash
# Build a release binary
cargo build --release

# Create a dedicated user and install directory
sudo useradd -r -s /usr/sbin/nologin satwatch
sudo mkdir -p /opt/satwatch-discord-bot
sudo cp target/release/satwatch-discord-bot .env /opt/satwatch-discord-bot/
sudo chown -R satwatch:satwatch /opt/satwatch-discord-bot

# Install and start the service
sudo cp docs/satwatch-discord-bot.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now satwatch-discord-bot

# View logs
sudo journalctl -u satwatch-discord-bot -f
```

`WorkingDirectory` in the unit file doubles as where the bot's `.env` file and its
sqlite database (`satwatch.db`) live, unless `DATABASE_PATH` is set in `.env`. Adjust the
`User`, paths, and `ReadWritePaths` in the unit file to match your deployment.
