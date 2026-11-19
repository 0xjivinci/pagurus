# Crypto art (uge build)

[← back to the README](../README.md)

The big build, **uge**, can research crypto art for collectors and artists. It is read-only: it never connects a
wallet, never buys, bids, lists or signs anything, and never asks for a seed phrase or private key. When you want to
act, it gives you the objkt link and you do it in your own wallet. Nothing it says is financial advice.

**Supported now:** Tezos, through objkt's free public API (no account or key needed).
**Coming next:** Ethereum, which needs a free OpenSea or Alchemy API key.

## Ask in plain words

- "Which Tezos artists should I look at?"
- "Tell me about the artist oinky.tez"
- "Is this a safe buy? https://objkt.com/tokens/KT1…/182"
- "Who collects my work?" (after telling it your address)
- "How did my work sell this month, and what did I earn in royalties?"
- "What's in my collection?" (your public tz… address)

Two bundled skills guide it: `art-collector` and `art-artist`.

## What a piece check covers

Artist and whether objkt has verified them, mint date, edition size, number of holders, royalties, current lowest
ask and recent sales. Then red flags to look at before buying:

- objkt has flagged the token or the creator account
- the creator isn't verified (check that it's the artist's own address)
- other accounts minted works with the same title (a possible copy)
- the same wallets sold the piece back and forth (possibly faked demand)
- the asking price is more than 3× the median sale

No red flags means nothing obvious showed up, not that a piece is safe.

## Commands

```sh
pagurus art trending [days] [n]       # artists selling to the most different collectors lately
pagurus art artist <who> [n]          # profile, recent works, sales of the last 90 days
pagurus art works <who> [n]           # works, newest first
pagurus art sales <who> [days]        # primary and secondary sales, royalties estimate
pagurus art collectors <who> [n]      # who holds their work
pagurus art holdings <who> [n]        # a wallet's collection
pagurus art piece <objkt link | KT1…/id>
```

`<who>` is a tz… address, a `.tez` name or an objkt username.

## Alerts that cost nothing

These print a single number, so they work as [heartbeat checks](monitoring.md) with no model calls:

```
- [every 15m] `pagurus art new oinky.tez 15` > 0  new work by oinky
- [every 30m] `pagurus art sold tz1… 30` > 0     one of my pieces sold
- [every 30m] `pagurus art ask KT1…/182` < 5     this piece is listed under 5 ꜩ
```

## Limits

- Sales figures cover fixed-price sales on objkt. Auctions and accepted offers are not included yet.
- Data is objkt's; if objkt is slow or down, the tool says so.
