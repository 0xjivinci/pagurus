---
name: art-collector
description: Research crypto art on Tezos for a collector: an artist's work and market, checking a piece before buying, watching for drops and price drops.
---

Use when someone asks about an artist, a piece, a collection or whether something is worth buying. Tezos only for now (objkt). For Ethereum, say it's coming.

## Rules
- Read-only. Never buy, bid, sign or ask for a seed phrase or private key. Give the objkt link so they act in their own wallet.
- Not financial advice: give facts and red flags, never "buy" or "sell". Don't pick a favourite or a "best bet": present options and let them choose.
- Say where data comes from (objkt) and that auctions and accepted offers are not in the sales numbers.

## Finding artists
When they ask who to collect or want suggestions:
1. `art` action=trending (days=7, or 30 for a calmer view): artists whose work sold to the most different collectors.
2. Present a short list with why each stands out (many collectors, resales, price range), and say it's activity, not quality. Sales that are almost all resales can mean demand or hype: say both, and suggest a piece check.
3. Ask what they like (style, price, editions vs 1/1s), then look up a few with action=artist.

## An artist
1. `art` action=artist with their tz… address, .tez name or objkt username.
2. Summarise: how often they mint, edition sizes, price range, primary vs secondary sales, who buys.
3. `art` action=collectors if they want to know who collects them.

## Before buying a piece
1. `art` action=piece with the objkt link or KT1…/id.
2. If they want to see it, `art` action=look shows you the preview: describe it honestly.
3. Go through every ⚠ line in plain words. Red flags to explain:
   - flagged account or token, unverified creator: confirm the address is the artist's own (their website or socials)
   - same title minted by other accounts: a possible copy
   - back-and-forth sales between the same wallets: possibly faked demand
   - asking price far above past sales
4. No flags means nothing obvious, not that it's safe.

## Watches (zero tokens)
Offer a heartbeat check instead of asking again later:
- new work: `` `pagurus art new <artist> 15` > 0 `` every 15m
- price drop: `` `pagurus art ask <piece>` < <tez> `` every 30m

## Their own collection
`art` action=holdings with their address. The sum of asks is not a valuation.
