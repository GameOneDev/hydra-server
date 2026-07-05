# hydra-server

Self-hosted **Hydra Cloud storage** server for the [Hydra launcher](https://github.com/hydralauncher/hydra).

Hydra's subscription ("Hydra Cloud") pays for the storage behind cloud saves and
related sync features. This server lets you host that storage yourself — for you
and your friends — while **everything else keeps using the official Hydra
servers**: your account, login, friends, profiles, comments, the game catalogue
and download sources browsing all work exactly as before.

## What it provides

| Feature | Where it lives |
| --- | --- |
| Account, login, friends, profiles, catalogue | Official Hydra servers (unchanged) |
| Cloud save backups (Ludusavi tar bundles) | **this server** |
| Emulation memory-card saves (PS1/PS2) | **this server** |
| Achievement sync across devices | **this server** |
| Download source list sync across devices | **this server** |
| Profile banner image hosting | **this server** (URL saved to the official profile) |
| Admin panel (users, storage, quotas) | **this server** at `/admin` |

## How authentication works

The launcher sends its **official Hydra access token** with every request. This
server validates the token by calling the official API's `/profile/me` and uses
the returned identity — it never stores passwords and issues no accounts of its
own. If someone isn't logged into Hydra, they can't use your server.

Optionally restrict who may use the server with `HYDRA_ALLOWED_USERS`, or block
users from the admin panel.

## Running

```bash
cargo build --release

HYDRA_ADMIN_PASSWORD=change-me \
HYDRA_SERVER_PUBLIC_URL=https://hydra-cloud.example.com \
./target/release/hydra-server
```

Or with Docker:

```bash
docker compose up -d
```

Then, in a launcher patched with self-hosted cloud support:
**Settings → Integrations → Self-hosted cloud storage** → enter your server URL
and save. Cloud save / achievement sync features unlock immediately; no
subscription needed.

### Configuration (environment variables)

| Variable | Default | Description |
| --- | --- | --- |
| `HYDRA_SERVER_BIND` | `0.0.0.0:8788` | Listen address |
| `HYDRA_SERVER_PUBLIC_URL` | `http://<bind>` | URL clients reach the server on — **must** be set when behind a reverse proxy, since upload/download URLs are built from it |
| `HYDRA_SERVER_DATA_DIR` | `./data` | SQLite database + stored save files |
| `HYDRA_OFFICIAL_API_URL` | `https://hydra-api-us-east-1.losbroxas.org` | Official API used to validate launcher tokens. If token validation fails with your launcher build, set this to the same API URL the launcher was built with (`MAIN_VITE_API_URL`) |
| `HYDRA_ADMIN_PASSWORD` | *(empty)* | Password for `/admin`. Panel is disabled while empty |
| `HYDRA_SERVER_SECRET` | auto-generated | Secret signing storage URLs and admin sessions; persisted to `<data dir>/.secret` when auto-generated |
| `HYDRA_MAX_BYTES_PER_USER` | `0` (unlimited) | Per-user storage quota in bytes |
| `HYDRA_BACKUPS_PER_GAME_LIMIT` | `100` | Max save backups per game per user |
| `HYDRA_ALLOWED_USERS` | *(empty = everyone)* | Comma-separated official user ids or usernames allowed to use this server |

### Admin panel

Open `https://your-server/admin`, sign in with `HYDRA_ADMIN_PASSWORD`:

- overview of users, backups and total storage
- per-user storage usage, backups and emulation saves
- download or delete any backup
- block/unblock users, delete all of a user's data

## API surface

Implements the endpoints the launcher routes to a self-hosted cloud server:

- `GET|POST /profile/games/artifacts`, `POST /profile/games/artifacts/{id}/download`,
  `DELETE|PATCH /profile/games/artifacts/{id}`, `PUT …/{id}/freeze|unfreeze`
- `PUT /profile/games/achievements` (union merge by achievement name, earliest
  unlock wins), `DELETE /profile/games/achievements/{remoteGameId}`
- `GET|POST|DELETE /profile/download-sources`
- `GET /profile/emulation-saves`, `POST /profile/emulation-saves/upload-url`,
  `POST …/{id}/commit`, `POST …/{id}/download-url`, `PUT|DELETE …/{id}`
- `POST /presigned-urls/{background-image|profile-image}` — profile image
  uploads; images are served publicly from `GET /images/…`
- `PUT|GET /storage/{token}` — S3-style presigned upload/download URLs
  (signed, short-lived, streamed to/from disk)
- `GET /health`

## Notes

- Put the server behind HTTPS (Caddy, nginx, Traefik) before exposing it to the
  internet — save bundles and tokens travel over this connection.
- Back up the data dir; it contains everything.
