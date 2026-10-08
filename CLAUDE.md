# DecDVB — working notes

Read [`docs/DESIGN.md`](docs/DESIGN.md) first: it holds the agreed scope,
the crate layout and the milestone plan. Update it when a decision changes.

## Commands

```bash
cargo check --workspace --all-targets   # fast feedback
cargo test --workspace                  # unit tests
cargo clippy --workspace --all-targets -- -D warnings
cargo run --release -p decdvb-gui       # the GUI
cargo run --release -p decdvb-cli -- modcods
```

The live HackRF front end is behind `--features hackrf` (on `decdvb-io`, re-exported
by both apps), so a plain build never needs the HackRF SDK and CI stays green.

## House rules

- **Licence:** GPL-3.0-or-later. The project borrows algorithms from GPL-3 code
  (gr-dvbs2rx, leansdr, gr-dtv) and MIT code (dontlookup). Credit the source in a
  comment at the function or module that borrows from it.
- **Never commit** IQ captures, reference clones (`reference/`), or built exes.
- **Specs are the authority:** ETSI EN 302 307-1 / -2. Cite the clause or table
  number in comments next to constants, tables and bit layouts.
- The user has a strong DSP background but is newer to Rust: explain Rust idioms
  in comments, not DSP basics.
- DSP is unusably slow unoptimised, so `[profile.dev]` runs at `opt-level = 1`
  with dependencies at 3. Prefer `--release` for anything timing-sensitive.
- Heavy test sweeps belong on CI, not on the user's machine.

## Release

See the project memory: bump `[workspace.package].version` by matching the
`version = "…"` line, commit, wait for CI, annotated tag, push the tag; the
release workflow builds the portable static-CRT exes and drafts the release.
