# cupel

**cupel**

*noun*: a shallow, porous container in which gold or silver can be refined or assayed by melting with a blast of hot air which oxidizes lead or other base metals.

*verb*: assay or refine (a metal) in a cupel.

(Source: Oxford Languages)

Lean coding harness written in Rust, focused on fast local workflows, deterministic tooling, and efficient code retrieval. Leveraging [fff](https://github.com/dmtrKovalenko/fff), described as:
"The fastest and the most accurate file search toolkit for AI agents, Neovim, Rust, C, and NodeJS."
Dmitriy Kovalenko

Parts of the architecture are a cheap copy of [pi.dev](pi.dev), while others reflect my own development workflows and coding practices.

# Milestones

**Not necessarily in order**

- [ ] *Agent Core* Crate
  - [ ] Agent runtime - long-lived coordinator for one active session
  - [ ] Tool calling - definition-first and composable
  - [ ] State management - mutable runtime state, durable session history (append-only), config/service state
- [ ] *CLI* Crate - TUI component definition
- [ ] *Provider Abstraction* Crate
  - [ ] provider integration & configuration
  - [ ] context persistence and hand-off management
  - [ ] cost traking
