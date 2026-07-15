# Why funes

This document explains the *why* behind funes — the design choices, and why it exists
alongside (not instead of) the growing field of "agent memory" providers. For the *what*
and the *how*, see the [README](../README.md).

## The problem

An LLM agent has the **goldfish problem**: when a session ends or the context window fills,
everything is gone. It meets your project as a stranger, again and again. The obvious fix is
to give it memory — but *how* you store and serve that memory is where every design parts
ways.

## The choices, and why

funes makes four deliberate, load-bearing choices. They are facets of one principle —
**defer to the reader**: keep the raw record intact, and let whoever reads it (you, or the
model) do the judging, interpreting, and asking. Each choice is a constraint we keep on
purpose, because it buys a property we value more than the flexibility we give up.

### 1. An append-only event log, not a mutable knowledge base

Every chunk is an immutable, timestamped record of *what was said when*. Nothing is ever
overwritten or "updated". When a fact changes, the old passage stays; the new one is simply
more recent.

**Why:** this is a problem funes chooses not to *have*, rather than one it solves. A mutable
store must decide at write time what each new piece of information supersedes — and every
wrong call loses information *silently*, by overwriting the right answer with a confident
wrong one. funes makes no write-time decisions at all. Every passage stays, and obsolescence
is resolved at **read time**: recency weighting, plus a reader (you, or the model) who can
see both the old and the new passage and judge. A log also keeps what a knowledge base
throws away — the superseded passage is often the answer itself: *what did we try before,
and why did we move off it?*

### 2. No LLM in the ingest path

Indexing is deterministic: parse → chunk → embed. No model summarizes, extracts "facts", or
rewrites anything. What you get back at recall time is the **actual passage**, with exact
provenance (`session_id` + `turn_uuid`), not a distilled paraphrase.

**Why:** putting an LLM in ingest means your memory is only as faithful as that model's
extraction, and the link back to the source turn is severed — you can no longer cite *where*
a "fact" came from or check whether it was distorted. Deterministic ingest keeps funes
**faithful** (you get the words that were actually written), **stable** (the same transcript
always indexes the same way), and **debuggable** (a bad result is traceable to a real line of
a real transcript).

There is also a bet on the future here. Distilling at ingest freezes your memory at the
interpretation quality of *today's* model — whatever it failed to connect or wrongly
summarized is what you are stuck with. Keeping the raw passages instead means the synthesis is
done by whichever model reads them, *later* — and models keep getting better at exactly this:
drawing inferences, and cross-referencing passages that a weaker model would never have linked.
A raw, append-only log compounds with model progress for free; a pre-distilled store can only
decay relative to it.

### 3. Local-first, model-agnostic, on storage you own

Local embeddings (`BAAI/bge-small-en-v1.5`) and a local cross-encoder reranker. The only
state is a derived, rebuildable store; the transcripts are the source of truth. Any model
can query it — switch models per task; nothing is trained into weights.

By default everything stays on the machine. **Optionally**, the store — a Lance dataset that
still holds the raw passages — can be synced to the **HF Hub**, where `recall` reads it
over `hf://` — fetching the immutable Lance files a query touches into a local cache pinned to
the dataset's commit, so a warm recall reads from disk with no network. That hub repo is
*yours*: your org, gated by your token. It is plain object storage, not a service — embedding
and reranking still happen locally; nothing on the hub processes, distills, or "learns" from
your data.

**Why:** memory of your work is among the most sensitive data you have, so funes never hands
it to a third party that runs a model over it. But "local" shouldn't mean "trapped on one
laptop": the optional hub tier lets you share a memory across your own machines or a team
*without* surrendering control of the data or changing what the data is — it's the same raw,
deterministic index, just hosted somewhere you own. Model-agnostic ingest means funes
outlives whichever model you use today.

### 4. Recall is pulled, not pushed

funes never volunteers memory. Recall is something the model (or you) *calls* — a query, when
there's a reason to look — and it returns nothing unless asked. There is no per-turn
injection, no memory block sitting permanently in the context window.

**Why:** the reflex elsewhere is to be always-on — prepend recalled snippets to every turn,
keep a resident memory block in the prompt. That spends context on guesses (a blind recall
against whatever the user just typed, relevant or not) and pre-empts the model's own judgment
about what matters right now. funes bets the model is the best judge of *when* its memory is
worth consulting — the same bet, applied to timing, that the other choices apply to
interpretation. Pulled on demand, context stays clean and recall happens for a reason.

## Why not just use a memory provider?

Agent-memory providers are now plentiful — frameworks like [hermes-agent](https://github.com/NousResearch/hermes-agent/tree/main/plugins/memory) bundle a
dozen. We surveyed the field. The specific tools and their feature lists churn monthly, but
the *paradigm* is remarkably uniform, and it is the opposite of funes on the choices above:

**They distill conversations with an LLM into a mutable store, inject it proactively, and
typically run as a managed service.** The consequences are structural, not incidental:

- **No provenance.** Because the original passage is distilled into a "fact", there is no
  link back to the verbatim turn it came from — you can't cite or audit the source.
- **Silent loss on reconcile.** A mutable store has to decide when new information supersedes
  old — when that call is wrong, it overwrites the right answer instead of keeping both.
- **Always on, never asked.** Recalled snippets are injected into every turn and a memory
  block sits resident in the prompt — the model never chooses when to consult it. The
  relevance call is made by a heuristic, not by the reader.
- **Your data, processed by someone else's model, on someone else's infrastructure.** Even
  the self-hostable ones put an LLM in the loop; the hosted ones also take custody of the
  data.

These are not flaws — they are a different product. An LLM-curated, reconciling knowledge
base is genuinely *better* than funes at **cross-session synthesis** and **reasoning over
entities and relationships**. If you want a system that answers "what does the user prefer?"
with a single synthesized statement and you need neither provenance nor data ownership, use
one.

funes optimizes for the opposite: **verbatim, auditable, local-first recall with exact
provenance, no LLM in the loop, pulled on demand rather than injected, and no dependency on
infrastructure you don't own** (sharing, when you want it, is to a hub repo you control).
It is a different contract with your data —
note that the *retrieval* machinery (hybrid vector + lexical search, fused and reranked) is
common to mature memory systems; funes does not differentiate on search. The difference is
entirely *upstream*: deterministic no-LLM ingest, immutable passages, and provenance.

This isn't a guess about search — it was measured. A run of recall-quality enhancements —
abstention thresholds, MMR diversity, near-duplicate collapse, semantic expansion, deeper
candidate pools, stronger rerankers, and a per-chunk *kind* facet, some at query time and
some computed at index time — was A/B'd against a labeled retrieval anchor; none moved
recall. The cross-encoder is the ceiling, and it already resolves a query's intent to the
right *kind* of passage without a stored label.

## Where this leaves funes

funes is the **verbatim recall layer**: the man who forgets nothing, with discrimination. It
is not a competitor to synthesis-and-graph memory — it is the auditable substrate such a
system could sit on top of. The two compose: funes hands back the exact passages with
provenance; a synthesis layer (your agent, or a provider) interprets them at read time.

Keep it local-first, keep it deterministic, keep the transcripts as the source of truth —
and when you share it, share it on storage you own.
