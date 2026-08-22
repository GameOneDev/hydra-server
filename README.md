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
| Cloud Save V2 — per-file snapshot sync (launcher 4.1.0+) | **this server** |
| Emulation memory-card saves (PS1/PS2) | **this server** |
| Achievement sync across devices | **this server** |
| Achievement souvenirs — screenshots on the profile (launcher 4.1.2+) | **this server** |
| Download source list sync across devices | **this server** |
| Profile banner image hosting | **this server** (URL saved to the official profile) |
| Custom game images (covers, icons, logos, banners) | **this server** |
| Admin panel (users, storage, history, quotas) | **this server** at `/admin` |
| User portal — players see and manage their own saves | **this server** at `/portal` |
| Event log, webhooks, Prometheus metrics, database backups | **this server** |

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

#### Using the pre-built image (GHCR)
By default, the compose file builds the image locally. To use the pre-built image from the GitHub Container Registry instead, edit `docker-compose.yml` and replace `build: .` with the image URL:
```yaml
services:
  hydra-server:
    image: ghcr.io/gameonedev/hydra-server:latest
```

#### Local Data Folder (Optional)
The `docker-compose.yml` file uses a Docker named volume for storage by default. If you prefer the data to be visible in a local `data` folder next to your compose file, change the `volumes` mapping in `docker-compose.yml` to `./data:/data`:
```yaml
    volumes:
      - ./data:/data
```
You can also remove the `volumes:` block at the bottom of the compose file if you do this.

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
| `HYDRA_MAX_BYTES_PER_USER` | `0` (unlimited) | Per-user storage quota in bytes — counts save backups, Cloud Save V2 blobs (once per distinct file), emulation saves and uploaded custom images |
| `HYDRA_BACKUPS_PER_GAME_LIMIT` | `100` | Max save backups per game per user |
| `HYDRA_ALLOWED_USERS` | *(empty = everyone)* | Comma-separated official user ids or usernames allowed to use this server |
| `HYDRA_LOGIN_MAX_ATTEMPTS` | `8` | Failed sign-ins from one address before it is locked out |
| `HYDRA_LOGIN_LOCKOUT_MINUTES` | `15` | How long a locked-out address stays locked |
| `HYDRA_TRUST_PROXY_HEADERS` | `false` | Take the client address from forwarding headers. **Only enable behind a proxy you control** — otherwise a client can spoof its address and walk past the lockout |
| `HYDRA_TRUSTED_PROXY_HOPS` | `0` | Proxies that append to `X-Forwarded-For` after the entry you want: `0` for one reverse proxy, `1` with Cloudflare in front of it |
| `HYDRA_CLIENT_IP_HEADER` | *(empty = auto)* | Read the client address from this header only, when the default order isn't what your proxy sets |
| `HYDRA_PORTAL_ENABLED` | `true` | Serve the user portal at `/portal` |
| `HYDRA_OFFICIAL_LOGIN_PATH` | `/auth/login` | Path on the official API the portal posts its sign-in form to |
| `HYDRA_METRICS_ENABLED` | `true` | Serve Prometheus metrics at `/metrics` |
| `HYDRA_METRICS_TOKEN` | *(empty = open)* | Bearer token required to scrape `/metrics` |
| `HYDRA_BACKUP_INTERVAL_HOURS` | `24` | Hours between automatic database backups (`0` disables them) |
| `HYDRA_BACKUP_KEEP` | `7` | Automatic backups kept before the oldest is pruned |
| `HYDRA_EVENT_RETENTION_DAYS` | `90` | Days of history kept in the event log |
| `HYDRA_PRESENCE_IDLE_MINUTES` | `15` | Quiet minutes after which a launcher counts as away, so its next call is logged as coming online (`0` switches these events off) |

`HYDRA_MAX_BYTES_PER_USER`, `HYDRA_BACKUPS_PER_GAME_LIMIT` and
`HYDRA_ALLOWED_USERS` can also be edited live from the admin panel; values saved
there are stored in the database and override the environment until reset.

