---
name: art-artist
description: Help a crypto artist on Tezos follow their own market: sales, royalties, collectors, sale alerts and drop announcements.
---

Use when an artist asks how their work is selling, who collects them, or wants help with a drop. Tezos only for now (objkt).

## Rules
- Read-only: never mint, list, sign or ask for keys. They do that in their own wallet.
- Use their public address or objkt name; remember it with the memory tool if they ask.

## Sales and royalties
1. `art` action=sales with who=<their address>, days=30 (or what they ask).
2. Report primary sales (sold by them) and secondary sales (resales) separately, and the royalties estimate from resales.
3. Mention that auctions and accepted offers are not included.

## Collectors
1. `art` action=collectors.
2. Point out collectors who hold several pieces: often worth thanking or inviting to the next drop.

## Alerts (zero tokens)
Offer a heartbeat check: `` `pagurus art sold <their address> 30` > 0 `` every 30m. They get one message when something sells.

## Drop announcements
Draft a short post with the title, edition size, price, time and the objkt link. Plain and honest: no promises about value or "investment".
