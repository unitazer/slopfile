#!/usr/bin/env python3
"""Key-free web search and page reading for the slopfile agent.

search: DuckDuckGo's lite endpoint, no API key, no account, no signup.
fetch:  plain HTTP GET, with the HTML reduced to readable text.

Both subcommands print plain text (not JSON) so results land in the context file
already condensed. Every network call is bounded by a timeout and the output is
capped, because a search result page or an article can be arbitrarily large and
this runs inside a session with a token budget.
"""

import argparse
import html
import re
import sys
import urllib.parse
import urllib.request

UA = "Mozilla/5.0 (X11; Linux x86_64)"
TIMEOUT = 45
MAX_RESULTS = 10
MAX_FETCH_CHARS = 40_000

DROP_TAGS = ("script", "style", "nav", "footer", "header", "svg", "noscript", "form", "iframe")


def fetch(url, timeout=TIMEOUT):
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    return urllib.request.urlopen(req, timeout=timeout).read().decode("utf-8", "replace")


def strip_html(doc):
    """Reduce a page to readable text, keeping link targets so they stay citable."""
    for tag in DROP_TAGS:
        doc = re.sub(rf"<{tag}\b.*?</{tag}>", " ", doc, flags=re.S | re.I)
    doc = re.sub(r"<!--.*?-->", " ", doc, flags=re.S)
    doc = re.sub(r"<br\s*/?>|</p>|</div>|</li>|</tr>|</h[1-6]>", "\n", doc, flags=re.I)
    doc = re.sub(r"<li\b[^>]*>", "- ", doc, flags=re.I)
    doc = re.sub(r"<h([1-6])\b[^>]*>", lambda m: "\n" + "#" * int(m.group(1)) + " ", doc, flags=re.I)
    doc = re.sub(
        r'<a\b[^>]*href="([^"#]+)"[^>]*>',
        lambda m: f"[link: {m.group(1)}] ",
        doc,
        flags=re.I,
    )
    doc = re.sub(r"<[^>]+>", "", doc)
    doc = html.unescape(doc)
    doc = re.sub(r"[ \t]+", " ", doc)
    return re.sub(r"\n\s*\n\s*\n+", "\n\n", doc).strip()


def search(query, limit=MAX_RESULTS):
    body = urllib.parse.urlencode({"q": query}).encode()
    req = urllib.request.Request(
        "https://lite.duckduckgo.com/lite/", data=body, headers={"User-Agent": UA}
    )
    doc = urllib.request.urlopen(req, timeout=TIMEOUT).read().decode("utf-8", "replace")
    # The title text and the href live in the same anchor; href must be captured
    # separately because the two are not in a fixed order.
    titles = re.findall(r"<a\b[^>]*class='result-link'[^>]*>(.*?)</a>", doc, re.S)
    hrefs = re.findall(r"<a\b[^>]*href=\"([^\"]+)\"[^>]*class='result-link'", doc, re.S)
    snippets = [
        html.unescape(re.sub("<[^>]+>", "", s)).strip()
        for s in re.findall(r"class='result-snippet'[^>]*>(.*?)</td>", doc, re.S)
    ]

    out = []
    for i, raw_title in enumerate(titles[:limit]):
        if i >= len(hrefs):
            break
        url = html.unescape(hrefs[i])
        if url.startswith("//"):
            url = "https:" + url
        # DuckDuckGo wraps the real destination in a redirect; unwrap it.
        real = urllib.parse.parse_qs(urllib.parse.urlparse(url).query).get("uddg", [url])[0]
        title = html.unescape(re.sub("<[^>]+>", "", raw_title)).strip()
        snippet = snippets[i] if i < len(snippets) else ""
        out.append(f"{len(out) + 1}. {title}\n   {real}\n   {snippet}")
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)

    s = sub.add_parser("search", help="search the web; prints numbered results")
    s.add_argument("query")
    s.add_argument("-n", type=int, default=MAX_RESULTS, help="max results")

    f = sub.add_parser("fetch", help="read one URL as text")
    f.add_argument("url")
    f.add_argument("--raw", action="store_true", help="skip HTML stripping")
    f.add_argument("--chars", type=int, default=MAX_FETCH_CHARS)

    args = ap.parse_args()
    try:
        if args.cmd == "search":
            results = search(args.query, max(1, min(args.n, MAX_RESULTS)))
            if not results:
                print("no results (DuckDuckGo may be rate limiting; wait and retry)")
            else:
                print("\n\n".join(results))
        else:
            doc = fetch(args.url)
            if not args.raw:
                doc = strip_html(doc)
            if len(doc) > args.chars:
                doc = doc[: args.chars] + f"\n\n[truncated at {args.chars} chars]"
            print(doc)
    except Exception as exc:  # a failed call must read as a plain fact, not a traceback
        print(f"{args.cmd} failed: {exc}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()