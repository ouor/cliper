"""End-to-end check of the `cliper serve` JSON-RPC contract (stdlib only).

Drives the whole pipeline the way the UI will:
create → audio.fetch → detect.run → (keep N songs) → clips.fetch → export.run,
validating responses and notification order along the way.

    python tools/ipc_e2e.py <vod url/id> --exe core/target/release/cliper.exe \
        --projects <dir> --models models [--songs 2]
"""
from __future__ import annotations

import argparse
import json
import os
import queue
import subprocess
import sys
import threading
import time


class Client:
    def __init__(self, cmd):
        self.p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=sys.stderr,
                                  text=True, encoding="utf-8", bufsize=1)
        self.next_id = 0
        self.responses: dict[int, queue.Queue] = {}
        self.events: queue.Queue = queue.Queue()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.p.stdout:
            msg = json.loads(line)
            if "id" in msg and msg.get("id") is not None and ("result" in msg or "error" in msg):
                self.responses.setdefault(msg["id"], queue.Queue()).put(msg)
            else:
                self.events.put(msg)

    def call(self, method, params=None, timeout=120):
        self.next_id += 1
        rid = self.next_id
        self.responses.setdefault(rid, queue.Queue())
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}}) + "\n")
        self.p.stdin.flush()
        msg = self.responses[rid].get(timeout=timeout)
        if "error" in msg:
            raise RuntimeError(f"{method}: {msg['error']}")
        return msg["result"]

    def wait_job(self, job_id, timeout=3600):
        t0 = time.time()
        last_print = 0
        while True:
            ev = self.events.get(timeout=timeout)
            m, prm = ev["method"], ev.get("params", {})
            if prm.get("job_id") != job_id:
                continue
            if m == "job.progress":
                pr = prm["progress"]
                if pr.get("message") or time.time() - last_print > 2:
                    last_print = time.time()
                    msg = pr.get("message") or f"{pr['done']}/{pr['total']}"
                    print(f"    [{prm['kind']}] {pr['stage']}: {msg}", flush=True)
            elif m == "job.done":
                print(f"    done in {time.time() - t0:.1f}s")
                return prm["result"]
            elif m == "job.failed":
                raise RuntimeError(f"job failed: {prm['error']}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("video")
    ap.add_argument("--exe", default="core/target/release/cliper.exe")
    ap.add_argument("--projects", required=True)
    ap.add_argument("--models", default="models")
    ap.add_argument("--songs", type=int, default=2, help="keep only the N songs after the first (0 = all)")
    args = ap.parse_args()

    c = Client([os.path.abspath(args.exe), "serve", "--projects", args.projects, "--models", args.models])
    ready = c.events.get(timeout=30)
    assert ready["method"] == "engine.ready", ready
    print("engine.ready:", json.dumps(ready["params"]["system"]["models"]))

    proj = c.call("project.create", {"input": args.video}, timeout=60)
    pid = proj["id"]
    print("project:", pid, "-", proj["source"]["title"])
    assert c.call("project.get", {"id": pid})["id"] == pid
    assert any(s["id"] == pid for s in c.call("project.list"))

    # error path: export before anything exists → PRECONDITION
    try:
        c.call("export.run", {"id": pid})
        raise AssertionError("expected an error")
    except RuntimeError as e:
        assert "PRECONDITION" in str(e), e

    if not proj.get("audio"):
        print("audio.fetch")
        c.wait_job(c.call("audio.fetch", {"id": pid})["job_id"])
    print("detect.run")
    res = c.wait_job(c.call("detect.run", {"id": pid})["job_id"])
    print("   ", res)

    proj = c.call("project.get", {"id": pid})
    songs = proj["songs"]
    for s in songs:
        print(f"    song {s['start']:8.1f}-{s['end']:8.1f} clip={s['clip_id']}")
    if args.songs:
        keep = {s["id"] for s in songs[1:1 + args.songs]}
        for s in songs:
            s["enabled"] = s["id"] in keep
        proj = c.call("songs.set", {"id": pid, "songs": songs})
    clips = [cl for cl in proj["clips"] if cl["video"] is None]
    print(f"clips.fetch ({len(clips)} clip(s))")
    c.wait_job(c.call("clips.fetch", {"id": pid})["job_id"])

    proj = c.call("project.get", {"id": pid})
    for cl in proj["clips"]:
        v = cl["video"]
        print(f"    clip {cl['id']}: {v['resolution']} vod_start={v['vod_start']} offset={cl['offset']:+.3f}s talk={len(cl['talk'])}")
    for s in proj["songs"]:
        if s["enabled"]:
            print(f"    song {s['id']}: {len(s['mutes'])} auto mute(s)")

    print("export.run")
    res = c.wait_job(c.call("export.run", {"id": pid})["job_id"])
    for e in res["exports"]:
        r = e["result"]
        loud = r.get("loudness") or {}
        print(f"    {r['path']} ({r['duration']:.1f}s, {r['video_codec']}, {loud.get('integrated', float('nan')):.1f} LUFS)")

    c.call("engine.shutdown")
    c.p.wait(timeout=10)
    print("OK")


if __name__ == "__main__":
    main()
