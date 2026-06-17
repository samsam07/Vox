# CONVENTIONS.md — Rust (portable)

Project-agnostic style. Travels to future Rust projects. vox-specific invariants
live in CLAUDE.md, not here.

## Errors
- Libraries/internal: `Result<T, E>` with a typed error (`thiserror`). Application
  edges (main, CLI parse): `anyhow` is fine.
- No `unwrap()` / `expect()` in non-test code paths that can fail at runtime.
  `expect()` is allowed only for genuine invariants that cannot fail if the program
  is correct, with a message saying why.
- Propagate with `?`. Don't swallow errors silently.

## Naming
- `snake_case` items/modules/files; `CamelCase` types/traits; `SCREAMING_SNAKE`
  consts.
- Names describe role, not mechanism. Match the canonical spelling of any already-
  named concept exactly (see CLAUDE.md anti-drift rule).

## Modules & files
- One responsibility per module. Prefer several small files over one large one.
- Keep `main.rs` thin: parse args, build config, start the engine, wait.

## Concurrency
- Prefer ownership handoff (channels, SPSC ring) over shared mutable state.
- Reach for `Arc<Mutex<>>` only when shared ownership is genuinely required — never
  to silence the borrow checker on a hot path.
- No blocking calls on real-time / callback threads.

## Comments
- Comment *why*, not *what*. No commented-out code in commits.
- Doc-comment (`///`) public items and any non-obvious invariant.

## Formatting & lint
- `rustfmt` default. `clippy` clean (warnings are work items, not noise).
- No `#[allow(...)]` without a one-line reason.

## Tests
- Unit tests beside the code (`#[cfg(test)]`). Integration tests in `tests/`.
- Test behaviour and edges, not implementation detail.

## Dependencies
- Add deliberately; prefer std. Pin sensible versions in `Cargo.toml`.
- Verify a crate is maintained before adopting (the PortAudio-binding lesson:
  check last-release date and open-issue health, not just that it exists).
