# rngit-rs

A Rust client for **rngit**, Git over [Reticulum](https://reticulum.network). It is a library
plus two programs:

- the **library** (`rngit`) has no UI and doesn't need a `git` binary: repository work is done
  in-process with [gix](https://github.com/GitoxideLabs/gitoxide), and the network side uses
  [rsReticulum](https://github.com/zevaryx/rsReticulum);
- **`git-remote-rns`** is a git remote helper, so plain `git clone`, `fetch` and `push` work with
  `rns://` URLs;
- **`rngit`** is a command line with a subcommand for every call the library makes. It shares its name with stock rngit's command, so install only one of them on `PATH`.

It is written to be embedded in any Rust application that needs to talk to rngit nodes, and it
follows stock rngit's behaviour request for request (see [Parity with stock rngit](#parity-with-stock-rngit)).

Requires Rust 1.89 or newer (edition 2024).

## What it does

| Area | Calls |
|---|---|
| Git | `list`, `fetch_bundle`, `fetch_bundle_with`, `fetch_into`, `fetch_objects`, `clone_into`, `mirror_into`, `push_from`, `push_from_with`, `push_all`, `push_bundle`, `delete_ref`, `update_ref` |
| Reading a copy | `local::log`, `local::tree`, `local::file` (`rngit log`, `tree`, `show` DIR REF): history, the files of a branch or tag, one file's text, from a repository on this computer such as the one `mirror_into` keeps |
| Repositories | `create`, `fork`, `mirror`, `sync` |
| Permissions | `repo_perms`, `group_perms`, `work_perms` and their `set_` calls |
| Releases | `releases`, `release`, `fetch_artifact`, `fetch_verified_release`, `create_release`, `delete_release`, `set_latest_release` (`release-latest`) |
| Checking a release offline | `rngit release-get URL TAG manifest.rsm DIR` saves the signed manifest; `rngit verify MANIFEST [DIR]` checks its signature and each file of it found in DIR, with no node |
| Hosting | `server::Node` answers `/git/list`, `/git/fetch`, `/git/push`, `/git/create` and `/git/delete` from a folder of `group/name.git` repositories, with the permissions of `.allowed` files; `serve::serve` puts it on Reticulum as a `git.repositories` node, and `rngit serve DIR [RULES]` runs one. Pushes, `create` and branch or tag deletion work, with size and time limits; releases, work documents, fork, mirror and sync are answered "not allowed". Stock `git clone` and ours both clone from it |
| Work documents | `work_list`, `work_create` (create or propose), `work_view`, `work_comment`, `work_edit`, `work_complete`, `work_activate`, `work_delete` |
| Finding nodes | `discovery::discover` listens for rngit announces |

Fetches only download what the local repository lacks. Pushes send a bundle, or just move the ref
(`update_ref`) when the node already holds the object. Annotated tags keep their tag object. `push_all` (`rngit push-all URL DIR [force] [prune]`) sends every branch and tag of a local repository, such as a mirror, to another node; a branch that isn't a fast-forward is reported and left alone unless forced, and `prune` also deletes what the node has and the copy doesn't.

`/git/delete` deletes a branch or tag (`delete_ref`). No request that deletes a whole repository
was found, so the library has none. Details are in [`docs/protocol.md`](docs/protocol.md).

## Use as a library

```rust
use rngit::{Client, RepoUrl};
use rngit::rns::RnsTransport;

let repo: RepoUrl = "rns://727ca184e3d7e83e0e35146940920442/public/demo".parse()?;
// `runtime` is an rsReticulum `ReticulumHandle`; `identity` is the long-lived client identity
// the node authorizes. `rngit::session::Session` starts both the way the programs below do.
let client = Client::new(RnsTransport::new(runtime, identity, repo.destination));
client.clone_into(&repo, std::path::Path::new("demo.git")).await?;
```

The protocol sits behind the `Transport` trait, so it can be driven without Reticulum
(`default-features = false` drops rsReticulum). The tests do this with an in-process node.

## Use with git

`cargo install --path .` installs `git-remote-rns` and `rngit`. Put them on `PATH`, then:

```
git clone rns://<destination hash>/<group>/<repo>
git push origin main
git push origin :old-branch
```

Settings, the same as for stock rngit:

| Variable | Meaning |
|---|---|
| `RNS_CONFIG` | The Reticulum config directory (default: Reticulum's own). |
| `RNGIT_CONFIG` | Where `client_identity` is kept (default `~/.rngit`). It is created on first use. |
| `RNGIT_PATH_WAIT` | Seconds to wait for the node's path before the first request (default 5). |

## Use from the command line

```
rngit nodes 60                                          # listen for nodes
rngit refs rns://<hash>/public/demo
rngit create rns://<hash>/public/demo
rngit delete-ref rns://<hash>/public/demo refs/heads/old
rngit work-new rns://<hash>/public/demo "Fix the thing" "Details"
rngit release-new rns://<hash>/public/demo v1.0 "Notes" app.tar.gz
```

`rngit help` lists every command. Releases and work documents are signed with the same client
identity git uses. The command logic is `rngit::cli`, so it is tested without a network.

## Safety

Everything a node sends is untrusted. Replies, bundles, artifacts and lists are size-capped
(`Limits`), parsing never panics, ref names from bundles must be valid names under `refs/`, a
bundle must carry the objects it builds on, artifact names with path separators are refused, and
a pushed ref is checked for fast-forward locally because the node's refusal can't be told apart
from a corrupt bundle. Release files are verified offline: signature, signer-equals-hash-of-key,
artifact hash and the manifest's notes hash. **Trusting the signer is the caller's decision.**

## Status

The protocol is implemented from [`docs/protocol.md`](docs/protocol.md), a clean-room spec
observed from stock rngit (its provenance is in section 0 of that file), and checked against
recordings of a real bundle, a real manifest and the requests a real client sent.

It has been run against live stock rngit nodes (rns 1.5.7) over a local Reticulum TCP link:
list, clone, create, fork, mirror, sync, incremental fetch, push (up to 40 MB), tags, branch and
tag deletion, the whole release cycle, the whole work document cycle, and permission reads all
work, and stock `git fsck` accepts the repositories on both sides. `examples/live.rs` drives
those calls; run it with `cargo run --example live -- <reticulum config dir> <rns:// url>`.

Known limits:

- Setting repository and group permissions answers `Not allowed` on the tested node, even for
  the repository's creator (stock Python gets the same). Reads work.
- Nodes reached over several hops and client aliases from `client_config` are untested.
- A node can't fork or mirror from itself; use another node as the source.

## Parity with stock rngit

- `tests/captured.rs` replays the 49 distinct requests recorded from a stock client and checks
  that rngit builds the same ones.
- `scripts/parity/` runs one long scenario twice against one stock node, through the stock git
  helper and through `git-remote-rns`: clone, push, tags, annotated tags, force pushes, deletes,
  incremental fetch, errors and a 4 MB push. The requests the node saw, the exit codes and the
  resulting repositories match step for step. See that folder's README to run it.

## Development

```
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo clippy --no-default-features --all-targets -- -D warnings
cargo test
```

CI runs these on Linux, macOS and Windows. Line coverage is kept at or above 85%
(`cargo llvm-cov`). Tests need a `git` binary. The parity script also needs `pip install rns`.

## Licence

AGPL-3.0-or-later, the same as rsReticulum, which it links against.
