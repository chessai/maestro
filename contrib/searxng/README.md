# Self-hosted SearXNG for maestro

A runnable local [SearXNG](https://docs.searxng.org/) instance for maestro's
private search path (`search.backend = "searxng"`, ADR-005). Search runs on
infra you control — no per-search API cost, engine snippets included, and
nothing leaves your network.

This is the alternative to the default `search.backend = "anthropic"` (Anthropic's
server-side `web_search`, which needs only an API key). Use SearXNG when you want
search entirely self-hosted / on-VPN.

## Run it

```sh
cd contrib/searxng
# edit settings.yml: set server.secret_key to a random value
#   sed -i "s/ultrasecret-change-me/$(openssl rand -hex 32)/" settings.yml
docker compose up -d          # or: podman compose up -d
```

Smoke-test the JSON API the daemon uses:

```sh
curl 'http://127.0.0.1:8888/search?q=rust+ownership&format=json' | head -c 400
```

A JSON body with a `results` array means it's ready. A `403` means the `json`
format isn't enabled — check `search.formats` in `settings.yml`.

## Point maestro at it

In `~/.config/maestro/config.toml`, on the profile you want:

```toml
[profiles.personal]
search.backend  = "searxng"
search.endpoint = "http://127.0.0.1:8888"   # no trailing slash
```

The daemon reads this fresh per request, so no restart is needed. Verify with
`maestro doctor` (the resolved profile shows `search.backend = "searxng"`); an
unreachable endpoint surfaces as a `backend_unavailable` tool error, never a
silent fallback (ADR-005).

## Remote / VPN box

Bind SearXNG on the tunnel interface instead of localhost (edit the `ports:`
line in `docker-compose.yml` and `SEARXNG_BASE_URL`), then set
`search.endpoint` to the WireGuard/VPN address. A machine that can't reach the
endpoint simply reports "no search on this host" rather than reasoning from
stale knowledge.
