# ADR-005: Search Shim Contract

Status: DRAFT

## Context

Web search is delegated for retrieval only; all synthesis happens in the advisor. The failure mode to engineer against is *judgment laundering*: a cheaper model's analysis sneaking into the planner disguised as data. The backend (SearXNG) is reachable from at most one machine at a time.

## Decision

### Two tools, composed by the advisor

**`search(queries: [string]) → [SearchResult]`**

```json
{ "query": "…", "url": "…", "title": "…", "engine_snippet": "…", "rank": 1, "retrieved_at": "…" }
```

Results metadata only; `engine_snippet` is verbatim from the engine, not model-generated.

**`fetch_extract(url, extraction_schema) → Extraction`**

```json
{
  "url": "…",
  "retrieved_at": "…",
  "content_digest": "sha256:…",
  "extractions": [
    { "field": "schema field name", "verbatim": "…", "char_offset": [1042, 1388] }
  ]
}
```

The extraction schema is supplied by the advisor per call (targeted extraction, not editorializing). **There is no free-text summary field anywhere in either schema.** Excerpts are verbatim with offsets into the fetched content; excerpt length capped (config, default 1500 chars/field). If there's no field for analysis, the shim model can't perform any.

### Execution

- Shim extraction is always Tier 0 one-shot API, defaulting to the **cheapest configured model (Haiku)** — the model's only job is verbatim span-mapping, which does not need a strong model; configurable via `roles.shim` (ADR-007). No driven session, no workspace. Shim calls are journaled as `sessions` rows with `role = 'shim'`, `task_id = NULL`, attributed to the calling advisor — so they carry no task lifecycle but do enter cost accounting.
- Fetching and readability-extraction happen in the daemon (Rust), not the model; the model's only job is returning, for each requested field, the exact **verbatim substring** it found in the page — nothing else. The model does NOT report offsets: LLMs cannot reliably count byte positions, so trusting a model-supplied offset rejects genuine quotes over off-by-one miscounts. Instead the **daemon locates** each returned `verbatim` in the fetched content (`content.find`) and computes the `char_offset` itself; a `verbatim` that does not occur in the content is rejected (a hallucinated quote is not in the page). This keeps purity *checked* — fabrication is caught by the locate step — while making the happy path robust. The tool result still carries `char_offset`, but it is daemon-computed, not model-claimed.

### Backend

The search backend is a per-profile config choice (`search.backend`, ADR-007), pluggable behind the `SearchBackend` trait, with **no automatic fallback between backends** — each is explicit:

- `anthropic` (**default**): Anthropic's server-side `web_search` tool. A cheap Tier-0 (Haiku) call issues the query; Anthropic runs the search on its own infrastructure and returns results. Works wherever the daemon has an API key — search is available out of the box, no self-hosted infra. Only the raw result **metadata (url + title)** is surfaced to the advisor; the page content Anthropic returns is encrypted for model citation, so there is no `engine_snippet` on this backend (the advisor uses `fetch_extract` for content). The query passes through a model, but no model prose reaches the advisor — the no-synthesis invariant holds at the result level.
- `searxng`: a self-hosted SearXNG instance (`search.endpoint`) — private/on-VPN search with engine snippets and no per-search API cost.
- `none`: search is explicitly disabled on this profile.

Search is **not** auto-disabled per profile — a profile that sets nothing gets the `anthropic` default. `backend_unavailable` is a structured tool error returned only when the resolved backend genuinely can't serve: `none` (explicit opt-out), an unreachable `searxng` endpoint, or a missing API key for `anthropic`. Never silent degradation: the advisor tells the human "no search on this host" instead of reasoning from stale knowledge.

### Caching

`shim_cache` keyed by (url, schema-hash), TTL 24h. `search` results are not cached (freshness is the point); `fetch_extract` is.

## Consequences

- Provenance discipline is mechanical: every claim the advisor synthesizes traces to a URL + offset + digest, glory-style.
- Daemon-side verbatim location (locate-or-reject) turns the "pure data shim" principle from a prompt request into an invariant, without asking the model to count bytes.

## Tradeoffs accepted

- Verbatim-with-offsets is clumsy for content requiring transformation (tables, unit conversions); accepted — the advisor transforms, or delegates a Tier 0 *computation* task, keeping the retrieval/analysis boundary clean.
- Machines without SearXNG still get web retrieval via the `anthropic` default (a machine only loses search if the profile sets `search.backend = "none"` or the API key is absent); accepted — search works out of the box, and disabling it is an explicit, visible profile choice rather than a silent gap.
- Offsets index the daemon's post-readability extraction of the page, not the raw HTML, and `content_digest` covers that cleaned text. Offset validation defeats fabrication and paraphrase, but not *selective quoting* — which spans to return is still the shim model's discretion. Accepted: the advisor sees url + digest + offset and can re-fetch or widen the schema, and targeted extraction schemas bound the cherry-picking surface.
