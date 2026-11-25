# Licensing

This repository (`Aquilo-Solution-S/Proxiima`) is licensed under the
**Apache License, Version 2.0** (see [`LICENSE`](LICENSE)).

## Scope of Apache-2.0 (This Repository Only)

Apache-2.0 applies to the open-core substrate and supporting artifacts
maintained in this repository:

- `apps/proxiima-mcp/` (canonical substrate-only MCP host)
- `crates/` (`proxiima-core`, `proxiima`, `proxiima-spiice`, storage,
  transport, auth, and testkit crates)
- `spiices/example/` (tutorial and test example spiice)
- `tools/` (local development helpers)
- `docs/` (documentation and Lean 4 `Causa` kernel specifications)

### Separate Commercial & Proprietary Repositories

Separate repositories and commercial products of **Aquilo Solution.S GmbH**
are **not** licensed under Apache-2.0 unless explicitly stated in those
repositories. This includes, in particular:

- **Proxiima Cloud & Web UX** (hosted service, live graph web application,
  workspace management, OAuth/billing control plane)
- **Centaurii / Uniiverse** (agent control plane, executor, mobile approvals,
  and keystore infrastructure)
- **Commercial Spiices & PlugiinStore** (proprietary domain/enterprise spiices
  and marketplace services)
- **Genuiine** (proprietary human-input attestation and trust products)

## What Apache-2.0 Means

For the contents of this repository, **Apache-2.0** allows anyone to:

- Use Proxiima for any purpose (commercial, non-commercial, internal, public)
- Modify the source code
- Redistribute original or modified versions
- Embed the open-core crates in proprietary hosts without an obligation to
  open-source derivatives

The standard Apache-2.0 patent grant applies strictly to the code in this
repository.

## Trademarks

In accordance with Section 6 of the Apache License, Version 2.0, this license
does **not** grant permission to use the trade names, trademarks, service
marks, or product names of **Aquilo Solution.S GmbH** — including **Aquiilo**
and **Proxiima** — except as required for reasonable and customary use in
describing the origin of the Work and reproducing the content of the NOTICE
file.

## Commercial Offerings

**Aquilo Solution.S GmbH** offers commercial products and services on top of
the open-core repository:

- **Hosted Service (Proxiima Cloud)** — Managed EU Proxiima instances and
  web platform
- **Commercial Spiices & PlugiinStore** — Proprietary domain spiices and
  certified integrations
- **Enterprise Add-ons & Agentic OS** — Centaurii / Uniiverse control plane,
  compliance packs, and separate proprietary trust integrations
- **Support & Sovereign Self-Hosting Services** — Enterprise SLA, Kubernetes
  self-hosting implementation, training, and consulting

For commercial inquiries, contact:

> Aquilo Solution.S GmbH — `heinrich.vonhelmolt@aquilo-solutions.com`

## Contributing & CLA

External contributions to this repository require signing the
[Contributor License Agreement (CLA)](.github/cla/CLA.md), granting
**Aquilo Solution.S GmbH** the right to distribute and relicense contributions
as part of open-source and commercial offerings. Mechanism:

1. Automated [CLA Assistant workflow](.github/workflows/cla-assistant.yml) on
   external pull requests
2. CLA text: [`.github/cla/CLA.md`](.github/cla/CLA.md) (you retain your
   copyright and grant Aquilo Solution.S GmbH a non-exclusive, irrevocable
   license, including relicensing rights)
3. DCO sign-off (`Signed-off-by:` trailer) is welcome but does not substitute
   for the CLA

## Why Apache-2.0 for the Core

Apache-2.0 removes legal adoption friction for developers and enterprises who
want to audit, embed, or self-host the core memory substrate (`Facts`,
`Abstractions`, `Perspectives`, `Goals`, provenance, and PostgreSQL RLS) while
commercial cloud UX, domain spiices, agent orchestration, and hardware trust
remain cleanly separated in Aquilo Solution.S GmbH's commercial offerings.