### Admin panel

Open `https://your-server/admin` and sign in with `HYDRA_ADMIN_PASSWORD`. The
panel is a full operations console for the server, in nine screens.

**Overview** — headline totals (users, storage, cloud saves, backups), a
30-day activity chart, a live feed of what the server has been doing, the
biggest users and games, and a year of playtime. Anything that needs a human
gets an alert at the top with the screen that fixes it: uploads that never
finished, saves whose bytes are missing, users sitting at their quota.

**History** — the full event log, searchable and filterable by category
(sync / admin / auth / system), severity, kind, user and date range. It records
what the launchers did, every operator action, every sign-in and lockout, who
came online and when, and every background job — including things whose rows
are long gone, because a deleted save that leaves no trace is exactly the one
you end up asking about. Rows expand to the event's own detail.

**Columns** on that screen are yours to choose: the *Columns* button lists
every one the screen can draw — user, actor, game, IP address, category, size,
severity — and remembers what you picked in that browser. *When* and *Event*
stay, since something has to identify a row. **Other** is the catch-all: each
event kind keeps its own facts (how many files a sync moved, why an upload was
refused, how long someone had been away) and there is no column shape that fits
all of them, so that column renders whatever this row happened to record. The
full blob is still one click away in the expanded row.

**Users** — searchable, sortable directory with storage against quota. Each
account opens onto its own screen: what it stores broken down by kind, the
machines it syncs from (hostname, platform, last seen), its top games, and
tabs for saves, achievements, custom images, shares, download sources and
activity. Blocking, a per-category data purge, and full deletion live there
too, with byte counts reported for whatever was freed.

**Saves** — every stored save on the server in one filterable table, across
all three generations: Cloud Save V2 snapshots, legacy tarball backups and
emulation memory cards. Filter by kind, owner, game or state; sort by size to
find what is eating disk. V2 snapshots expand into their file manifest — each
file with its hash, size and a download of its own — and flag any file whose
bytes never arrived. Backups can be downloaded, frozen (exempt from the
per-game limit) or deleted.

**Games** — the same data pivoted by game: who plays it, what they store, how
long they have played, which custom art they picked. Names and covers resolve
from the Steam store and cache; a game whose lookup failed can be retried from
its own page.

**Storage** — usage measured from disk rather than the database, per area,
next to what the database expects, so drift is visible. The integrity scan
reconciles both directions: rows whose bytes are gone (a restore would come
back short) and files no row points at (space nothing will reclaim). It only
reports — deleting is a separate, explicit step.

