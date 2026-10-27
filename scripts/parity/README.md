# Parity tests against stock rngit

Two scripts share one lab: `parity.py` covers git (below) and `parity_mgmt.py` covers the management calls.

`parity.py` runs one long scenario twice against the same stock rngit node: once through
the stock `git-remote-rns` and once through ours. The scenario covers clone (empty, after a
big push, one branch), push (new branch, fast-forward, tags, annotated tags, unicode and
slashed branch names, several refs, a 4 MB pack), refused and forced pushes, branch and tag
deletion, incremental fetch, `ls-remote`, missing repositories and missing branches.

It then compares, step by step, the exit code and every request the node received (taken from
the node's own log, written by `srvhook.py`), and finally the refs, history and `git fsck`
result of the node's repository and of each clone. It exits non-zero if anything differs.

This needs `pip install rns` (for the stock `rngit` and `git-remote-rns`), `cargo build --bins`,
and a lab laid out under `$PARITY_LAB` (default `/tmp/lab`):

- `srvrns/config`: a Reticulum config with `enable_transport = yes`, `share_instance = yes`
  and a `TCPServerInterface` on 127.0.0.1:4242;
- `srv/config`: the node config, with `public = <repos dir>` under `[repositories]` and
  `public = rw:all, c:all` under `[access]`;
- `cli/config` and `pycli/config`: Reticulum configs with a `TCPClientInterface` to that port,
  one for our client and one for the stock one;
- start the node with `RNS_CONFIG=$PARITY_LAB/srvrns TRACE_LOG=$PARITY_LAB/par/server.trace
  python3 srvhook.py --config srv --rnsconfig srvrns`, note the destination it prints, then
  `PARITY_NODE=<that hash> python3 -I parity.py r1`.

`trace_hook.py` only wraps documented public RNS calls to log their arguments.

Known, accepted difference: the top-level `have` list on `/git/fetch` is a hint. We send the tip
of every local ref (up to 256); stock sends a smaller selection. The node's reply and the
resulting repositories are identical, so `parity.py` ignores that list's contents.

Also checked by hand with the same two helpers, on a group with only `r:all`: clone works, and a
push and a branch delete are both refused at the listing, with the same requests and exit code.

## Serving

`rngit serve DIR r:all` hosts the repositories in `DIR` (`group/name.git`) from a client Reticulum
config. Clone from it with the stock `git-remote-rns` (use the `.git` suffix in the URL, as stock
sends the path as typed) and with ours. Pushing, `rngit create` and deleting a branch work the same way, with rules such as `"c:all, rw:all"`. Releases work too (`rel:all` lets everyone publish): stock's `rngit release` creates, lists, views, fetches, makes latest and deletes them, `rngit work` writes and reads work documents (the node checks their signatures) and `rngit perms` edits the rules of a repository and of a group.

## Management calls

`parity_mgmt.py` runs create, permissions, work documents, releases, fork and sync through the stock
`rngit` command and ours, against the same node, and compares the requests the node received and
the files it ended up with. Besides the lab above:

- put both client identities in the node's `[access]` line as admins, with `rel:all, i:all, p:all`
  for everyone, and set `PARITY_STOCK_ID` and `PARITY_OURS_ID` to their identity hashes (the file
  `client_identity` in each client config folder);
- set `PARITY_STOCK` to the folder with stock's `rngit` and `git-remote-rns`;
- `editor.sh` stands in for `$EDITOR`, which stock opens for every text it asks for.

Run `python3 -I parity_mgmt.py TAG`. Some flows differ on purpose: stock's `perms` always reads and
then writes through an editor, and `work edit` views the document first, so those steps compare only
the write request. Stock asks before a delete, so the script answers `y`. The script also covers
`release latest`, the saved manifest with offline `verify`, and `mirror` with `sync`.
