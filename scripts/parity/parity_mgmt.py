"""Differential test for the management calls: create, permissions, work
documents, releases, fork and sync. The stock `rngit` command and ours run the
same steps against one stock node, and the requests the node saw are compared.

Setup is the same as for parity.py (see README.md in this folder), then:
    python3 -I parity_mgmt.py TAG

Stock `rngit perms` always reads and then writes through an editor, while ours
reads or writes in one call each, so that step compares the "set" request only.
Stock opens $EDITOR for every text; this script points it at editor.sh, which
writes $EDIT_TEXT.
"""
import glob, importlib.util, json, os, re, shutil, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("parity", os.path.join(HERE, "parity.py"))
parity = importlib.util.module_from_spec(spec)
spec.loader.exec_module(parity)
LAB, D, STOCK_BIN = parity.LAB, parity.D, os.environ.get("PARITY_STOCK", "/usr/local/bin")
# The identities the two clients use; the node's config makes them admins.
ADMIN = {"stock": os.environ["PARITY_STOCK_ID"], "ours": os.environ["PARITY_OURS_ID"]}
CONNECT = "Requesting path|Path resolved|Establishing link|Link established"


def stock(env, *args):
    return [os.path.join(STOCK_BIN, "rngit"), args[0], "--rnsconfig", LAB + "/pycli", "--config", LAB + "/pycli-git", *args[1:]]


def ours(env, *args):
    return [os.path.join(parity.OURS, "rngit"), *args]


def norm(reqs, name):
    out = []
    for path, data in reqs:
        data = data.replace(name, "REPO")
        # Times, signatures and hashes belong to whichever identity ran.
        data = re.sub(r"'(timestamp|released|created|updated)': [^,}]*", r"'\1': <t>", data)
        data = re.sub(r"[0-9a-f]{32,64}", "<hex>", data)
        out.append((path, data))
    return out


class Run:
    def __init__(self, helper, tag):
        self.helper = helper
        self.e = parity.Env(helper, f"mg_{tag}_{helper}")
        self.name = self.e.name
        self.url = self.e.url
        self.e.env["EDITOR"] = HERE + "/editor.sh"
        self.build = stock if helper == "stock" else ours
        self.steps = []

    def cmd(self, step, stock_args, ours_args, text=None, only=None, cwd=None, confirm=False):
        """Run one step. `only` lists the request paths worth comparing, when the two flows differ on purpose."""
        self.e.env["EDIT_TEXT"] = text or ""
        args = (stock_args if self.helper == "stock" else ours_args)
        found = glob.glob(os.path.join(cwd or ".", "*.rsm"))  # the manifest a fetch left in the folder
        manifest = sorted(found)[0] if found else "missing.rsm"
        argv = self.build(self.e, *[a.replace("@URL", self.url).replace("@NAME", self.name).replace("@MANIFEST", manifest) for a in args])
        if confirm and self.helper == "stock":  # stock asks "Are you sure?" before a delete; ours doesn't
            argv = ["sh", "-c", 'printf "y\\n" | exec "$@"', "sh", *argv]
        p = self.e.sh(step, argv, cwd=cwd)
        entry = self.e.log[-1]
        entry["only"] = only
        return p

    def state(self):
        root = parity.REPOS
        files = sorted(os.path.relpath(os.path.join(d, f), root).replace(self.name, "REPO") for d, _, fs in os.walk(root) for f in fs
                       if self.name in os.path.join(d, f) and "/objects/" not in os.path.join(d, f))
        return files


