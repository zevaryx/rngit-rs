"""Send requests to the node as a plain RNS client. Usage: create_repo.py FILE,
where FILE holds a Python list of (path, request) pairs. Prints each reply."""
import RNS, sys, time, threading, ast
import RNS.vendor.umsgpack as umsgpack
import os
LAB = os.environ.get("PARITY_LAB", "/tmp/lab")
RNS.Reticulum(LAB + "/pycli", loglevel=2)
ident = RNS.Identity()
dest_hash = bytes.fromhex(os.environ["PARITY_NODE"])
RNS.Transport.request_path(dest_hash)
for _ in range(100):
    if RNS.Transport.has_path(dest_hash): break
    time.sleep(0.2)
rid = RNS.Identity.recall(dest_hash)
d = RNS.Destination(rid, RNS.Destination.OUT, RNS.Destination.SINGLE, "git", "repositories")
link = RNS.Link(d)
while link.status != RNS.Link.ACTIVE: time.sleep(0.1)
link.identify(ident)
def go(path, data):
    ev = threading.Event(); out = {}
    def ok(rr): out['r']=rr.response; ev.set()
    def fail(rr): out['r']=("FAIL", getattr(rr,'status',None)); ev.set()
    link.request(path, data, response_callback=ok, failed_callback=fail, timeout=15)
    ev.wait(20); r=out.get('r')
    print(path, repr(data)[:140], "->", repr(r)[:200] if not hasattr(r,'read') else "FILE")
for path, data in ast.literal_eval(open(sys.argv[1]).read()):
    go(path, data)
link.teardown()
