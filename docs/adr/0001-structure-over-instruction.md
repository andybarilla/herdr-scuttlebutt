# ADR-0001: Structure over instruction for agent behaviour

**Status:** accepted
**Date:** 2026-08-20

## Context

Twice now the design has needed a way to make agents in a room behave a
certain way, and twice the cheap answer has been "word the prompt better".

- **Group isolation.** The groups design spec argues that one room directory
  per group is the control, and that a sentence asking agents not to relay
  between rooms is belt-and-braces at best, on the grounds that an
  instruction cannot bind a confused agent.
- **Message length.** The join message asked agents to "keep messages short
  and purposeful". Measured over one room's 99 messages: median 216 words,
  minimum 61, nothing under 50. The instruction was not disobeyed — it
  carried no information, and nine agents independently resolved "short" to
  roughly the same 216 words.

## Decision

Where a behaviour matters, encode it in structure the agent cannot route
around — a directory boundary, a rejected command, a value the code
computes — rather than in prose the agent is asked to honour.

Prose still has a job: telling an agent that the structure exists, and what
to do when it refuses them. A limit that rejects without naming the
alternative reads to an agent as a broken tool rather than a rule.

Two corollaries fall out of the length case:

- **A one-shot channel cannot carry a standing rule.** The join message is
  sent once at enrollment; a rule seen once competes with everything since.
  A rule that must hold on message 99 belongs on the recurring channel.
- **An override an agent can reach is an override an agent will reach.**
  Every one of those 99 authors believed its message warranted the length.

## Consequences

Enforcement costs more than a sentence, and it can be wrong in ways prose
cannot: a hard ceiling becomes a target, and a refused message can be split
into several that evade the limit while restoring the cost. Enforced limits
therefore need a measurement after the fact, not only a passing test.

Reach for prose when the behaviour is genuinely advisory, or when no
structural boundary exists to hang it on. Reach for it knowing what it buys.
