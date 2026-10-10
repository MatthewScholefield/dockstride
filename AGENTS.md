# Dockstride agent guide

## Core goal

Be the most USER FRIENDLY and agent-friendly devloop tool. Automate as much setup, development, and deployment work as safely possible, without magic. The tool, its configuration, and its behavior must remain simple to understand, inspect, and change.

## Product principles

- Optimize for the simplest useful experience, not the most features or abstractions. Prefer fewer steps, flags, concepts, and files.
- Handle routine work automatically with safe defaults. Ask only for decisions that genuinely require user intent.
- No magic: keep configuration in ordinary, inspectable files; make consequential actions and their reasons clear. Avoid hidden state or surprising side effects.
- Preserve explicit configuration and existing secrets. Reruns should be safe and predictable; destructive actions require explicit intent.
- Keep Docker Compose, Swarm, and Nickel semantics recognizable. Do not create an opaque parallel orchestration system.
- Keep default output concise and actionable. Interactive output should be easy to scan; non-interactive output should be frame-free and agent-friendly. Preserve structured JSON output and raw machine-consumable payloads.
- Put advanced detail behind verbose output or inspection commands. Errors should say what failed and how to fix it.
- When convenience conflicts with transparency or safety, choose the simplest behavior that preserves both.

## Repository layout

- `src/main.rs`: CLI arguments and command dispatch.
- `src/output.rs`: shared interactive, non-interactive, and JSON presentation.
- `src/config.rs`, `src/sources.rs`, `src/defaults.rs`, `src/allocations.rs`: YAML configuration, shared sources, setup defaults, and port allocation.
- `src/nickel.rs`, `src/model.rs`: Nickel evaluation, configuration contracts, and validated project models.
- `src/runtime.rs`, `src/deploy.rs`: Docker Compose lifecycle and Swarm deployment.
- `src/secrets.rs`, `src/state.rs`: secret provisioning and local state/locking.
- `src/commands.rs`, `src/status.rs`, `src/diagnostics.rs`, `src/environment.rs`: declared commands, readiness, diagnostics, and worktree inventory.
- `assets/`: bundled Nickel library and starter configuration; `examples/`: example projects.
- `tests/`: integration tests and fixtures, including mocked Docker behavior.
- `scripts/`: installation, release, and smoke-test tooling.
- `vendor/`: patched dependencies; change only when necessary.
- `docs/`: existing documentation; `target/`: generated build artifacts.

## Working conventions

- Match existing code style and conventions. Prefer targeted changes and existing files over new abstractions or files.
- Add regression tests for behavior changes. Prefer isolated fixtures and mocked Docker over modifying real projects or touching live services.
- Do not expose secret bytes in output, diagnostics, tests, or commits.
- Do not add code comments or documentation files unless requested.

## Completion requirements

- ALWAYS run relevant validation and build the release binary with `cargo build --release` after making changes.
- ALWAYS commit task-related changes before finishing, after validation and a successful release build.
- Leave unrelated changes untouched. Never commit secrets or credentials, skip hooks, or push unless explicitly instructed.
- Summarize changes, validation results, and the commit. If validation, the release build, or the commit is blocked, report the blocker clearly.
