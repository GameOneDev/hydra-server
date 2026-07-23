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
| Custom game images (covers, icons, logos, banners) | **this server** |
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
| `HYDRA_MAX_BYTES_PER_USER` | `0` (unlimited) | Per-user storage quota in bytes — counts save backups, emulation saves and uploaded custom images |
| `HYDRA_BACKUPS_PER_GAME_LIMIT` | `100` | Max save backups per game per user |
| `HYDRA_ALLOWED_USERS` | *(empty = everyone)* | Comma-separated official user ids or usernames allowed to use this server |
| `HYDRA_UPDATE_CHECK` | `true` | Periodically check GitHub for a newer server release and flag it in the admin panel. Set to `0`/`false` for air-gapped installs |
| `HYDRA_UPDATE_CHECK_INTERVAL_HOURS` | `6` | Hours between update checks (minimum 1) |
| `HYDRA_UPDATE_REPO` | `gameonedev/hydra-server` | `owner/repo` whose GitHub releases are compared against the running version |
| `HYDRA_AUTO_UPDATE` | `false` | Default for auto-install: when on, a detected update is downloaded and installed automatically. Off by default — you just get notified. Editable live from the admin panel |

`HYDRA_MAX_BYTES_PER_USER`, `HYDRA_BACKUPS_PER_GAME_LIMIT`,
`HYDRA_ALLOWED_USERS` and the auto-install toggle can also be edited live from
the admin panel; values saved there are stored in the database and override the
environment until reset.

### Admin panel

Open `https://your-server/admin`, sign in with `HYDRA_ADMIN_PASSWORD`:

- overview of users, backups, shares, achievements and total storage
- server info: version, uptime, database size and effective configuration
- update status: whether a newer server release is out (checked against GitHub),
  with a link to the release notes, a "Check now" button and — on native
  installs — an "Update now" button that installs it and restarts
- edit settings without a restart: per-user quota, backups-per-game limit, the
  allowed-users list and the auto-install toggle, applied immediately and
  persisted across restarts
- per-user detail: profile info plus save backups, achievements and emulation
  saves — backups show the game's name and cover art (resolved from the Steam
  store and cached) instead of the raw shop id
- download or delete any backup
- block/unblock users, delete all of a user's data

## API surface

Implements the endpoints the launcher routes to a self-hosted cloud server:

- `GET|POST /profile/games/artifacts`, `POST /profile/games/artifacts/{id}/download`,
  `DELETE|PATCH /profile/games/artifacts/{id}`, `PUT …/{id}/freeze|unfreeze`
- `PUT /profile/games/achievements` (union merge by achievement name, earliest
  unlock wins), `DELETE /profile/games/achievements/{remoteGameId}`
- `GET /profile/achievements/{userId}` — recently unlocked achievements for a
  profile, so members show recent activity the official API only compares for
  subscribers. Names and unlock times only; the launcher joins the public
  catalogue for icons and titles. Deliberately not under
  `/profile/games/achievements`, which the launcher mirrors to both servers
- `POST /profile/games/{shop}/{objectId}/artwork/{grids|heroes|logos|icons}/upload-url`,
  `PUT|DELETE /profile/games/{shop}/{objectId}/artwork/{kind}` — custom game
  images, uploaded here or picked from SteamGridDB
- `GET /profile/games/artwork`, `GET /profile/games/artwork/{userId}` — the
  launcher reads these back to repaint its library and to show other members'
  custom images on their profiles
- `GET|POST|DELETE /profile/download-sources`
- `GET /profile/emulation-saves`, `POST /profile/emulation-saves/upload-url`,
  `POST …/{id}/commit`, `POST …/{id}/download-url`, `PUT|DELETE …/{id}`
- `POST /presigned-urls/{background-image|profile-image}` — profile image
  uploads; images are served publicly from `GET /images/…`
- `PUT|GET /storage/{token}` — S3-style presigned upload/download URLs
  (signed, short-lived, streamed to/from disk)
- `GET /health`

## Versioning & updates

The server's version tracks the Hydra launcher it targets: **`hydra-server
X.Y.Z` is built for Hydra app `X.Y.Z`**, so the version number alone tells you
which client release a given server is meant to run alongside.

Checking for updates and installing them are separate, independently
controlled steps:

- **Checking** (`HYDRA_UPDATE_CHECK`, on by default) polls this repo's GitHub
  releases on a schedule and flags a newer version in the admin panel — an
  "Update available" banner plus a "Check now" button for an on-demand look.
  Leave it on to always know when you're behind, or disable it for air-gapped
  installs.
- **Installing** is off by default: you decide when to update. Flip on
  **auto-install** (`HYDRA_AUTO_UPDATE`, or the admin panel toggle) to have
  detected updates download and apply themselves, or leave it off and click
  **Update now** in the panel when you're ready.

When an install runs (auto or button), the server downloads the release binary
for its platform, swaps it in and restarts into the new version. Your data dir
carries over untouched; migrations run on start.

**Containers can't self-install** — the swapped binary wouldn't survive the
next restart, which reverts to the image. There the panel says so and you
update the image instead:

- **Docker:** `docker compose pull && docker compose up -d` (or rebuild with
  `docker compose up -d --build` when building from source).
- **Binary:** the panel's "Update now" handles it; to update by hand instead,
  `git pull && cargo build --release`, then restart the service.

> Self-install needs release assets named with the OS and architecture, e.g.
> `hydra-server-linux-x86_64` / `hydra-server-linux-aarch64` — a raw executable,
> not an archive. Releases without a matching asset still show the notification;
> the "Update now" button just reports that there's nothing to install for your
> platform.

## Notes

- Put the server behind HTTPS (Caddy, nginx, Traefik) before exposing it to the
  internet — save bundles and tokens travel over this connection.
- Back up the data dir; it contains everything.
