# Crypto art (uge build)

[← back to the README](../README.md)

The big build, **uge**, can research crypto art for collectors and artists. It is read-only: it never connects a
wallet, never buys, bids, lists or signs anything, and never asks for a seed phrase or private key. When you want to
act, it gives you the objkt link and you do it in your own wallet. Nothing it says is financial advice.

**Tezos:** objkt's free public API, no account or key needed.
**Ethereum:** OpenSea's API. pagurus creates a free OpenSea key for agents by itself the first time (no account,
valid 7 days, renewed automatically) and stores it in `~/.pagurus/env`. Your own key in `OPENSEA_API_KEY` is used instead if you set one.

## Ask in plain words

- "Which Tezos artists should I look at?" · "Show me artists who went quiet" · "New generative artists?"
- "Who is collected by the same people as ilan_katin?"
- "Tell me about the artist oinky.tez"
- "Is this a safe buy? https://objkt.com/tokens/KT1…/182"
- "Who collects my work?" (after telling it your address)
- "How did my work sell this month, and what did I earn in royalties?"
- "What's in my collection?" (your public tz… address)
- "Find 10 pixel artists and make me a PDF of their best pieces with the stats"

Two bundled skills guide it: `art-collector` and `art-artist`. It can also look at a piece's preview image (`look`) when your model can see images.

## What a piece check covers

Artist and whether objkt has verified them, mint date, edition size, number of holders, royalties, current lowest
ask and recent sales. Then red flags to look at before buying:

- objkt has flagged the token or the creator account
- the creator isn't verified (check that it's the artist's own address)
- other accounts minted works with the same title (a possible copy)
- the same wallets sold the piece back and forth (possibly faked demand)
- the asking price is more than 3× the median sale

No red flags means nothing obvious showed up, not that a piece is safe.

## Reports (PDF)

`report` makes a PDF of chosen pieces, or of artists' best-selling works, and sends it with the reply. Each piece
gets its image (downloaded by pagurus and embedded, so the PDF never shows empty boxes) and its market numbers:
mint date, edition and holders, sold out or still open at primary (how many left, at what price), floor and how
many are listed, number of sales (all and last 30 days), volume, last and highest sale. A summary table comes first.
It is printed by Chromium, Chrome, Brave or Edge if one is installed; otherwise you get the HTML page. Files land in
`workspace/reports/`.

## Styles

`style` ranks artists whose recent work carries a tag by different collectors. When few turn up (a niche like pixel),
it widens the search: every spelling of the tag (pixel, pixels, pixelart…) and titles, any date, ranked by how many
different people collected that work.

## Commands

```sh
pagurus art trending [days] [n] [volume]   # most different collectors (or volume) lately
pagurus art quiet [silent-days] [n]        # sold to 3+ collectors in the past year, no sales since
pagurus art newcomers [started-days] [n]   # first mint recently, already 2+ collectors
pagurus art style <tag> [days] [n]         # e.g. generative, pixelart, photography
pagurus art similar <who> [n]              # collected by the same people
pagurus art search <name>                  # username, .tez name or X handle; "a/b" tries both
pagurus art collection <KT1…>              # a collection by its contract
pagurus art artist <who> [n]          # profile, recent works, sales of the last 90 days
pagurus art works <who> [n]           # works, newest first
pagurus art sales <who> [days]        # primary and secondary sales, royalties estimate
pagurus art collectors <who> [n]      # who holds their work
pagurus art holdings <who> [n]        # a wallet's collection
pagurus art piece <objkt link | KT1…/id>
pagurus art look <objkt link | KT1…/id>   # fetch the preview image
pagurus art report <pieces and/or artists…>  # PDF with images and market stats
```

`<who>` is a tz… address, a `.tez` name, an objkt username or X handle (matched loosely), or for Ethereum an 0x… address or OpenSea username
(add `--eth` to an OpenSea username). Ethereum alerts: `pagurus art eth-sold <artist> <minutes>`, `pagurus art eth-ask <piece>`.

## Alerts that cost nothing

These print a single number, so they work as [heartbeat checks](monitoring.md) with no model calls:

```
- [every 15m] `pagurus art new oinky.tez 15` > 0  new work by oinky
- [every 30m] `pagurus art sold tz1… 30` > 0     one of my pieces sold
- [every 30m] `pagurus art ask KT1…/182` < 5     this piece is listed under 5 ꜩ
```

## Limits

- Sales cover objkt: fixed-price sales, English and Dutch auctions, accepted offers and open editions. Free claims are left out.
- Ethereum: artists are found by OpenSea username or address (ENS names only if they have an OpenSea profile);
  collectors per artist and new-mint alerts are Tezos only for now; royalties shown are what was due, not what was paid.
- Data is objkt's and OpenSea's; if a service is slow, down or rate-limited, the tool says so.
