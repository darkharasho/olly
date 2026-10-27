# olly

olly is a tiny Rust background daemon for a Bazzite desktop that watches kernel memory-pressure signals (PSI) and per-process memory growth, and uses deterministic local rules to freeze, throttle, or kill runaway processes within seconds, before the system freezes. The Claude Code SDK is invoked on demand only, for ambiguous or slow-building issues and post-incident diagnosis, never in the emergency path. olly is a sibling to otto (a hotkey AI assistant): separate codebases, but olly exposes a small local IPC interface so otto can query its status and send simple commands.
