---
name: consolidate
description: Store this session's work in Proxiima. Decide whether it established anything new, remember the observations no Fact holds yet, then derive an Abstraction for each conclusion. Use at a natural break, after the context was compacted, or when the user asks to consolidate.
---

# Consolidate

The server's `proxiima://how-to` resource documents the tools and handles
named here. This skill only puts them in order.

1. **Decide what is new before writing anything.** Name what the session
   established that no memory you read already states: a measured result, a
   decision, a cause found. If there is nothing, write nothing and say so.
   Why: restatements bury what matters. Example: three failed builds are new;
   that the repository is written in Rust is not.
2. **Remember what is missing.** For each new observation that no Fact holds
   yet, call `core_remember`. Judgment: store what a later session could not
   re-derive from the repository. Example: "the user retired agent-memory in
   favour of the Proxiima plugins", not "edited run.py".
3. **Derive one Abstraction per conclusion.** For each conclusion from step 1,
   call `core_derive` with kind `Abstraction`. As `source_handles`, pass the
   Facts and Abstractions it rests on: those from step 2, and those the
   session read or wrote earlier, which are in your tool results or in the
   summary if the context was compacted. Judgment: one conclusion per
   Abstraction, so a later session can cite or supersede each on its own.
   Example: "raising the memory limit did not stop the out-of-memory
   failures", not "three builds failed". The server accepts only Facts and
   Abstractions as origins and refuses an Abstraction without any, so a
   conclusion with nothing under it goes back to step 2.
