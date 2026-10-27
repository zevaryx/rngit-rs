# AGENTS.md

Guidance for AI coding agents working on rngit-rs, a Rust client library for
rngit (Git over Reticulum), plus the `git-remote-rns` remote helper and the
`rngit` command line. Read `README.md` for what it does and `docs/protocol.md`
for the wire protocol. rettui embeds this library as a submodule.

## Branches, commits and pull requests

- Work on `dev`. Pull requests go from `dev` to `main`.
- A large change may get its own branch off `dev`, named `feature/<name>`,
  and merge back into `dev` when it's done. Before starting a very large
  change, ask whether it should have its own branch and how to split it
  up. Don't push to any other branch unless asked.
- Don't open a pull request unless asked.
- **Always use [Conventional Commits](https://www.conventionalcommits.org/):**
  `type(scope): summary`, imperative mood, no trailing period. Use one of
  these types: `feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `build`,
  `ci`, `chore`. The scope is the area touched, for example `protocol`,
  `client`, `rns`, `helper`, `cli`, `discovery`, `deps`. Mark a breaking
  change with `!` and a `BREAKING CHANGE:` footer. The body says what changed
  and why.
- Never rewrite pushed history (no force-push, rebase or amend on `dev`).
  To bring in another branch, merge it.
- Never put AI model names or identifiers in commits, code, comments, docs
  or anything else committed.

## Before every push

Run these and make sure they pass. CI runs the same checks, and runs the
tests on Linux, macOS and Windows:

    cargo fmt                                            # rustfmt.toml: 140 columns, wide style
    cargo clippy --locked --all-targets -- -D warnings
    cargo clippy --locked --no-default-features --all-targets -- -D warnings
    cargo test --locked
    cargo deny --all-features check                      # if installed

- New code comes with tests. The tests drive the library against an
  in-process node (`tests/common`), so they need no network and no Reticulum.
- Code that only runs on Windows or macOS still has to compile there. When
  you touch it, run `cargo check --all-targets --target x86_64-pc-windows-gnu`.
- When CI fails, find and fix the root cause. Never skip, disable or
  quarantine a test. "Flaky" is not a diagnosis: find the race or timing
  assumption and fix it.

## Protocol and compatibility

- Stock rngit is the reference. Follow it request for request, so that
  nodes and clients written against it keep working. `scripts/parity`
  compares this library with stock rngit; run it when you change a request
  or a reply, and record what you learn in `docs/protocol.md`.
- Sideband, NomadNet and rngit itself are other people's projects. Read their
  formats, but never copy code from a project whose licence doesn't allow it.
- Treat everything that comes from the network (replies, refs, bundles,
  manifests, announces) as untrusted. Parse it without panicking, and cap
  sizes and lengths. Never trust a path, ref name or length a node sends.
- Check signatures before trusting content: release files are verified
  against the signed manifest.
- The library is embedded in other programs. Don't `unwrap`, `expect` or
  index on input from the network, and don't panic in library code. Return an
  error from the `Error` type instead.

## API and features

- The public API is used by rettui and other embedders. Keep changes
  backwards compatible where you can. Mark a breaking change with `!` in the
  commit, and bump the version for it.
- `reticulum` is a default feature. The protocol layer must keep building and
  testing with `--no-default-features`, behind the `Transport` trait.
- A new call goes in the library, then in the `rngit` command line as a
  subcommand, and in the table in `README.md`. A new request type is
  described in `docs/protocol.md`.
- The library has no UI and doesn't need a `git` binary: do repository work
  in-process with gix.

## Files and data

- When writing a file that matters (a cloned repository, a downloaded
  artifact), write it to a temporary name in the same folder and then rename
  it, so a failure leaves nothing half written.
- Keep identity keys private. Never log them or print them.

## Dependencies

- rsReticulum is pinned by `rev` in `Cargo.toml`, on purpose. rettui pins
  the same commit. Don't move it unless asked, and when it moves, move
  rettui's `deps/rsReticulum` pointer with it.
- Never open a pull request against an upstream project for a change made
  in a fork.
- Don't link system libraries that servers and Docker images don't have.
  Call an installed program instead.
- When cargo-deny reports an advisory, update the crate
  (`cargo update -p <crate>`). New licences must be allowed in `deny.toml`.
- gix is used with default features off. Add only the features you need.

## Code style

- Match the surrounding code: its naming, idioms and comment density.
- Comments are plain English. They say why, or what something is, briefly,
  in the same voice as the existing code. Don't restate the code.
- User-facing text (errors, command line help, docs) is plain and concrete.
  Say what happens and why, without jargon.
- Tests drive the library the way an embedder would. Give each test its own
  temp directory.

## Docs

- `README.md` covers the library, the programs and the parity story;
  `docs/protocol.md` covers the wire protocol. Update the pages a change
  affects in the same commit.
- Versioning: new features bump the minor version and breaking changes the
  major version, once the crate is 1.0 (before that, the minor version). The
  version in `Cargo.toml` must match the release tag.
