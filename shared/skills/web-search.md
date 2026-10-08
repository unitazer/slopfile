---
name: web-search
description: >
  Search the web and read pages, with no API key and no account. Use this whenever a question needs
  information you do not have: current versions, recent releases, error messages from libraries,
  "what is X", "how do I configure Y", docs for a crate or API you have not seen, or any fact that
  may have changed since training. Also use it to look up an error message verbatim or to check
  whether a behaviour is a known bug. Triggers on: search the web, google, look up, find out,
  check the docs, latest version, current release, what does X do, why am I getting this error.
---

## Web search

No API key, no signup, no cost. Uses DuckDuckGo's lite endpoint.

```bash
python3 /shared/skills/websearch.py search "rust tokio select macro" -n 5
```

Prints numbered results as `title` / `url` / `snippet`. Default is 10 results; `-n` caps it.

**Always search before reading.** A snippet is often enough to answer the question, and reading a
whole page costs far more context than you should spend on a side question.

## Reading a page

```bash
python3 /shared/skills/websearch.py fetch "https://example.com/page"
```

Reduces the page to readable text and keeps link targets inline so you can cite or follow up. A
typical doc page is ~5x smaller as text than as HTML. Useful flags:

- `--chars 8000` to cap the output when you only need the top of a long page
- `--raw` to skip HTML stripping when you need the markup itself

## Writing down what you found

Search results are throwaway. If an answer depends on something you looked up, put it in this
session file, not just in your reply: a `note` region, or appended to a notes region you keep.
The reasoning that matters later is usually the fact plus its source URL, not the page contents.

```
<!-- slopfile:note -->
duckdb 1.4 has no ARM64 linux build; use the python client instead (github.com/duckdb/duckdb/issues/8821)
<!-- /slopfile -->
```

## Limits worth knowing

- DuckDuckGo rate-limits bursts. If `search` returns nothing, wait a few seconds and retry once; do
  not loop on it.
- The script only reads public pages. Anything behind a login or a paywall returns an error or a
  login page, and there is no way around that.
- Output is capped at 40,000 characters by default. For something longer, page through it with
  `curl -r` rather than raising the cap far.

## Better sources, if you outgrow this

None of these are needed to start; this skill exists so you have search on day one.

- **Self-hosted SearXNG** — unlimited and free, but you must run the container and add it to
  `docker-compose.yml`. Better than DuckDuckGo once you want no third party in the loop.
- **Tavily** — 1,000 free credits/month, no card. Returns AI-cleaned snippets and has an extract
  endpoint, so it beats scraping. Set `TAVILY_API_KEY` and use their HTTP API.
- **Firecrawl** — search and scrape in one call. Needs a free API key; the keyless tier rejects
  requests from some networks.