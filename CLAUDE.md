# Star Fight — project instructions for Claude

## Verification workflow (user requirement)

ALWAYS delegate build/verification runs to a cheaper model instead of running
them inline: `cargo check`, `cargo build`, `cargo test`, `cargo clippy`,
`cargo fmt --check`, and similar grunt work go to a subagent
(`Agent` tool, `subagent_type: "general-purpose"`, `model: "haiku"`) that
returns only a compact summary — PASS/FAIL per step plus full error messages
with file:line for anything that failed. The main model reads the summary and
does the investigation and edits itself. This applies to every session; the
user has asked for it explicitly more than once.

When writing the subagent prompt, say explicitly: "You ARE the delegated
verifier: run the commands yourself with Bash; do not delegate further."
Otherwise the subagent reads this file and tries to re-delegate (it cannot
spawn agents) and returns nothing.

Small exception only when it clearly saves credits: a single-line check the
main model is already forced to wait on (e.g. `grep -c` over a one-off
command) — never full test/clippy/build runs.

## Policies

- Clippy warnings are errors: `cargo clippy --workspace -- -D warnings` must
  stay at zero. `sf-client` has a documented crate-level allow for
  `type_complexity` and `too_many_arguments` only (Bevy idiom).
- Frequent small commits, one concern each, with tests; deterministic dice via
  the `roll: &mut dyn FnMut() -> u8` injection (7 = blank, 0 = hit/evade).
- `NEXT-SESSION.md` is the resume document — read it first each session and
  keep it current at the end of a session.
- Game rules come from the user's `core_rules_en.pdf` (repo root, gitignored)
  and from chat; encode them in `sf-core` + RON data with tests.
