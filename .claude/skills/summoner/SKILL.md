---
name: summoner
description: Run tool-agnostic coding orders in isolated Git worktrees.
---

# Summoner

Use this skill when a task is a good fit for parallel, isolated coding orders.
Keep the driver in charge of requirements, candidate selection, and integration.

## Workflow

1. Read the repository instructions and inspect the current Git tree. Confirm
   the order files and their scopes before dispatch.
2. Validate the orders:

   ~~~sh
   summoner check orders/
   ~~~

3. Inspect the dependency waves:

   ~~~sh
   summoner plan orders/
   ~~~

4. Check command availability and required environment variables:

   ~~~sh
   summoner doctor orders/
   ~~~

5. Run with a small concurrency value first. Add --stream when progress events
   are useful:

   ~~~sh
   summoner run --jobs 5 --stream orders/
   ~~~

6. Read the final JSON report. For every verified order, inspect the candidate
   commit and changed paths. A retained worktree is evidence to investigate,
   not a reason to delete it blindly.

## Order and config rules

Each order names id, title, brief, and scope. It may choose an executor,
verification profile, timeout, base, branch, acceptance items, and one parent.
Keep scopes narrow and make acceptance criteria observable. Use ordinary argv
commands in verification profiles. Summoner does not know which coding-agent
vendor an executor belongs to; configure the argv in the personal config.

The four commands are the complete interface: check, plan, doctor, and run.
Do not add a second orchestration path in a repository. If an order fails,
inspect its logs and candidate, then decide whether to retry or edit the order.
The driver independently reviews diffs, retries when useful, and integrates
selected commits; Summoner never merges them.