def scenario(helper, tag):
    r = Run(helper, tag)
    e = r.e
    w = e.work + "/w"
    os.makedirs(w)
    e.local("init", "-q", "-b", "main", cwd=w)
    e.local("remote", "add", "origin", e.url, cwd=w)
    r.cmd("create", ["create", "@URL"], ["create", "@URL"])
    subprocess.run(["git", "symbolic-ref", "HEAD", "refs/heads/main"], cwd=e.server, check=True)
    parity.commit(e, w, "one")
    e.git("push-main", "push", "-q", "origin", "main", cwd=w)
    e.local("tag", "-a", "-m", "v1", "v1", cwd=w)
    e.git("push-tag", "push", "-q", "origin", "v1", cwd=w)

    r.cmd("create-again", ["create", "@URL"], ["create", "@URL"])
    group = "rw:all\nc:all\nrel:all\ni:all\np:all\nadm:" + ADMIN["stock"] + "\nadm:" + ADMIN["ours"]
    r.cmd("perms-group", ["perms", "rns://%s/public" % D], ["gperms", "rns://%s/public/@NAME.git" % D, group], text=group,
          only=["/mgmt/perms:set"])
    perms = "rw:all\nc:all\nrel:all\ni:all\np:all\nadm:" + ADMIN[helper]
    r.cmd("perms-set", ["perms", "@URL"], ["perms", "@URL", perms], text=perms, only=["/mgmt/perms:set"])

    # Work documents
    r.cmd("work-create", ["work", "@URL", "create", "-t", "First"], ["work-new", "@URL", "First", "Body one"], text="Body one")
    r.cmd("work-propose", ["work", "@URL", "propose", "-t", "Idea"], ["work-propose", "@URL", "Idea", "Maybe"], text="Maybe")
    r.cmd("work-list", ["work", "@URL", "list"], ["work", "@URL"])
    r.cmd("work-list-all", ["work", "@URL", "list", "--scope", "all"], ["work", "@URL", "all"])
    r.cmd("work-view", ["work", "@URL", "view", "-d", "1"], ["work-view", "@URL", "1"])
    r.cmd("work-comment", ["work", "@URL", "update", "-d", "1"], ["work-comment", "@URL", "1", "A comment"], text="A comment")
    r.cmd("work-edit", ["work", "@URL", "edit", "-d", "1", "-t", "Renamed"], ["work-edit", "@URL", "1", "Renamed", "New body"], text="New body", only=["/mgmt/work:edit"])
    r.cmd("work-complete", ["work", "@URL", "complete", "-d", "1"], ["work-complete", "@URL", "1"])
    r.cmd("work-list-completed", ["work", "@URL", "list", "--scope", "completed"], ["work", "@URL", "completed"])
    r.cmd("work-activate", ["work", "@URL", "activate", "-d", "1"], ["work-activate", "@URL", "1"])
    r.cmd("work-view-missing", ["work", "@URL", "view", "-d", "99"], ["work-view", "@URL", "99"])
    r.cmd("work-delete", ["work", "@URL", "delete", "-d", "1"], ["work-delete", "@URL", "1"], confirm=True)

    # Releases
    art = e.work + "/dist"
    os.makedirs(art)
    open(art + "/app.bin", "wb").write(b"artifact\n" * 50)
    r.cmd("release-create", ["release", "@URL", "create", "v1:" + art], ["release-new", "@URL", "v1", "Notes for v1", art + "/app.bin"],
          text="Notes for v1", cwd=w)
    r.cmd("release-list", ["release", "@URL", "list"], ["releases", "@URL"])
    r.cmd("release-view", ["release", "@URL", "view", "v1"], ["release", "@URL", "v1"])
    dl = e.work + "/dl"
    os.makedirs(dl)
    r.cmd("release-fetch", ["release", "@URL", "fetch", "v1:app.bin"], ["release-get", "@URL", "v1", "app.bin", dl], cwd=dl)
    r.cmd("release-view-missing", ["release", "@URL", "view", "v9"], ["release", "@URL", "v9"])
    r.cmd("release-latest", ["release", "@URL", "latest", "v1"], ["release-latest", "@URL", "v1"], confirm=True)
    r.cmd("release-latest-missing", ["release", "@URL", "latest", "v9"], ["release-latest", "@URL", "v9"], confirm=True)
    # Offline: stock leaves its manifest in the folder it fetched into; ours is asked for it.
    r.cmd("release-manifest", ["release", "@URL", "view", "v1"], ["release-get", "@URL", "v1", "manifest.rsm", dl], only=["/none"])
    r.cmd("release-verify-offline", ["release", "@MANIFEST", "verify"], ["verify", "@MANIFEST"], cwd=dl)
    open(dl + "/app.bin", "ab").write(b"tampered")
    r.cmd("release-verify-tampered", ["release", "@MANIFEST", "verify"], ["verify", "@MANIFEST"], cwd=dl)
    open(dl + "/app.bin", "wb").write(b"artifact\n" * 50)
    r.cmd("release-delete", ["release", "@URL", "delete", "v1"], ["release-delete", "@URL", "v1"], confirm=True)

    # Fork and sync
    fork = "rns://%s/public/@NAME_fork.git" % D
    r.cmd("fork", ["fork", "@URL", fork], ["fork", fork, "@URL"])
    r.cmd("sync", ["sync", fork], ["sync", fork])
    r.cmd("sync-plain-repo", ["sync", "@URL"], ["sync", "@URL"])
    mirror = "rns://%s/public/@NAME_mirror.git" % D
    r.cmd("mirror", ["mirror", "@URL", mirror], ["mirror", mirror, "@URL"])
    r.cmd("sync-mirror", ["sync", mirror], ["sync", mirror])
    return r.e.log, dict(files=r.state())


def compare(result, tag):
    (sl, sf), (ol, of) = result["stock"], result["ours"]
    sname, oname = f"mg_{tag}_stock", f"mg_{tag}_ours"
    bad = 0
    for a, b in zip(sl, ol):
        ra, rb = norm(a["reqs"], sname), norm(b["reqs"], oname)
        if a.get("only"):
            keep = lambda reqs: [x for x in reqs if any(x[0] == w.split(":")[0] and (f"'operation': '{w.split(':')[1]}'" in x[1] or f"'step': '{w.split(':')[1]}'" in x[1]) for w in a["only"])]
            ra, rb = keep(ra), keep(rb)
        same = ra == rb and (a["rc"] == b["rc"])
        print(f"{a['step']:24} exit {a['rc']}/{b['rc']}  requests {'same' if same else 'DIFFER'}")
        if a["rc"] and same:
            print("    both said:", a["err"].strip().splitlines()[-1:], "|", b["err"].strip().splitlines()[-1:])
        if not same:
            bad += 1
            for i in range(max(len(ra), len(rb))):
                x, y = (ra[i] if i < len(ra) else None), (rb[i] if i < len(rb) else None)
                if x != y:
                    print("    stock:", str(x)[:400]); print("    ours :", str(y)[:400])
            print("    stock said:", a["err"].strip().splitlines()[-1:] if a["err"].strip() else "")
            print("    ours said :", b["err"].strip().splitlines()[-1:] if b["err"].strip() else "")
    if sf != of:
        bad += 1
        print("FINAL STATE DIFFERS:\n  stock:", sf, "\n  ours :", of)
    print(f"{len(sl)} steps, {bad} differences")
    return 1 if bad else 0


if __name__ == "__main__":
    tag = sys.argv[1]
    result = {h: scenario(h, tag) for h in ("stock", "ours")}
    json.dump(result, open(f"{LAB}/par/result_mgmt_{tag}.json", "w"), indent=1)
    sys.exit(compare(result, tag))
