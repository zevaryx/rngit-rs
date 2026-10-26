"""Differential test: drive the same git commands through the stock rngit
helper and through rngit's git-remote-rns against one stock node, then
compare the requests the node saw, the exit codes and the final repositories.

Setup (see README.md in this folder), then:  python3 -I parity.py TAG
"""
import os, re, subprocess, sys, json, shutil, time, ast
LAB = os.environ.get("PARITY_LAB", "/tmp/lab")
D = os.environ["PARITY_NODE"]  # the node's git.repositories destination hash
TRACE = os.environ.get("TRACE_LOG", LAB + "/par/server.trace")
REPOS = os.environ.get("PARITY_REPOS", LAB + "/repos/public")  # the node's "public" group directory
OURS = os.environ.get("PARITY_OURS", os.path.abspath(os.path.join(os.path.dirname(__file__), "../../target/debug")))
HELPERS = {
  "stock": dict(PATH="/usr/local/bin:" + os.environ["PATH"], RNS_CONFIG=LAB + "/pycli", RNGIT_CONFIG=LAB + "/pycli-git"),
  "ours": dict(PATH=OURS + ":" + os.environ["PATH"], RNS_CONFIG=LAB + "/cli", RNGIT_CONFIG=LAB + "/rngit-cli", RNGIT_PATH_WAIT="2"),
}
BASE = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_SYSTEM="/dev/null", GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@t",
            GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@t", GIT_TERMINAL_PROMPT="0")

def trace_len():
    return len(open(TRACE, "rb").read())

def requests_since(pos):
    data = open(TRACE, "rb").read()[pos:].decode("utf8", "replace")
    out = []
    for line in data.splitlines():
        m = re.match(r"[\d.]+ REQ_IN path='([^']*)' data=(.*?) req_id=", line)
        if not m: continue
        out.append((m.group(1), norm(m.group(2))))
    return out

def norm(text):
    # The top-level "have" list is a hint; stock sends fewer tips than we do and the node's answer is the same.
    text = re.sub(r"'have': \[[^\]]*\]", "'have': [...]", text)
    text = re.sub(r"[0-9a-f]{40}", "<sha>", text)
    text = re.sub(r"b'(?:[^'\\]|\\.)*'|b\"(?:[^\"\\]|\\.)*\"", "<bytes>", text)
    return text

class Env:
    def __init__(self, helper, name):
        self.helper, self.name = helper, name
        self.env = dict(BASE, **HELPERS[helper])
        self.url = f"rns://{D}/public/{name}.git"
        self.server = f"{REPOS}/{name}.git"
        self.work = f"{LAB}/par/{name}"
        shutil.rmtree(self.work, ignore_errors=True)
        os.makedirs(self.work)
        self.log = []

    def server_init(self):
        shutil.rmtree(self.server, ignore_errors=True)
        f = f"{LAB}/par/create_{self.name}.txt"
        open(f, "w").write(repr([("/git/create", {0: f"public/{self.name}.git"})]))
        out = subprocess.run(["python3", "-I", os.path.join(os.path.dirname(os.path.abspath(__file__)), "create_repo.py"), f], capture_output=True, text=True, timeout=90)
        assert "b'\\x00'" in out.stdout, out.stdout + out.stderr
        subprocess.run(["git", "symbolic-ref", "HEAD", "refs/heads/main"], cwd=self.server, check=True)

    def sh(self, step, args, cwd=None, timeout=180):
        pos = trace_len()
        p = subprocess.run(args, cwd=cwd or self.work, env=self.env, capture_output=True, text=True, timeout=timeout)
        time.sleep(0.3)
        reqs = requests_since(pos)
        err = (p.stderr + p.stdout)
        self.log.append(dict(step=step, rc=p.returncode, reqs=reqs, err=err))
        return p

    def git(self, step, *args, cwd=None):
        return self.sh(step, ["git", *args], cwd=cwd)

    def local(self, *args, cwd=None):
        return subprocess.run(["git", *args], cwd=cwd or self.work, env=self.env, capture_output=True, text=True, check=True).stdout.strip()

    def state(self, cwd):
        return dict(refs=self.local("for-each-ref", "--format=%(refname) %(objectname)", cwd=cwd))

def server_refs(path):
    return subprocess.run(["git", "for-each-ref", "--format=%(refname) %(objecttype)", ], cwd=path, capture_output=True, text=True).stdout.strip()

def commit(env, repo, msg, fname=None, size=40):
    fname = fname or (msg.replace(" ", "_") + ".txt")
    open(os.path.join(repo, fname), "w").write((msg + "\n") * size)
    env.local("add", ".", cwd=repo)
    env.local("commit", "-q", "-m", msg, cwd=repo)

