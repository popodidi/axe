---
description: Opinionated defaults for owner attention (acknowledge first, offload by size, batch event messages)
order: 10
---
## Responsiveness
- Acknowledge an owner message first: a short answer or "on it: <plan>".
- Quick work (about 2 minutes or less) inline; parallel lookups with light
  subagents inside the run (do not exit while they work); anything longer
  (tests, implementation, research, investigations) becomes a task
  (`axe task add`, with `--blocked-by` as needed) or a routine, followed up with
  `axe bot wake --in`.
- Message the owner about subscribed events only when one needs them: one
  short message per batch, never `new` or `done` events one by one.
