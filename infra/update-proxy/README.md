## Verified live — October 16, 2026

`https://updates.lathe-ide.com/releases` returns HTTP 200 and a JSON array of
30 releases (latest observed: `v1.11.2-beta`). `GITHUB_TOKEN` is configured as a
Cloudflare encrypted secret; its value was not read. `LATHE_PINGS` is bound to
`lathe_pings`. Unbenched settings match the original snapshot.
The token was prepared with a January 14, 2027 expiration; rotate it before then.
Live analytics writes remain untested; these header-free verification requests
do not create install events. Client changes still require the next release.
No commits or pushes were performed.

# Lathe update proxy

A dedicated `lathe-update-proxy` Worker proxies the fixed public GitHub releases
endpoint. Asset URLs and response body bytes stay unchanged. Analytics failures
are swallowed in `waitUntil`; updates do not depend on analytics succeeding.
Only GET `/releases` records pings. Without an install header no point is written.
Header values are restricted to ASCII letters, digits, `.`, `_`, `-`, and 64
characters (country: 2). No request headers or IP addresses are logged.
Observability is disabled in this Worker's config.

## Deploy manually

## Deployment update — October 16, 2026

Deployment succeeded. Worker version: `7d778dc6-1dee-4032-a550-e211287ef121`.
Custom domain: `https://updates.lathe-ide.com/releases`.
Workers.dev: `https://lathe-update-proxy.chris-r-paterson.workers.dev/releases`.
The `LATHE_PINGS` binding points to `lathe_pings`. Unbenched settings match the
pre-deployment snapshot and its domains remain unchanged.

HTTPS works, but live release verification received GitHub upstream 403/429
rate-limit responses. No GitHub token has been configured. Do not release the
prepared client changes until the endpoint returns release JSON reliably.
No commits or pushes were performed. The previous Analytics Engine activation
blocker has cleared. Live analytics writes have not been tested.

For subsequent deployments, run the commands below.
Use Node 24 LTS. Keep this directory's config separate from your other app.
Before publishing, check that `lathe-update-proxy` and `lathe_pings` are unused
names in your account; do not overwrite another existing project or dataset.

```sh
cd /Users/ChrisP/Documents/EDOC/lathe/infra/update-proxy
npm ci
npm run check
npm test
npx wrangler login
npx wrangler whoami
export CLOUDFLARE_ACCOUNT_ID='YOUR_ACCOUNT_ID'
npx wrangler deploy --config wrangler.toml
```

The deployment creates this Worker and binds the `lathe_pings` dataset; its SQL
table appears after the first recorded ping. The URL printed by Wrangler is
`https://lathe-update-proxy.YOUR_SUBDOMAIN.workers.dev`. Check it without creating
an analytics event:

```sh
curl --fail-with-body 'https://lathe-update-proxy.YOUR_SUBDOMAIN.workers.dev/releases'
```

Optionally add a GitHub token to increase upstream rate-limit headroom. A
fine-grained token restricted to the public Lathe repository with read access to
repository contents is sufficient for listing releases. Paste it at the prompt:

```sh
npx wrangler secret put GITHUB_TOKEN --config wrangler.toml
```

All four `LATHE_UPDATE_URL` workflow settings already target
`https://updates.lathe-ide.com/releases`:

- `.github/workflows/release_fork.yml` (three occurrences)
- `.github/workflows/lathe_linux_ci.yml` (one occurrence)

Keep `LATHE_DEFAULT_UPDATE_URL` set to GitHub. New clients retry GitHub once after
a proxy network failure or 5xx; the retry sends no Lathe headers. A proxy 4xx is
reported rather than retried. The proxy does not redirect client requests, so an
install hash cannot follow a redirect to GitHub or another host. Release notes
requests use the same cache/fallback path but do not record install activity.

## Use your own domain (no website required)

The Worker can directly serve `https://updates.YOUR_DOMAIN/releases`. You do not
need a website on the root domain. For this Custom Domain setup, the domain must
be an active Cloudflare zone in the same account. Choose an unused subdomain;
do not replace DNS records or routes belonging to your other app. A hostname
with an existing CNAME cannot be used as a Worker Custom Domain.

The route in `wrangler.toml` is already configured for `updates.lathe-ide.com`
(the generic example below shows its structure):

```toml
[[routes]]
pattern = "updates.YOUR_DOMAIN"
custom_domain = true
```

Use the same `npx wrangler deploy --config wrangler.toml` command above. Cloudflare
creates the DNS record and provisions its certificate for this hostname. Keep
`workers_dev = true` while validating the custom endpoint. Check it:

```sh
curl --fail-with-body 'https://updates.YOUR_DOMAIN/releases'
```

Then set all four workflow `LATHE_UPDATE_URL` values to
`https://updates.YOUR_DOMAIN/releases` instead of the workers.dev placeholder.
Use the direct endpoint; the client intentionally does not follow redirects.

Reference: https://developers.cloudflare.com/workers/configuration/routing/custom-domains/

