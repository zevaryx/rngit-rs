# Parity test against stock rngit

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