def scenario(helper, tag):
    e = Env(helper, f"par_{tag}_{helper}")
    e.server_init()
    w = e.work + "/w"; os.makedirs(w)
    e.local("init", "-q", "-b", "main", cwd=w)
    e.local("remote", "add", "origin", e.url, cwd=w)
    # 1 clone of an empty repo
    e.git("clone-empty", "clone", "-q", e.url, "empty")
    # 2 first push, ff push, tags
    commit(e, w, "one"); commit(e, w, "two")
    e.git("push-new-branch", "push", "-q", "origin", "main", cwd=w)
    commit(e, w, "three")
    e.git("push-ff", "push", "-q", "origin", "main", cwd=w)
    e.local("tag", "light", cwd=w); e.local("tag", "-a", "-m", "annotated", "ann", cwd=w)
    e.git("push-tags", "push", "-q", "origin", "light", "ann", cwd=w)
    e.local("branch", "feat/ünï", cwd=w)
    e.git("push-slash-unicode-branch", "push", "-q", "origin", "feat/ünï", cwd=w)
    # 3 clone and compare
    e.git("clone", "clone", "-q", e.url, "c1")
    # 4 a second clone gets new work incrementally
    c1 = e.work + "/c1"
    commit(e, w, "four")
    e.git("push-four", "push", "-q", "origin", "main", cwd=w)
    e.git("fetch-incremental", "fetch", "-q", "origin", cwd=c1)
    e.git("fetch-nothing-new", "fetch", "-q", "origin", cwd=c1)
    e.git("pull-ff", "pull", "-q", "--ff-only", "origin", "main", cwd=c1)
    # 5 diverged push rejected, then forced
    e.local("reset", "-q", "--hard", "HEAD~2", cwd=w); commit(e, w, "diverge")
    e.git("push-nonff-rejected", "push", "-q", "origin", "main", cwd=w)
    e.git("push-force", "push", "-q", "-f", "origin", "main", cwd=w)
    # 6 delete branch and tag
    e.git("delete-branch", "push", "-q", "origin", ":feat/ünï", cwd=w)
    e.git("delete-tag", "push", "-q", "origin", ":light", cwd=w)
    # 7 several refs at once
    e.local("branch", "a", cwd=w); e.local("branch", "b", cwd=w)
    e.git("push-two-branches", "push", "-q", "origin", "a", "b", cwd=w)
    # 8 big push (incompressible)
    open(w + "/big.bin", "wb").write(os.urandom(4 * 1024 * 1024))
    e.local("add", ".", cwd=w); e.local("commit", "-q", "-m", "big", cwd=w)
    e.git("push-big", "push", "-q", "origin", "main", cwd=w, )
    e.git("clone-after-big", "clone", "-q", e.url, "c2")
    # 8b more shapes
    e.local("tag", "-a", "-m", "second annotated", "ann2", cwd=w); e.local("tag", "v-late", cwd=w)
    e.git("push-all-tags", "push", "-q", "origin", "--tags", cwd=w)
    e.git("push-renamed", "push", "-q", "origin", "main:refs/heads/renamed", cwd=w)
    e.git("fetch-tags-prune", "fetch", "-q", "--prune", "--tags", "origin", cwd=c1)
    e.git("clone-branch", "clone", "-q", "--branch", "renamed", e.url, "c4")
    commit(e, w, "five"); commit(e, w, "six")
    e.git("push-two-commits", "push", "-q", "origin", "main", cwd=w)
    e.git("fetch-two-commits", "fetch", "-q", "origin", cwd=e.work + "/c4")
    # 9 errors: unknown repo, no such branch
    e.git("ls-remote", "ls-remote", e.url)
    e.git("clone-missing-repo", "clone", "-q", f"rns://{D}/public/does_not_exist_{tag}.git", "cm")
    e.git("push-missing-repo", "push", "-q", f"rns://{D}/public/does_not_exist_{tag}.git", "main", cwd=w)
    e.git("fetch-missing-branch", "fetch", "-q", "origin", "refs/heads/nope", cwd=c1)
    # 10 read-only repo
    open(e.server[:-4] + ".git.allowed", "w").write("r:all\n")
    e.git("push-readonly", "push", "-q", "origin", "a", cwd=w)
    e.git("clone-readonly", "clone", "-q", e.url, "c3")
    os.remove(e.server[:-4] + ".git.allowed")
    # final states
    final = dict(server=server_refs(e.server), fsck=subprocess.run(["git", "fsck", "--strict"], cwd=e.server, capture_output=True, text=True).returncode)
    for c in ("c1", "c2", "c3", "c4"):
        p = e.work + "/" + c
        if os.path.isdir(p) and os.path.isdir(p + "/.git"):
            final[c] = dict(refs=e.local("for-each-ref", "--format=%(refname)", cwd=p), log=e.local("log", "--format=%s", "origin/main", cwd=p) if c != "c3" else "", fsck=subprocess.run(["git", "fsck", "--strict"], cwd=p, capture_output=True, text=True).returncode)
    return e.log, final

def compare(result, tag):
    (sl, sf), (ol, of) = result["stock"], result["ours"]
    fix = lambda reqs: [(p, d.replace(f"par_{tag}_stock", "REPO").replace(f"par_{tag}_ours", "REPO")) for p, d in reqs]
    bad = 0
    for a, b in zip(sl, ol):
        same = fix(a["reqs"]) == fix(b["reqs"]) and a["rc"] == b["rc"]
        print(f"{a['step']:28} exit {a['rc']}/{b['rc']}  requests {'same' if same else 'DIFFER'}")
        if not same:
            bad += 1
            for (pa, da), (pb, db) in zip(fix(a["reqs"]), fix(b["reqs"])):
                if (pa, da) != (pb, db):
                    print("    stock:", pa, da[:300]); print("    ours :", pb, db[:300])
    for k in sf:
        if sf[k] != of.get(k):
            bad += 1
            print("FINAL STATE DIFFERS:", k, "\n  stock:", sf[k], "\n  ours :", of.get(k))
    print(f"{len(sl)} steps, {bad} differences")
    return 1 if bad else 0

if __name__ == "__main__":
    tag = sys.argv[1]
    result = {}
    for h in ("stock", "ours"):
        result[h] = scenario(h, tag)
    json.dump(result, open(f"{LAB}/par/result_{tag}.json", "w"), indent=1)
    sys.exit(compare(result, tag))
