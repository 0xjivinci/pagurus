---
name: plan-a-project
description: Turn a big goal into small ordered tasks on the task board, with shared decisions written into every task.
---

Use when a job is too big for one answer: many steps, hours of work, or something to finish later in the background.

## Steps
1. Write the goal and what "done" looks like in 2-3 lines. Ask the user only about choices that change the plan.
2. Decide the shared choices up front: names, folders, file formats, tools, limits. Every task must repeat the ones it needs, because each task runs without this conversation.
3. Split the work into tasks a single agent run can finish (roughly 10 tool rounds each). Each task body says: what to do, inputs (paths), the expected output, and how to check it.
4. Order them: give a task a `parent` when it needs another task's output. Independent tasks can run side by side.
5. Add them with the `board` tool, then tell the user the plan as a short numbered list and how to follow it (`/board`).

## Rules
- Prefer 3-8 tasks. One giant task fails; fifty tiny ones waste runs.
- Put risky or approval-needing steps (installing, deleting, sending) in their own task so the user sees them.
- When a task fails, read its result before retrying; change the task, don't just repeat it.
