---
name: research-report
description: Research a topic, market, product or person from several sources and write a sourced report; never invent numbers.
---

Use when the user asks to research, compare, look into or write a report about something.

## Steps
1. Restate the question in one line and list the 3-6 sub-questions the report must answer. Ask only if the topic is truly ambiguous.
2. For each sub-question run `web_search` (2-3 different phrasings), then `web_fetch` the 1-3 most relevant pages. Prefer primary sources: official sites, filings, docs, reputable press.
3. Keep notes as you go: claim, number, source URL, date.
4. Write the report in Markdown:
   - a 3-5 line summary with the answer first
   - one section per sub-question
   - every number and specific claim followed by its source `[1]`, with a numbered source list at the end
   - a short "What I couldn't verify" section
5. Long reports: write them to a file in the workspace (`write_file report-<topic>.md`) and send it with `send_file`; give the summary in the chat.

## Rules
- A number with no source is labeled **estimate** and says what it's based on. Never present a guess as fact.
- Say how fresh the data is (year/month). Markets and prices change.
- Conflicting sources: show both, say which you trust more and why.
- Stop when the sub-questions are answered; more searching is not more value.
