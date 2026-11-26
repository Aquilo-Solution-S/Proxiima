---
name: consolidate
description: Store this session's work in Proxiima. Decide what it established, remember the observations no Fact holds yet, then derive an Abstraction for each conclusion they support. Use at a natural break, after the context was compacted, or when the user asks to consolidate.
---

# Consolidate

The server's `proxiima://how-to` resource documents the tools and handles
named here. This skill only puts them in order.

1. **Decide what is new before writing anything.** If your context was
   compacted, first find the session's earlier memories again: the summary
   names what the session worked on, and `core_search_memories` finds what it
   stored, with their handles. Keep only the results whose specifics (names,
   ids, numbers) match the summary: the search also returns other sessions'
   memories that merely resemble them, such as another build's failures. Then
   name what the session established that no memory you read already states:
   observations (a measured result, a decision, an error and its cause) and
   the conclusions they support together, drawn now if nobody drew them yet;
   Facts the session stored that no Abstraction covers are the usual source. A
   conclusion says what none of its observations says alone; observations that
   only happened in the same session support none. If there is nothing, write
   nothing and say so. Why: restatements and lists bury what matters. Example:
   that a test fails only when run in parallel is a conclusion; that the
   session fixed a typo and bumped a dependency is a list; that the repository
   is written in Rust is not new.
2. **Remember what is missing.** For each new observation that no Fact holds
   yet, call `core_remember`. Judgment: store what a later session could not
   re-derive from the repository. Example: "the user retired agent-memory in
   favour of the Proxiima plugins", not "edited run.py".
3. **Derive one Abstraction per conclusion.** For each conclusion from step 1,
   call `core_derive` with kind `Abstraction`. As `source_handles`, pass the
   Facts and Abstractions it rests on: those from step 2, and those from step
   1 that the session read or wrote earlier. Judgment: one conclusion per
   Abstraction, so a later session can cite or supersede each on its own.
   Example: "the test fails only when run in parallel", not "the test failed
   three times". The server accepts only Facts and Abstractions as origins
   and refuses an Abstraction without any, so a conclusion with nothing under
   it goes back to step 2.
