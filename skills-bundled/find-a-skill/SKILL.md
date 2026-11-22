---
name: find-a-skill
description: When no skill of yours fits a task, search the skills hub for one others wrote, and install it with the user's OK.
---

Use when a task needs know-how you don't have as a skill (a specific tool, file format, service or workflow), or when the user asks for a skill.

## Steps
1. Check your own skills in the system prompt first. A close match is better than a new install.
2. `skill_hub` action=search with 2-4 plain words (e.g. "pdf forms", "home assistant", "obsidian notes").
3. Pick the best 1-3 results. Prefer, in this order: **builtin** (Hermes' own), **trusted** (known GitHub repos), then **community**.
4. Tell the user what you found: name, what it does, trust level, and why it fits. For a community skill, say that nobody reviewed it.
5. Install only the one they want: `skill_hub` action=install with its `id`. The user is asked to approve; pagurus scans it for prompt injection first.
6. After installing, `skill_view` it and follow it for the task.

## Rules
- Never install several skills "just in case". One skill for one need.
- A skill that asks for API keys, sudo, or to send data somewhere: point it out before installing.
- If nothing fits, say so and do the task with your normal tools, or write your own skill afterwards with `skill_propose`.
- The user can undo any install with `pagurus learn undo <id>`.
