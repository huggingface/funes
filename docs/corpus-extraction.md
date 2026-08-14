# Extracting a corpus from a memory

A worked example of building on funes: instead of a feature that selects sessions, delegate the
selection to an agent that uses the memory as its substrate.

The task below picks the sessions of one memory that are worth publishing — either because the work
landed, or because the session shows a claim of a reference document being argued — and safe to
publish. The result is a list of session ids.

This is a **spec**, not a procedure. It states what qualifies and what the answer must report.
Nothing here says how to find the sessions: that is the agent's problem.

## The prompt

```markdown
Extract from `<org>/<repo>` the sessions worth publishing as a public corpus.

A session qualifies if rule 1 holds, either form of rule 2 does, and rule 3
does not exclude it.

1. It is an <org>/<repo> session.

2a. It ended in a pull request that was merged into main.
2b. It illustrates a specific claim in <reference document> — shows the claim
    being reasoned about, decided, or demonstrated, not merely sharing its
    vocabulary.

3. Exclude the session when its evidence mentions or reveals any of:
   - an internal or non-public project, codename, repository, system,
     customer, or partner;
   - a private/internal discussion, strategy, negotiation, or decision;
   - an identifiable person in an internal or non-public context;
   - unannounced intent, roadmap work, planned action, or other non-public
     future activity.

   Do not infer that an ordinary open-source project name, public issue, or
   publicly documented person is internal merely because it is specific. Cite
   the exact evidence that creates or resolves the risk. Absence of a match is
   not clearance: report a session you could not clear as unverified, never as
   qualifying.

Output: the qualifying session ids, one per line, each naming the form of
rule 2 it qualifies under and what it shows — for 2a, the pull request it
produced; for 2b, the claim it illustrates and a one-line description. Then,
which claims found no session, which sessions rule 3 excluded and on what
evidence, and which you could not clear either way.
```

Adapt the memory, the project, the source of the claims, and what rule 3 treats as internal.
The shape carries over.
