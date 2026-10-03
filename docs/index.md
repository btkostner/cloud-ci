# cloud-ci documentation

Status: **Proposed and in development.** Most of what this site describes is design, not shipped
code — treat every page as a proposal until its linked ADR says `Accepted`, and see the
[roadmap](./roadmap) for what has actually landed so far.

cloud-ci is a CI system you deploy into **your own Cloudflare account**, with first-class GitHub
support. See [architecture](./architecture) for the full shape of the system: how the pieces fit
together, the data model, and the core run flows.

## Where to start

- [**Architecture**](./architecture) — the mechanism: system shape, packages, data model, core
  flows, coordination invariants.
- [**Roadmap**](./roadmap) — the phased build-out, ordered by risk, and what each phase has
  actually proven so far.
- **Design docs** and **decision records** are in the sidebar on the left — each design doc
  covers one feature end to end; each ADR records one decision and why, so a later change can see
  what it would be reversing.

## Source

This site renders the `docs/` tree from [github.com/btkostner/cloud-ci](https://github.com/btkostner/cloud-ci)
as-is — it is navigation and search over the same files you'd find in the repository, not a
separate copy. For the project overview, the commands to build it, and the full package layout,
see the [repository README](../README.md) and [AGENTS.md](../AGENTS.md).
