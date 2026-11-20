---
name: art-taste
description: Learn a collector's taste in crypto art, keep their wishlist, and make the weekly shortlist of pieces within their budget.
---

Use when the user reacts to a piece ("love this", "not for me"), asks what they like, wants a wishlist, or for the weekly art proposals.

## Learning their taste
- Whenever they react to a specific piece, record it: `art` action=rate, piece, verdict love|like|pass, why (their words).
- After every few ratings, `art` action=taste, then rewrite the profile with action=taste note="…": 3–6 short lines
  on what they like and avoid (artists, styles, colours, subjects, edition sizes, price range), each backed by ratings
  or their collection. Replace it; don't append. Never invent a preference they didn't show.
- Their own collection counts: `art` action=holdings with their wallet.

## Wishlist
`art` action=wish (piece, note) · unwish · wishlist. Offer a price watch for wishlist pieces (see art-collector).

## Weekly proposals
1. `art` action=taste and action=budget. No ratings yet: ask them to rate a few pieces first, and stop.
2. Find candidates that match the profile: action=similar on artists they love, style with their tags, quiet and new.
   Skip artists and styles they passed on, and pieces already proposed or owned.
3. For each promising piece: action=piece (red flags, price), then action=look and judge it against the profile.
   Drop flagged pieces, likely copies and anything over the budget.
4. Record the best ones (aim for the picks_per_week in proposals): action=propose, piece, why (point at their
   ratings: "like X, which you loved"), fit 1–5. Be honest with fit: 5 only for a clear match.
5. Message the shortlist: title, artist, price, why, red flags, link. Ask them to rate each (that teaches the next round).
   They buy in their own wallet; nothing is bought for them.
- action=weekly schedules the round (default Sunday 10:00).

## Rules
Read-only. Not financial advice: say why it fits their taste, never that it will rise in value.
