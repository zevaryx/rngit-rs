# rngit wire protocol: observed specification (draft 1)

Purpose: a clean-room spec for a Rust client (and optionally a node) that interoperates with stock rngit.

## 0. Provenance and clean-room status

- Observed against stock `rns` 1.5.7 (`rngit`, `git-remote-rns`), run locally over a loopback TCP interface.
- Sources used: the public manual (<https://reticulum.network/manual/git.html>) and runtime behaviour only.
- No rngit or RNS source code was read. The trace hook (`captures/trace_hook.py`) wraps only documented public RNS calls (`Destination.register_request_handler`, `Link.request`, `Destination.__init__`) and logs their arguments.
- One disclosure: I opened the node's interactive console (`rngit -i`) once and printed the module-level names. That showed names such as `ReticulumGitNode`, `create_rsg` and `validate_rsg`. I looked at nothing else there and stopped.
- Later additions came from more runtime observation: probing a stock node with a plain RNS client, the differential runs in `scripts/parity/`, and the requests recorded from the stock helper.
- Everything marked **UNKNOWN** or **UNVERIFIED** below was not observed. Fill it in by probing, not by reading the Python.
- Test node settings: access via `[access]` and `.allowed` files, `loglevel`, no `node_name`.
- Captures are in `captures/`: `*.trace`, `fetch-bundle.bin`, `release-manifest.rsm`, `probe.py`.

## 1. Transport layer (standard Reticulum, not rngit-specific)

| Item | Observed value |
|---|---|
| Node destination | `SINGLE`, direction IN. App name `git`, aspect `repositories`. Full name `git.repositories`. |
| Destination identity | A dedicated "repository node identity", separate from the "Git Peer Identity" that `rngit -p` also prints. |
| Announce | Sent about 5 s after start, then every `announce_interval` minutes (default 360). Observed `app_data` was empty (`None`). `node_name` in config is documented as "included in announces", but setting it did not change `app_data` (still `None`) on rns 1.5.7, with the page node off. |
| URL scheme | `rns://<32-hex destination hash>/<group>/<repo>`. The hash is the 16-byte hash of `git.repositories`. |
| Client flow | Resolve a path (`Transport.request_path`), recall the identity, open a `Link` to the destination, call `link.identify(<client identity>)`, send requests, tear the link down. |
| Client identity | The client identifies with a long-lived identity (`client_identity`). The server uses `remote_identity.hash` (16 bytes, 32 hex) for permissions. The node's example config says a peer that does not identify can be blocked by listing `d7db22f63b453c23bb0688dde565b7c1` (the "null ident"). How that hash is derived and how unidentified peers are treated is **UNVERIFIED**. |
| Link requests | Standard RNS `Link.request(path, data)`. `data` is MessagePack-encoded by RNS. Large request bodies and responses become RNS Resources automatically. |
| Unknown request path | The request fails at RNS level (client failed-callback, status 0). |

Request handlers registered by the node (all with `allow=ALLOW_ALL` at the RNS level; authorisation is done by rngit itself):

```
/git/list  /git/fetch  /git/push  /git/create  /git/fork  /git/sync  /git/mirror  /git/delete
/mgmt/perms  /mgmt/release  /mgmt/work
```

(`/git/delete`: handler exists, request format not found, see 6.)

## 2. Application framing

### 2.1 Requests

The request data is a MessagePack map. Integer keys are used for the repository and group:

- Key `0`: repository path as `"<group>/<repo>.git"`. **The `.git` suffix is required.** `public/demo` returns Not found.
- Key `2`: group name, used only by group-level permission editing (see 5.3).
- All other keys are strings.

### 2.2 Responses

A response is a `bytes` object: **one status byte followed by an optional payload**. Either the whole response is one RNS response blob, or (for bulk data) an RNS Resource.

| Status | Meaning | Examples seen |
|---|---|---|
| `0x00` | OK | |
| `0x01` | Not allowed | `Not allowed` (permission denied for a known repo, for write-type operations) |
| `0x02` | Bad request | `No repository specified`, `Invalid request`, `Invalid request data`, `No refs specified`, `Missing ref specification`, `No source specified`, `Invalid step`, `Repository is neither fork nor mirror` |
| `0x03` | Not found | `Not found`, `Release not found` (also returned when a repo exists but the caller has no read permission, so repos stay hidden) |
| `0xff` | Server error | `Could not verify bundle`, `Failed to fetch from source: ...` |

After the status byte, an error payload is plain UTF-8 text with no terminator. A success payload is operation specific: raw text for `/git/list`, MessagePack for most `/mgmt/*` replies, empty for simple acks.

Exceptions to "one status byte then payload": see `/git/fetch` (2.3 below) and release `fetch`. Those reply with an RNS Resource whose body is the data, with no status byte observed.

### 2.3 Bulk responses (Resources)

For `/git/fetch` and release `fetch`, the response is an RNS Resource (a file). The server returns a tuple `(file, metadata)`. For `/git/fetch` the metadata observed was the map `{1: 0}`. **UNKNOWN**: meaning of key `1` (0 may mean "OK" or "format").

## 3. Git transport requests

### 3.1 `/git/list`

Request:

```
{0: "public/demo.git", "for_push": false}
```

Response: status `0x00`, then UTF-8 text, one line per ref:

```
<40-hex sha> <refname>\n
...
@<target refname> HEAD\n
```

Example:

```
\x00f8e8558727a95429ad4c65a02ed661ea84d15b78 refs/heads/main\n
f8e8558727a95429ad4c65a02ed661ea84d15b78 refs/tags/v1.0.0\n
@refs/heads/main HEAD\n
```

- The last line is a symref in the style of git's remote-helper `list`: `@<ref> HEAD` means HEAD points to `<ref>`.
- For an empty repo, only the `@refs/heads/master HEAD` line is returned.
- `for_push: true` returned the same data. The difference (if any) is permission checking: write permission is probably required. **UNVERIFIED**.
- Missing key `0`: `0x02 No repository specified`. Request not a map: `0x02 Invalid request`.
- No read permission or no such repo: `0x03 Not found`.

### 3.2 `/git/fetch`

Request:

```
{0: "public/demo.git", "refs": [{"sha": "<40 hex>", "ref": "refs/heads/main"}, ...]}
```

- `refs` is empty or missing: `0x02 No refs specified`.
- Response: an RNS Resource whose body is a **git bundle**, `# v2 git bundle` format.

The observed bundle was:

```
# v2 git bundle\n
<sha> refs/heads/main\n
\n
PACK....
```

- Prerequisites (`-<sha>` lines) were not seen in the initial clone. For incremental fetches the client probably sends `have` information somewhere. **UNKNOWN**: the request had no `have` field, so each fetch may be a full pack of the requested refs.
- The bundle can be unpacked with `git bundle unbundle`, or its pack fed to `git index-pack --stdin`.

### 3.3 `/git/push`

Request:

```
{0: "public/demo.git",
 "local_ref": "refs/heads/main",
 "remote_ref": "refs/heads/main",
 "force": false,
 "bundle": <bytes: git bundle v2>}
```

- `local_ref` and `remote_ref` must be full ref names. `HEAD:main` gave `0x02 Missing ref specification` from the stock helper because it forwarded `local_ref: "HEAD"`.
- `bundle` is a v2 git bundle with a prerequisite line for the server's current tip:

```
# v2 git bundle\n
-<old sha> <subject>\n
<new sha> HEAD\n
\n
PACK....
```

- The whole bundle travels inline in the request map, so RNS turns large pushes into an outbound Resource automatically.
- Success: `0x00` with empty payload. Updated the bare repo's ref.
- Invalid bundle: `0xff Could not verify bundle`.
- No write permission: `0x01 Not allowed`.
- Tags: `git push origin v1.0.0` sent `remote_ref: refs/tags/v1.0.0`, and the tag then appeared in `/git/list`.
- Non-fast-forward without `force`: a push whose bundle diverges from the server's tip is rejected with `0xff Could not verify bundle`. The same text is used for a corrupt bundle, so a client cannot tell the two apart from the reply. Check ancestry locally before pushing. With `"force": true` the same bundle succeeds and the ref moves (observed). Branch deletion uses `/git/delete` (3.7).

**What stock clients do (observed, and rngit does the same):** one `/git/list` with `for_push: true` per push, however many refs. For each ref, if the object being pushed is already on the node (a ref there points at it, or at a commit with it in its history) the helper sends the bundle-less `operations: [update_ref]` form (section 3.8) instead of a bundle. An annotated tag is pushed as the tag object, with the tag object in the pack, so the node keeps the tag. `git push origin :branch` becomes a `/git/delete` (section 3.7).

### 3.4 `/git/create`

Request `{0: "public/new.git"}`. Success `0x00`. Needs `c` on the group. Creates a bare repo (default branch `master`) and writes `<repo>.git.allowed` containing `adm:<creator identity hash>`. Without permission: `0x01 Not allowed`.

### 3.5 `/git/fork` and `/git/mirror`

Request:

```
{0: "public/forky.git", "source": "rns://<hash>/public/demo.git"}
```

- `source` may also be an HTTPS or SSH git URL (per the manual).
- The server creates the bare repo and fetches `+refs/*:refs/*` from `source`.
- For an `rns://` source it runs `git` with the installed `git-remote-rns` helper, so **the server's PATH needs the helper**. Without it: `0xff Failed to fetch from source: git: 'remote-rns' is not a git command...`.
- Fork and mirror differ only in the stored type: `repository.rngit.type = fork|mirror`, plus `repository.rngit.upstream.source` in the repo's git config.
- Missing `source`: `0x02 No source specified`.
- A local path or `file://` source is refused with `0x01 Prohibited source URL`.
- A node cannot fork from itself over `rns://`: its own helper cannot resolve a path to its own destination. Verified live with a second node as the source; fork, mirror and sync all succeed that way, and the new repo's `/git/list` matches the source.

### 3.6 `/git/sync`

Request `{0: "public/forky.git"}`. Success `0x00`. A repo that is neither fork nor mirror returns `0x02 Repository is neither fork nor mirror`. A missing repo returns `0x03`. The manual says it needs read and write permission.

### 3.7 `/git/delete`

Deletes one **ref**, not a repository:

```
{0: "public/demo.git", "ref": "refs/heads/feature"}
```

- Success `0x00` with empty payload, also when the ref does not exist. Observed to delete a branch, a tag, and `refs/heads/main` (the default branch is not protected).
- `ref` that is not a full ref name (`heads/x`): `0x02 Invalid request`. A missing repository: `0x03 Not found`.
- Earlier probing with only key `0` and guessed extra keys always gave `0x02 Invalid request` because `ref` was the missing key.
- No request that deletes a whole repository was found, and the manual does not describe one.

### 3.8 `/git/push` without a bundle

Instead of `local_ref`, `remote_ref`, `force` and `bundle`, a push may carry:

```
{0: "public/demo.git", "operations": [{"action": "update_ref", "ref": "refs/tags/t2", "sha": "<40 hex>", "force": false}]}
```

This points the ref at an object the node already has. Success `0x00`. An unknown object gives `0xff Object <sha> does not exist in repository`. Observed for a new tag and for moving `refs/heads/main`.

## 4. Release management: `/mgmt/release`

All requests carry `0: "<group>/<repo>.git"` and `"operation": <str>`.

| operation | Extra keys | Response |
|---|---|---|
| `list` | none | `0x00` + msgpack `{"releases": [ {tag, hash, created(int unix), status, created_by(hex), preview, format, artifacts(int)} ... ], "latest": <tag or nil>}` |
| `view` | `tag` | `0x00` + msgpack `{tag, hash, created, status, created_by, notes, notes_format, artifacts: [{name, size}], thanks: 0}` |
| `fetch` | `tag` (or `"latest"`), `artifact` (file name) | RNS Resource with the raw file body |
| `delete` | `tag` | `0x00` |
| `latest` | `tag` | `0x00`; makes that release the `latest` in `list`. A missing release gives `0x03 Release not found` |
| `create` | multi-step, below | |

Missing release gives `0x03 Release not found`. A bad operation gave `0x01 Not allowed` for an unprivileged caller.

`list` on a repository with no releases answers a bare empty array (`0x00 0x90`), not a map.

### 4.1 Creating a release (3 phases)

1. `{0, "operation":"create", "step":"init", "tag", "hash": <commit sha or null>, "notes", "notes_format":"markdown"}`
   - The tag must already exist on the server (pushed). Otherwise `0x02 Tag '<tag>' does not exist in repository`.
   - Success `0x00`.
2. For each file (and its `.rsg` signature file, and `manifest.rsm`), one request:
   `{0, "operation":"create", "step":"artifact", "tag", "artifact_name", "artifact_data": <bytes>}`
   - Observed order: `manifest.rsm`, then each `x.rsg`, `x`, and again in some order. The client resent some files, so the server must tolerate duplicates.
   - Each success is `0x00`.
3. `{0, "operation":"create", "step":"finalize", "tag"}` returns `0x00`, and the release is then `published`.

Needs the `rel` permission.

### 4.2 Signature files (`.rsg`) and manifest (`.rsm`)

Both have the same layout:

```
<64-byte Ed25519 signature> <MessagePack map>
```

- The signature is made with the signer's Reticulum identity key, **over the MessagePack bytes that follow it**. Verified: `Identity.validate(sig, body)` returned true, for both file types.
- `.rsg` body (per artifact):

```
{"hashtype": "sha256",
 "hash": <32 bytes, SHA-256 of the artifact file contents>,
 "meta": {"signer": <16 bytes identity hash>, "pubkey": <64 bytes>, "timestamp": <int unix>}}
```

- `.rsm` body (manifest):

```
{"hashtype": "sha256",
 "hash": <32 bytes>,
 "meta": {"signer", "pubkey", "name": "demo.git", "version": "<tag>",
          "released": "2026-10-26T05:33:28Z", "timestamp": <int>,
          "origin": <16 bytes node destination hash>, "path": "public/demo.git",
          "commit": "<sha>",
          "artifacts": [ {"name": "b.bin", "rsg": <bytes: the whole .rsg file>}, ... ]},
 "message": <bytes: release notes, UTF-8>}
```

- Manifest `hash` is the SHA-256 of the `message` bytes (the release notes). Found by testing candidates against `captures/release-manifest.rsm` (rngit-rs, `sign::verify_manifest` checks it). It is not the hash of the packed `meta` map.
- The signer's public key is carried in the files, so verification is offline. The client should also check that `signer` equals the hash of `pubkey`.
- The manifest lists only the non-signature artifacts. The release view listed 5 objects: manifest, 2 `.rsg`, 2 files.

## 5. Permissions and work documents

### 5.1 Permissions model (from manual and observation)

- Letters: `r w rw c s rel i p adm`. Targets: a 32-hex identity hash, `all`, or `none`.
- Sources: the node config `[access]` (per group), `<group>/group.allowed`, and `<repo>.git.allowed`. Per work document: `<repo>.git.work/<id>.allowed`.
- Default is no access.
- The node resolves repo rules first, then group rules, then admin override. Work-document rules come before repo rules.
- The server checks read permission before reporting a repo exists. Without it, a repo gives `0x03 Not found`, not `0x01`.
- A world-readable repo (`r:all`) served `/mgmt/release list` and `/mgmt/work list` to an unprivileged identity.

### 5.2 `/mgmt/perms` (repository)

Two-step edit of the repo's `.allowed` file:

```
{0: "public/demo.git", "operation": "rperms", "step": "get"}
 -> 0x00 + msgpack {"content": "<file text>"}
{0: "public/demo.git", "operation": "rperms", "step": "set", "content": "r:all\n"}
 -> 0x00
```

An unknown step returns `0x02 Invalid step`. A missing `operation` returns `0x02 Invalid request`. Needs `adm`. On the tested node (rns 1.5.7) a `set` from the repo's creator, who holds `adm` in the repo's `.allowed` file, got `0x01 Not allowed`, and group `get` and `set` did too; stock Python gave the same, so a node-level grant may be required. Reads of repo permissions work. The server validates syntax (the manual lists the checks). The exact error response for invalid content was **not captured**.

### 5.3 `/mgmt/perms` (group)

Same as 5.2 with `operation: "gperms"` and the **group** in key `2` (not key `0`):

```
{2: "public", "operation": "gperms", "step": "get"|"set", ["content": ...]}
```

### 5.4 `/mgmt/work`

All requests carry `0: repo` and `"operation"`.

| operation | Keys | Response |
|---|---|---|
| `list` | `scope`: `active`\|`completed`\|`proposed`\|`all` | `0x00` + msgpack `{"active": [doc...], "completed": [...], "proposed": [...]}` where doc = `{id, title, created(float), edited(float), author(hex), format, comments(int)}`. An unknown scope returned all three lists empty. |
| `create` | `title`, `content`, `format`, `signature` | `0x00` + msgpack `{"id": n, "scope": "active"}` |
| `propose` | same as `create` | `{"id": n, "scope": "proposed"}` (needs `p`) |
| `view` | `doc_id`, `scope` | `0x00` + msgpack `{id, scope, content, comments: [...], meta: {title, created, edited, author, identity(64 bytes), signature(64 bytes), format}}` |
| `comment` | `doc_id`, `scope`, `content`, `format` | `{"id": <comment id>}` (the `update` CLI command uses this) |
| `edit` | `doc_id`, `scope`, `content`, `title`, `signature` | `0x00` |
| `complete` | `doc_id` | `{"id", "scope": "completed"}` |
| `activate` | `doc_id` | `{"id", "scope": "active"}` |
| `delete` | `doc_id`, `scope` | `0x00` |
| `perms` | `doc_id`, `step`: `get`\|`set`, `content` (for set) | like 5.2: `{"content": ...}` then `0x00` |

- `id` is a per-repo integer counter, shared by all scopes.
- **Signature**: 64-byte Ed25519 signature by the author's identity **over the UTF-8 bytes of `content` only** (verified). The title is not signed. The view response returns the author's public key (`identity`) and the signature in `meta`. A client must verify with that key.
- Only the author may edit or delete (per the manual). The denial text was not captured.
- On-disk layout (informational, not part of the protocol): `<repo>.git.work/<scope>/<id>/...`, `<repo>.git.releases/`.

## 6. Open questions (probe before relying on them)

1. Meaning of the Resource metadata `{1: 0}` on `/git/fetch`.
2. ~~Whether `/git/fetch` supports incremental fetch.~~ Answered (observed from the stock helper's second fetch): each `refs` entry may carry `have` (the client's current sha for that ref), and the request may carry a top-level `have` list of the client's other tips. The reply is then a thin bundle with `-<sha>` prerequisite lines for what the client holds.
3. ~~`/git/delete` request format.~~ Answered (3.7): it deletes a ref. Whether repository deletion is exposed at all is still unknown; none was found.
4. ~~What the manifest `hash` covers.~~ Answered: SHA-256 of the release notes (section 4.2).
5. ~~Announce `app_data` content when `node_name` is set.~~ Answered: with `node_name` set and `announce_interval = 1`, rns 1.5.7 still announced `git.repositories` with `app_data` of `None`. The name is probably only used by the page node.
6. Size caps (max bundle, artifact, request, document size) and rate limits. None were hit. Any Rust implementation should set its own caps (cap everything that comes from the network).
7. Exact error texts for work-document author checks, invalid permission syntax, and non-fast-forward pushes.
8. Stats (`s`) permission and the stats pages are served only over the NomadNet page node. No request path for stats was seen in the node's handler list.
9. ~~Whether `/git/list` with `for_push: true` checks write permission.~~ Answered: yes. On a read-only group it answers `0x03` with the text `Not allowed` (a `0x03` status, not `0x01`), while `for_push: false` works. `/git/delete` and a bundle-less `/git/push` answer `0x01 Not allowed`. Stock and rngit helpers both fail the push at the listing, with the same exit code and requests.
10. Behaviour when two clients push the same ref at once.

## 7. Implementation notes

- Needed from rsreticulum: `SINGLE` out destination, announce/path handling, `Link` establish and `identify`, `Link.request` with MessagePack bodies, Resource receive and send (bundles, artifacts), and request responses that are Resources. Check each is available in the pinned rsReticulum revision before starting.
- Git side: use `gix` in-process (see section 9). Shelling out to system `git` is the fallback, but it needs a `git` binary and runs a program.
- Applications that must not run programs (a web UI, for example) can still offer clone and push, because `gix` runs in-process. Browsing and viewing releases and work documents can be in both.
- Identity: use a dedicated rngit client identity, never the user's main identity keys exposed to the web UI.
- Treat all fields as untrusted: cap response sizes, bundle size, text lengths, artifact names (reject path separators and `..`), and verify signatures and hashes before accepting release artifacts.
- Suggested phases: (1) list refs and clone/fetch, (2) push, (3) create/fork/mirror/sync and permissions editing, (4) releases with verification, (5) work documents.
- Test plan: run stock `rngit` in a test (like this lab) and drive the Rust client against it. `captures/probe.py` shows how to talk to a node from Python.

## 8. Lab setup (to reproduce)

1. `pip install rns msgpack` in a venv. Two Reticulum config dirs (server with `TCPServerInterface` on 127.0.0.1:4242, client with `TCPClientInterface`) and two rngit config dirs.
2. Server `[repositories]`: `public = <dir containing demo.git>`. Server `[access]`: `public = adm:<client identity hash>`. The client identity hash is printed in the server debug log (`Peer identified as ...`) or by `rnid -i client_identity -p`.
3. Server PATH must include the venv `bin` so `git-remote-rns` is found (for fork and mirror).
4. Run `rngit --config <srvdir> --rnsconfig <srvrns>`. Run the client with `RNS_CONFIG` and `RNGIT_CONFIG` set, e.g. `git clone rns://<repositories destination>/public/demo.git`.
5. `PYTHONPATH=captures` with `trace_hook.py` renamed to `sitecustomize.py`, and `TRACE_LOG=<file>`, logs every request and response on both sides.

## 9. gix spike results (Rust git library for the client)

Code: `gix-spike/` (`src/main.rs`, `Cargo.toml` trimmed features, `Cargo.default-features.toml` for comparison). gix 0.90, gix-pack 0.77, gix-odb 0.87. Run against the lab node from section 8. No `git` binary was used by the gix side; stock `git` was used only to check the results.

### 9.1 What was tested and passed

| Test | Result |
|---|---|
| List refs and HEAD symref from a bare repo | Same output as `/git/list`, including `@refs/heads/main HEAD`. |
| Ingest the bundle a stock rngit node served for `/git/fetch` | Parsed the v2 header, indexed the pack with `Bundle::write_to_directory`, set the ref. `git fsck` and `git log` clean. |
| Build a full v2 bundle (commit walk, `output::count::objects`, `entry::iter_from_counts`, `bytes::FromEntriesIter`) | `git bundle verify` accepts it. |
| Build an incremental bundle with a prerequisite line | `git bundle verify` accepts it. **Pushed it to stock rngit with `/git/push`: accepted, ref updated, `git fsck` clean.** |
| Large repo: 1500 commits, 40 files edited repeatedly plus binary blobs, 6.7 MB after `git gc` (deltas) | gix full bundle 6,086,416 bytes (real git: 6,086,471), built in 0.12 s. Ingested into a fresh repo in 0.49 s, 1500 commits, `git fsck` clean. |
| Thin pack: `git bundle create <old>..main` from real git (deltas against prerequisite objects) ingested by gix into a repo that has the base | Worked, ref set, `git fsck` clean, 3 new commits visible. Needs the repo's object database passed as `thin_pack_base_object_lookup`. |
| Non-fast-forward push to stock rngit with `force:false` and `force:true` | Server rejects and accepts respectively (see 3.3). |
| Cross-compile check (`cargo check`) for `x86_64-pc-windows-gnu` and `aarch64-apple-darwin` | Both compile. Run-time behaviour on Windows and macOS was not tested. |

### 9.2 Feature trimming

| Config | Packages in tree | Release build (clean) | Binary |
|---|---|---|---|
| Default features | 582 | 73 s | 5.85 MB |
| `gix` with `default-features = false, features = ["sha1"]` | 296 | 35 s | 4.70 MB |
| Plus `gix-pack` with `default-features = false, features = ["generate", "streaming-input"]` and `gix-odb` with defaults off | 296 | 35 s | 4.71 MB |

- The trimmed config still passes every test in 9.1. The only native-ish crate left is `linux-raw-sys`, which is Linux-only. Compression is `zlib-rs` (pure Rust). No `-sys` crates and no C compiler needed.
- The spike's own code uses: `gix::open`, `init_bare`, `references`, `find_reference`, `reference` (ref edit), `rev_walk`, and the `gix-pack` pack read/write functions. Nothing network-related (no HTTP, SSH or credentials code).
- Features beyond `sha1` were not needed for any of the above. Anything extra (revision parsing, status, blob-diff) would be added as needed.

### 9.3 Traps found

1. Before pack output, call `prevent_pack_unload()` on the object database handle, or it panics ("handle must be configured to prevent_pack_unload()").
2. Use a handle that implements `gix_pack::Find`: `repo.objects.clone().into_arc()?.into_inner()` (the unwrapped cache), not the in-memory proxy.
3. Pack entry iteration should be single-threaded (`thread_limit: Some(1)`) so entries stay in sequence order for `FromEntriesIter`. Parallel output needs re-ordering by sequence id.
4. `init_bare` refuses a non-empty directory. Open the repo first and only init when it does not exist.
5. Bundles built this way have no `HEAD` line. rngit's own push bundles do. A local clone from a gix-made bundle warns about an unborn HEAD. Harmless for rngit, since the node ignores it.
6. `Bundle::write_to_directory` can create an empty pack for an empty reply; the doc says to handle that. Not exercised here.

### 9.4 Still not tested

- Windows and macOS **run-time** behaviour (file renames while virus scanners hold files, long paths). Only compile checks were run.
- Repos far larger than the test one (hundreds of MB), memory use and progress reporting for them.
- Concurrent pushes and fetches against one repo.
- Shallow, partial or promisor repos, LFS pointers, submodules.
- Whether bundle thread-parallel output can be re-ordered correctly for speed.
- Repos using SHA-256.
- Interrupt and cancel handling during long ingests (`should_interrupt`).

### 9.5 Recommendation

Use gix, with the trimmed feature set above, in-process for all repository work. That removes the need to run `git` and lets an application offer browse, clone and push without running programs. The remaining work is the bundle glue (about 100 lines in the spike), local non-fast-forward checks, and size caps on every network input.