**Maintenance** — database backups (take one, download it, upload one taken
elsewhere, restore from any of them — see [Backups](#backups)) plus the
housekeeping the server otherwise only does lazily: sweep abandoned uploads,
collect orphaned blobs, delete orphaned files, re-resolve missing game
metadata, prune old history, clear the token cache, compact the database. Each
reports what it actually changed. There is also a JSON export of the whole
inventory.

**Webhooks** — send events anywhere that accepts a POST. Filter by event family
and minimum severity, pick the payload shape (full JSON, or a rendered message
for Discord/Slack), and set a secret to have each delivery signed. A test button
sends one immediately and reports the status code; a hook that fails twenty
times in a row switches itself off.

**Settings** — per-user quota, backups-per-game limit and the allowed-users
list, applied immediately and persisted. Each value shows all three layers:
the environment default, whether an override is saved, and what is in force.

Everywhere else: ⌘K (Ctrl-K) opens a command palette that jumps to any screen
or searches users and games, and the panel follows your system light/dark
theme with a toggle to override it.

### User portal

`https://your-server/portal` is the players' own view: what they have stored
here, how much of their quota it uses, which machines they sync from, and their
achievements, custom images, shares and playtime. They can download any save —
including individual files out of a cloud save — and delete what they no longer
want, without an operator in the loop.

Signing in asks for the Hydra account they already have. The server forwards
those credentials **once** to the official Hydra API (`HYDRA_OFFICIAL_LOGIN_PATH`,
the same exchange the launcher's own sign-in performs), uses what comes back to
confirm the identity against `/profile/me`, and then issues a session cookie of
its own — the password is never stored, logged or kept in memory afterwards.
Two fallbacks exist for deployments where that doesn't fit: pasting a launcher
access token, and **portal links** an operator mints from a user's page in the
admin panel, which sign that one account in for fifteen minutes.

Set `HYDRA_PORTAL_ENABLED=false` to switch the whole thing off.

### Monitoring

`GET /metrics` exposes Prometheus metrics: users, stored bytes by kind, save
counts, pending uploads, blob count, playtime, failing webhooks, events per
category in the last hour, database size, free disk, request and byte counters.
Nothing personal — counts and totals only. Set `HYDRA_METRICS_TOKEN` to require
a bearer token, or `HYDRA_METRICS_ENABLED=false` to remove the endpoint.

### Presence

The launcher never signs out — it just stops calling — so "online" here means
*calling at all*, and coming online is the first call after a gap of
`HYDRA_PRESENCE_IDLE_MINUTES`. That produces two event kinds:

- `user.first_seen` — the account's first ever request to this server.
- `user.online` — a client that had gone quiet is back, with how long it was
  away and the address it came from.

Nothing is written while a client keeps calling: the check is a read against an
in-memory table, and the durable `last_seen_at` column is what stops a restart
announcing everyone who was already here as newly arrived. Set the variable to
`0` if you would rather not have these events at all.

### Backups

The stored save files are content-addressed and easy to copy with any tool; the
database is the part that maps them back to games and users, so it is backed up
on its own schedule with SQLite's `VACUUM INTO` (a consistent copy of a live
database, no writer blocking). Backups land in `<data dir>/backups`, the oldest
beyond `HYDRA_BACKUP_KEEP` are pruned, and the panel can take one on demand or
hand you the file.

**Restoring** happens from the panel too — *Maintenance → Database backups →
Restore*, which asks you to type `restore` first. The server does not swap the
file underneath itself (the pool has open connections and SQLite is mid-WAL);
it attaches the backup and replaces every table's rows inside one transaction,
so readers see the old database or the new one and nothing in between. No
restart, and the panel session survives it.

Three things worth knowing:

- **It is reversible.** A backup of the current database is taken first and
  named in the report, so restoring the wrong file costs one more click. That
  file is exempt from pruning for the run, so it can never delete the backup
  you are restoring from.
- **Save files on disk are untouched.** Rows from an older database can point
  at bytes that were already collected, and files uploaded since belong to no
  row — run *Storage → Integrity* afterwards, which reports both directions.
- **The schema has to match.** A backup taken before a migration is refused
  rather than restored into the wrong columns. Those still restore the old way:
  stop the server, put the file where `hydra-server.db` was, start it, and the
  migrations run against it.

*Upload backup* takes a file back the other way — a `.db` from this server that
lives somewhere else now — verifies it is a real, matching database, and puts
it in the backup directory ready to restore. Both the restore and the upload
are recorded in the event log, the restore at critical severity.

### Sign-in protection

Both password forms — the admin panel and the portal — lock an address out
after `HYDRA_LOGIN_MAX_ATTEMPTS` failures within fifteen minutes, and every
failure and lockout is recorded in the event log.

### Client addresses behind a proxy

The address is the key for all of that, so getting it wrong is not cosmetic:
every sign-in is logged from the proxy, and one visitor's fumbled password
locks out everyone behind it. Set `HYDRA_TRUST_PROXY_HEADERS=true` when a proxy
is the only way in — until then the connecting socket is the only thing
believed, because a forwarding header is forgeable by anything that can reach
the server directly.

With that on, the address is taken from the first of these that carries one:

1. `CF-Connecting-IP`, then `True-Client-IP` — Cloudflare overwrites these on
   every request, so they hold even when a reverse proxy behind Cloudflare
   fills `X-Real-IP` in with Cloudflare's edge address.
2. `X-Forwarded-For`, counted **from the right** and skipping
   `HYDRA_TRUSTED_PROXY_HOPS` entries. Each proxy appends the address it saw,
   so the right-hand end was written by machines you run while the left-hand
   end is whatever the visitor claimed — Cloudflare appends to a header the
   visitor sent rather than replacing it, which is why "take the first entry"
   is forgeable. Leave the hop count at `0` for a single reverse proxy.
3. `X-Real-IP`.

If your proxy puts it somewhere else, name that header in
`HYDRA_CLIENT_IP_HEADER` and nothing else is consulted. Entries that aren't
addresses are discarded rather than passed through, since these strings become
rate-limit keys and log lines.

**To check it worked:** *Settings → Client addresses* shows what the server
made of the request that drew the screen — the address it settled on, the
header it came from, the socket it actually arrived on, and every forwarding
header that turned up. It also says so plainly when a proxy is clearly in front
and its headers are being ignored.

#### Extending the panel

The panel is deliberately modular, one module per screen:

| Layer | Where |
| --- | --- |
| API routes | `src/admin/<area>.rs`, merged in `src/admin/mod.rs` |
| Screen | `static/admin/js/views/<area>.js`, routed in `static/admin/js/main.js` |
| Navigation | the `NAV` table in `static/admin/js/components/shell.js` |
| Shared UI | `static/shared/js/` — design system, tables, charts, dialogs, toasts |
| Portal | `src/portal/` and `static/portal/`, over the same shared UI |

A new screen is a new module, a new view file and one line in each of the three
registries; nothing else needs to change. Both front ends are embedded in the
binary and served from one place (`src/assets.rs`), so there is still a single
artifact to deploy — add a row there when you add a file under `static/`.

To record something new in the log, build an `events::Event` and hand it to
`events::record`; the History screen, the audit trail and every webhook pick it
up with no further wiring.

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
- `POST /presigned-urls/achievement-image` — reserves an achievement souvenir
  and returns the presigned PUT for its screenshot
- `GET /users/{userId}/souvenirs`, `POST …/{souvenirId}/like`,
  `POST …/{souvenirId}/report` — the profile's souvenir tab
- `PATCH /profile/souvenirs/{id}/visibility`, `DELETE /profile/souvenirs/{id}`,
  `PATCH /profile/souvenirs-visibility` — hiding one souvenir, deleting one, and
  the account-level setting the launcher mirrors here
- `GET /users/{userId}/games/achievements?shop=&objectId=` — the souvenir
  thumbnail for each unlock, for the achievement list
- `PUT|GET /storage/{token}` — S3-style presigned upload/download URLs
  (signed, short-lived, streamed to/from disk)
- `GET /health`
- `GET /capabilities` — version and feature list (see below)
- `GET /metrics` — Prometheus metrics (see Monitoring)

### Cloud Save V2

Launcher 4.1.0 replaced the tarball-per-backup flow with per-file snapshots for
Steam games. A save is a manifest of files, each content-addressed by SHA-256:

- `POST /profile/cloud-saves/prepare-snapshot` — registers the manifest and
  returns a presigned PUT for each blob the server doesn't already hold
  (everything else comes back as `skip`, so only changed files upload)
- `POST /profile/cloud-saves/commit-snapshot` — verifies the bytes landed and
  promotes the snapshot to the game's current save
- `GET|DELETE /profile/cloud-saves/snapshots?shop=&objectId=`
- `GET /profile/cloud-saves/snapshot-restore-manifest?snapshotId=`
- `GET /profile/cloud-saves/snapshot-download-urls?snapshotId=`

Notable behaviour:

- **Conflict detection.** Every commit bumps the snapshot `version`. The
  launcher sends the version it started from as `baseVersion`; if another
  machine has committed since, the upload is refused with `409` instead of
  overwriting the newer save.
- **Deduplication.** Blobs are stored once per user per hash, so a file shared
  across variants or games costs one copy — and re-syncing an unchanged save
  transfers nothing.
- **Integrity.** Blob uploads are hashed as they stream and rejected if they
  don't match the hash the URL was signed for, so a content-addressed object
  can never hold the wrong bytes.
- **Garbage collection.** Blobs are deleted once no manifest references them;
  abandoned uploads are swept after 24 hours.

The legacy artifact endpoints stay in place — the launcher still uses them for
non-Steam games and for older clients.

### Achievement souvenirs

Launcher 4.1.2 takes a screenshot when an achievement pops and files it on the
player's profile. Several achievements that unlock together share one picture,
so the picture is the record and the achievement names hang off it.

The upload is a handshake, because the row exists before the bytes do:

1. `POST /presigned-urls/achievement-image` reserves the capture's `clientId`
   and answers with the storage key and a presigned PUT.
2. The launcher PUTs the screenshot to `/storage/{token}`.
3. `PUT /profile/games/achievements` arrives with the souvenir next to the
   achievements it belongs to, which files it and makes it visible.

The launcher retries the whole sequence with the same `clientId` until the
server acknowledges it, so every step is idempotent: re-authorizing returns the
same key (`status: "claimed"` once the bytes are here), and re-syncing returns
the same souvenir id. Failures answer with the codes the launcher's retry policy
knows — `achievements/souvenir-conflict` plus a `reason`, or
`achievements/souvenir-upload-incomplete` — so a souvenir that only needs
another attempt gets one instead of being dropped.

Notable behaviour:

- **One souvenir per achievement, per game.** A second capture for an
  achievement that already has one is refused with `achievement_already_assigned`
  and the launcher syncs the achievements without it. Names are compared
  upper-cased and scoped to the game, since plenty of games ship an `ACH_WIN`.
- **Nothing is visible until it is complete.** A reservation whose upload never
  arrived shows on no profile, counts against the quota while its bytes sit on
  disk, and is swept by *Maintenance → Sweep abandoned uploads*.
- **Two levels of privacy.** Each souvenir is `PUBLIC` or `PRIVATE`, and the
  whole tab is gated by the account-level setting from the official profile.
  That setting lives on the official API, so the launcher mirrors it here; until
  it does, the default is `PRIVATE` and other members see `hiddenReason`. On
  this server "friends only" means "members of this server" — the official
  friend graph isn't visible from here, and everyone who can reach this server
  is someone you let in.
- **Likes and reports.** Any member can like another's souvenir (one POST
  toggles) or report it. A report is recorded and raised as a warning in the
  event log with who, why and which picture — the History screen and the
  Overview alert are how an operator sees it, and the record outlives the
  souvenir being deleted. Reports are capped at 30 per person per hour.
- **Two things this server can't know.** Achievement display names, icons and
  points come from the public catalogue, so souvenirs are served with raw
  achievement names and the launcher joins the catalogue for the rest — the same
  split the recent-achievements endpoint already uses. And because rarity is
  computed from those catalogue points, `sortBy=rare` falls back to ordering by
  capture time.

Souvenir images are served from `GET /images/souvenirs/…` under an unguessable
name, like banners and custom artwork: hiding a souvenir removes it from the
profile and from the achievement list, but does not make its URL stop working
for someone who already has it.

### Capabilities

`GET /capabilities` (unauthenticated) reports what this build supports:

```json
{ "name": "hydra-server", "version": "4.1.2", "features": ["cloud-saves-v2", "souvenirs", "..."] }
```

The launcher checks this before enabling a feature whose endpoints might not
exist here yet — upstream keeps adding subscription-gated features that get
routed to whichever cloud server is configured, and without this the only
symptom would be a 404 in the middle of a sync. `features` is the contract to
match on; `version` tracks the launcher release this server targets and is for
display and support.

## Releasing

Docker images are published to `ghcr.io/gameonedev/hydra-server`

## Notes

- Put the server behind HTTPS (Caddy, nginx, Traefik) before exposing it to the
  internet — save bundles and tokens travel over this connection.
- Back up the data dir; it contains everything.
