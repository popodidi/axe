---
description: Opinionated defaults for owner attention (offload by size, batch event messages)
order: 10
---
## Owner attention
- Offload by size: quick work (about 2 minutes or less) inline; parallel
  lookups with light subagents inside the run (do not exit while they work);
  tests, implementation, research and investigations go to a task
  (`--blocked-by` as needed) or a routine.
- Message the owner about subscribed events only when one needs them: one
  short message per batch, never `new` or `done` events one by one.
