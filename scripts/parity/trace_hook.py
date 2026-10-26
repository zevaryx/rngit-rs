# Observation hook: wraps only documented public RNS API entry points and logs payloads.
import os, sys, time, functools
LOG = os.environ.get("TRACE_LOG")
def dump(obj, tag):
    try:
        f = obj[0] if isinstance(obj, tuple) else obj
        if hasattr(f, 'name') and hasattr(f, 'read'):
            d = os.path.join(os.path.dirname(LOG), 'dumps'); os.makedirs(d, exist_ok=True)
            pos = f.tell(); f.seek(0); data = f.read(); f.seek(pos)
            fn = os.path.join(d, '%s_%d.bin' % (tag, int(time.time()*1000)))
            open(fn, 'wb').write(data)
            return "FILE %s %d bytes meta=%r" % (fn, len(data), obj[1:] if isinstance(obj, tuple) else None)
    except Exception as e:
        return "DUMPERR %r" % e
    return None
def log(*a):
    if LOG:
        with open(LOG, "a") as f:
            f.write("%.3f %s\n" % (time.time(), " ".join(repr(x) if not isinstance(x, str) else x for x in a)))
try:
    import RNS
    D = RNS.Destination
    orig_reg = D.register_request_handler
    def reg(self, path, response_generator=None, allow=None, allowed_list=None, auto_compress=True):
        log("REGISTER_HANDLER", path, "allow=%s" % allow)
        @functools.wraps(response_generator)
        def wrapped(p, data, request_id, *rest):
            remote_identity = rest[-2] if len(rest)>=2 else None
            log("REQ_IN", "path=%r" % p, "data=%r" % (data,), "req_id=%s" % (request_id.hex() if request_id else None),
                "remote=%s" % (remote_identity.hash.hex() if remote_identity else None))
            r = response_generator(p, data, request_id, *rest)
            log("RESP_OUT", "path=%r" % p, dump(r, "srv") or "resp=%r" % (r if not isinstance(r, (bytes, bytearray)) or len(r) < 600 else (len(r), r[:300]),))
            return r
        return orig_reg(self, path, wrapped, allow, allowed_list, auto_compress)
    D.register_request_handler = reg
    oi = D.__init__
    def di(self, identity, direction, type, app_name, *aspects, **kw):
        oi(self, identity, direction, type, app_name, *aspects, **kw)
        log("DEST", "dir=%s" % direction, "type=%s" % type, "name=%s" % ".".join((app_name,)+aspects), "hash=%s" % self.hash.hex())
    D.__init__ = di
    L = RNS.Link
    orig_req = L.request
    def req(self, path, data=None, response_callback=None, failed_callback=None, progress_callback=None, timeout=None):
        log("REQ_OUT", "path=%r" % path, "data=%r" % (data,))
        def rc(rr):
            log("RESP_IN", "path=%r" % path, dump(rr.response, "cli") or "resp=%r" % (rr.response if not isinstance(rr.response,(bytes,bytearray)) or len(rr.response)<600 else (len(rr.response), rr.response[:300]),))
            if response_callback: response_callback(rr)
        return orig_req(self, path, data, rc, failed_callback, progress_callback, timeout)
    L.request = req
    for name in ("set_packet_callback", "set_resource_callback", "set_remote_identified_callback"):
        pass
    log("HOOK_INSTALLED", sys.argv[0])
except Exception as e:
    log("HOOK_ERR", repr(e))
