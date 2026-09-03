# subbier — agent guide

## Tooling

- **jj** (jujutsu) for version control, not git.
- **runes** for issues, decisions and everything docs/ does not hold. Run
  `runes quickstart`.
- `cargo build` / `cargo test` at the workspace root. `packaging/bundle.sh`
  builds `Subbier.app` for local use.

## Where writing goes

`docs/` holds three files and gains no more. They are the guiding docs — high
level, extracted from the decisions and the implementation, and kept true to
what is built:

- `docs/ARCHITECTURE.md` — how it is put together, and which shapes are
  load-bearing.
- `docs/PRINCIPLES.md` — the three rules subbier is built to.
- `docs/MENU-DESIGN.md` — the macOS menu, the one UX surface detailed enough to
  need its own file.

Everything else is a rune, where it has a state and can move ahead of the
implementation:

- `runes list all --kind decision` — load-bearing decisions with wide impact.
  Context, decision, consequences. Write one when a choice will constrain work
  that has not been specified yet; not for a choice inside a single feature.
- `runes list all --kind doc` — reference and research: verified provider API
  shapes, protocol detail, per-crate traps, ported-from notes, and anything
  specific to one feature or spike.
- `runes list --kind task` / `--kind bug` — the work itself.

A doc rune's status is meaningful. `wip` is a living reference expected to drift
as the world does; `closed:done` is a research input that has been fully
absorbed and is kept for the explanation, not the plan.

## Keeping the two in step

The guiding docs cite runes by id (`sub-abc`). When a decision changes, update
the rune and then check whether the guiding docs still describe what is built.

Do not copy a rune's contents into `docs/`, and do not add a fourth file to
`docs/` without deciding it belongs in the guiding set.
