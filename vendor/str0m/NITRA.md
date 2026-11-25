# str0m 0.25.0, patched for nitra

A copy of [str0m](https://github.com/algesten/str0m) 0.25.0 from crates.io
(MIT OR Apache-2.0; used under MIT, see `LICENSE-MIT.txt`), wired in with
`[patch.crates-io]` in the workspace `Cargo.toml`.

Kept: `src`, `docs` (read by a unit test), the license and the readme. The
examples and integration tests were left out, and their `[[example]]` and
`[[test]]` entries removed from `Cargo.toml`.

## The change

`src/streams/register_nack.rs`: `MAX_MISORDER` 100 -> 1000 (packets whose
NACK retries are tracked). See the comment there. str0m 0.26.0 still has it
fixed at 100, so updating means applying this again (or upstream making it
configurable).
