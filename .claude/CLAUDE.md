@../AGENTS.md

<!-- Claude-only additions go below; shared conventions live in AGENTS.md. -->

## Tracking Plan Execution

**When executing an implementation plan, check items off in the plan document as you complete them**
— the moment each one lands, not batched at the end of a phase or of the session. I read the doc
while you work; a plan updated only at the end tells me nothing while it still matters.

This holds regardless of execution mode. Inline execution, subagent-driven execution, worktrees,
anything else — the plan doc is checked off in flight either way. When a subagent does the work and
doesn't touch the doc, you check the item off yourself as soon as that subagent's task lands.

## Nothing Is Approved Until I Say It Is

**A proposal is not accepted until I explicitly approve it.** This holds for all design discussion
and brainstorming, and most of all inside the brainstorming skill.

- Silence is not approval. If I respond to a proposal without approving or rejecting it, it is still
  only tabled and I am most likely still discussing it.
- Never carry an unapproved idea forward as settled — not into a later message, a spec, a plan, or
  code.
- When you can't tell whether something is approved, ask me directly. The question costs a line;
  building on a decision I never made costs the whole branch.

## One Topic at a Time

**When working through a list of topics (issues, questions, features, review findings), don't move
off one until I explicitly move on or explicitly approve a proposal that fully addresses it.** A
proposal that resolves part of the topic does not close it.

- I frequently have addendums or adjacent points to raise before the next topic, so default to
  asking for my go-ahead rather than assuming.
- Skip the ask only when the topic is obviously finished — that exception exists to save tokens on
  the trivial cases, not to justify moving on early.
