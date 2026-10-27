## Project Context

olly is a tiny Rust background daemon for a Bazzite desktop that watches kernel memory-pressure signals (PSI) and per-process memory growth, and uses deterministic local rules to freeze, throttle, or kill runaway processes within seconds, before the system freezes. The Claude Code SDK is invoked on demand only, for ambiguous or slow-building issues and post-incident diagnosis, never in the emergency path. olly is a sibling to otto (a hotkey AI assistant): separate codebases, but olly exposes a small local IPC interface so otto can query its status and send simple commands.

## Goals

- Detect runaway memory early using PSI and per-process RSS growth rate, before the system locks up
- Act automatically within seconds using deterministic local rules (freeze via cgroup, throttle, or kill), with no network or LLM dependency in the emergency path
- Respect a never-kill allowlist and send a desktop notification whenever olly acts
- Use the Claude Code SDK on demand for ambiguous cases, slow leaks, and post-incident diagnosis; zero cost when idle
- Keep a very small, steady memory and CPU footprint so olly keeps working under extreme memory pressure
- Expose a small local-only IPC interface: status and incident history queries, simple commands (pause/resume, add to allowlist), and an event stream so otto can relay what olly did
- Keep an incident log for later review

## Out of scope

- Sharing a codebase with otto; they are separate projects that only talk over IPC
- Depending on otto: olly must work fully on its own
- Calling an LLM during the critical seconds of a memory emergency
- General-purpose system monitoring dashboards or metrics collection beyond what olly needs to protect the system

## Suggested stack

- **Rust** — Single small binary with predictable, low memory use; it can keep running when the system is nearly out of memory
- **Linux PSI + cgroups v2 (systemd user slices)** — Kernel memory-pressure signals for early detection; cgroup freeze/throttle gives gentler options than killing
- **systemd user/system service** — Native way to run a background daemon on Bazzite (immutable Fedora Atomic) and set OOMScoreAdjust so olly itself is protected
- **Claude Code SDK** — On-demand reasoning for ambiguous or slow-building issues and post-incident diagnosis
- **Desktop notifications (freedesktop/libnotify)** — Tell the user whenever olly acts
