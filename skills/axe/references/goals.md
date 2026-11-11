# Pursuing a goal

How a bot works toward a goal: propose it, then run a short loop of
Hypothesis -> Test -> Verdict until a stop condition holds. Each stage leaves
one durable record (a task or a trajectory event); write no reports between
stages. Commands are in `axe goal --help`; the design is `docs/design/goals.md`.

Your goals: `axe goal list --bot $AXE_BOT`. Read `axe goal show G` (budget,
linked tasks, latest trajectory) before creating any goal work.

## 1. Proposing a goal

Propose a goal for an outcome worth deliberate improvement, not routine
upkeep. Write `goal.json`:

```json
{
  "name": "bot-cost",
  "project": "<home project>",
  "intent": "## Outcome\n...\n\n## Evidence of progress\n...\n\n## Guardrails\n...\n\n## Autonomy tier\n...\n\n## Stop conditions\n...",
  "deadline": "2026-12-31",
  "budget_cost_week": 20,
  "budget_attention_week": 3,
  "bots": []
}
```

Intent sections:

- **Outcome**: what is true when the goal is met.
- **Evidence of progress**: a metric with its source when one is meaningful;
  a qualitative signal is fine otherwise.
- **Guardrails**: what must not get worse. Always include anti-gaming:
  reclassifying, suppressing or hiding work never counts as progress.
- **Autonomy tier**: which hypotheses you may start without asking (see 3).
- **Stop conditions**: when to stop and report (see 5).

Budgets are weekly: `budget_cost_week` in USD, `budget_attention_week` in
owner interventions. `bots` lists other bots to attach (needs `--owner-ref`);
you are attached automatically.

Run `axe goal propose --file goal.json`, then send the owner one Go card.
Only the owner activates (`axe goal activate`); never activate or edit intent,
deadline or budget without the owner's explicit word, cited with `--owner-ref`.

## 2. Hypothesis -> Test -> Verdict

### Hypothesis

A falsifiable claim drawn from evidence: the suspected cause, the
intervention, the predicted outcome, the eval date and a kill criterion.

1. Check open H tasks first: `axe goal show G` lists linked tasks. At most 2
   open H tasks per goal, across all its bots.
2. Create the H task: `axe add --project P --goal G --title "H<n>: <claim>"
   --detail-file spec.md`. Its Verify is the outcome measurement at the eval
   date, separate from the delivery verification.
3. Record it: `axe goal record G --kind hypothesis --task ID "<claim>; predict
   <X> by <date>; kill if <Y>"`.

### Test

The H task runs under the project's normal delivery. After shipping, it
waits for the eval date (`axe wait ID --until <eval> --note "measure H<n>"`)
and then measures.
Book your own follow-up: `axe bot wake --at <eval> --goal G --note "eval H<n>"`.

### Verdict

At the eval date, compare prediction and result, then pick one:

- **keep**: the change stays.
- **revert**: undo it in a new task.
- **revise**: a new hypothesis citing the old one.

`axe goal record G --kind verdict --task ID "predicted X, got Y -> keep|revert|revise; <lesson>"`

Use `--kind observation` for evidence that will feed the next hypothesis.

## 3. Autonomy tier

Self-start an H task (state `pending`) only if all hold:

- the intent's autonomy tier allows it,
- it is reversible,
- it fits the remaining budget,
- it is not on the project's "ask the owner first" list.

Otherwise create it with `--state escalated`, then `axe escalate ID --class
decision ...` so it shows as one Go/Skip on the board. Never self-approve.

## 4. Budget

Read `axe goal show G` before creating work. The server refuses new
goal-linked tasks once a weekly budget is spent. Ask the owner to raise a
budget only with evidence of value (kept verdicts, measured delta), never just
to keep going.

## 5. Stop

Stop when any holds:

- the target is met,
- the deadline has passed,
- two consecutive verdicts were revert,
- no hypothesis is worth testing.

Then record a scorecard, `axe goal record G --kind note "<scorecard>"`:
hypotheses tried and kept, measured delta against the guardrails, attention
and cost spent. Send it to the owner as one digest item. Close with the
owner's word: `axe goal done G --why "..." --owner-ref "msg #N"`, or
`axe goal pause G` likewise.

## 6. Several bots on one goal

A goal has one shared trajectory. Read it (`axe goal log G`) before
proposing a hypothesis, cite other bots' H tasks instead of duplicating them,
and count their open H tasks toward the limit of 2.