## Cache behavior

The Cache API stores release responses for up to 24 hours, tracking a separate
five-minute freshness timestamp. Fresh data avoids an upstream request; stale
data is refreshed and used if GitHub returns an error or the network fails.
Entries can be evicted early, so outage coverage is best effort. Responses sent
to clients use `no-store`, ensuring each poll reaches the Worker for analytics.

Cloudflare's Cache API is local to each data center, not a single global cache.
Five-minute caching therefore reduces GitHub traffic but cannot guarantee staying
under its unauthenticated rate limit. The optional token is recommended. A
10-second upstream timeout permits stale-cache service or client fallback.

## Query after the first new release

Create a Cloudflare custom API token with **Account / Account Analytics / Read**,
restricted to this account. No edit permissions are needed for querying. This is
a separate token from Wrangler's deployment authentication.

```sh
export CF_ACCOUNT_ID='YOUR_ACCOUNT_ID'
read -r -s -p 'Read-only Cloudflare API token: ' CF_API_TOKEN
export CF_API_TOKEN
./stats.sh daily
./stats.sh weekly
./stats.sh versions
./stats.sh channels
./stats.sh platforms
./stats.sh sampling
# Or run all six queries:
./stats.sh all
```

`queries.sql` contains the SQL used by `stats.sh`; each invocation POSTs one query
to `https://api.cloudflare.com/client/v4/accounts/$CF_ACCOUNT_ID/analytics_engine/sql`.
Daily/calendar-week buckets use UTC; weeks start Monday. The 30-day range can
include partial first/current buckets. Version/channel/platform views cover the
last seven days. Version adoption means "seen on this version within seven days":
an install that upgrades can appear under multiple versions. Do not add those
rows to estimate unique people. Reinstalls and release channels may have separate
installation IDs. Only new builds configured for this proxy and opted into
metrics contribute counts. These are active updating installs, not all users.

Each hourly check writes a point; `count(DISTINCT index1)` deduplicates installs
at query time. Analytics Engine can sample data. Check `sampling`; if its value
exceeds 1, treat distinct counts as observed values that may undercount, not exact
population counts. Multiplying distinct counts by `_sample_interval` does not
correct that. Data retention is three months.

## Free-plan capacity (checked October 15, 2026)

Workers Free permits 100,000 requests/day per account (shared with other Workers
and Pages Functions) and 10 ms CPU/request. Analytics Engine publishes 100,000
writes/day and 10,000 read queries/day on Free. Its pricing page currently says
usage is not yet billed; size the app against the published allocation anyway.
At 24 checks per install/day:

| Active installs | Requests/day | Writes/day if all opt in | Remaining/day before other apps |
| ---: | ---: | ---: | ---: |
| 100 | 2,400 | 2,400 | 97,600 |
| 1,000 | 24,000 | 24,000 | 76,000 |
| 2,400 | 57,600 | 57,600 | 42,400 |
| 10,000 | 240,000 | 240,000 | 140,000 over allocation |

These are estimates before retries, manual checks, release notes, bots, and other
app traffic. Opted-out requests still use Worker requests but write no analytics.
Caching does not reduce Worker invocations. Watch account usage as installs grow.
The 10,000-install case does not fit Free; the intended current scale does.

Sources:
- https://developers.cloudflare.com/workers/platform/limits/
- https://developers.cloudflare.com/analytics/analytics-engine/pricing/
- https://developers.cloudflare.com/analytics/analytics-engine/limits/
- https://developers.cloudflare.com/analytics/analytics-engine/sql-api/
- https://developers.cloudflare.com/analytics/analytics-engine/sql-reference/aggregate-functions/
- https://developers.cloudflare.com/workers/runtime-apis/cache/

## Privacy and rollback

The stable install key is lowercase hex SHA-256 of
`lathe-update-ping-v1` followed by the installation ID. The constant is domain
separation, not a secret. `telemetry.metrics = false` suppresses this header and
therefore the entire analytics point. Version/channel/OS/arch may still be sent.
The Worker stores only the hash, these dimensions, country, timestamp, and 1.
Cloudflare must process network IPs to serve requests; this code neither writes
nor logs them. Do not enable request logging or tail production traffic if you
want to preserve that property.

The inherited `server_url: https://zed.dev` remains unchanged. The client maps it
to `https://api.zed.dev/telemetry/events` for upstream telemetry. Update pings and
that existing telemetry are separate paths.

To roll back build routing, restore the four workflow values to
`https://api.github.com/repos/paterschris/lathe/releases?per_page=30` and publish a
new release. Existing proxy-enabled binaries need the Worker endpoint to remain
available; keep it serving releases and remove the `recordPing` scheduling line
in `src/index.ts` if collection must stop immediately, then redeploy only this
Worker using the command above. To revert a later Worker deployment after a
previous working version exists, run `npx wrangler rollback --config wrangler.toml`.
Do not delete or change the other app's Worker, Pages project, routes, secrets, or
bindings. No commits or pushes are part of this setup.
