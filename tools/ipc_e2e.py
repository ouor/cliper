"""End-to-end check of the `cliper serve` JSON-RPC contract (stdlib only).

Drives the whole pipeline the way the UI will (protocol 3):
create → audio.fetch → detect.run → (keep N songs) → video.fetch {song_ids,
process} → stems.ensure (started by the engine) → export.run, then grows the
first song by 20 s and checks that only the missing segments are fetched and
only the new part is separated.

    python tools/ipc_e2e.py <vod url/id> --exe core/target/release/cliper.exe \
        --projects <dir> --models models [--songs 2] [--height 540]
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

    def wait(self, pred, timeout=3600, on_event=None):
        """Wait for the first notification matching `pred` (others go to `on_event`)."""
        deadline = time.time() + timeout
        while True:
            ev = self.events.get(timeout=max(1, deadline - time.time()))
            if pred(ev):
                return ev
            if on_event:
                on_event(ev)

    def wait_job(self, job_id, timeout=3600):
        t0 = time.time()
        last = [0.0]

        def show(ev):
            prm = ev.get("params", {})
            if ev["method"] == "job.progress" and prm.get("job_id") == job_id:
                pr = prm["progress"]
                if pr.get("message") or time.time() - last[0] > 2:
                    last[0] = time.time()
                    msg = pr.get("message") or "%s/%s" % (pr["done"], pr["total"])
                    print(f"    [{prm['kind']}] {pr['stage']}: {msg}", flush=True)

        ev = self.wait(lambda e: e["method"] in ("job.done", "job.failed") and e["params"].get("job_id") == job_id, timeout, show)
        if ev["method"] == "job.failed":
            raise RuntimeError(f"job failed: {ev['params']['error']}")
        print(f"    done in {time.time() - t0:.1f}s")
        return ev["params"]["result"]

    def wait_request(self, request_id, timeout=3600):
        def show(ev):
            if ev["method"] == "video.changed":
                v = ev["params"]["video"]
                for r in v["requests"]:
                    if r["id"] == request_id:
                        print(f"    [video] {r['have_sec']:.0f}/{r['total_sec']:.0f} s", flush=True)

        ev = self.wait(lambda e: e["method"] == "video.request_done" and e["params"]["request_id"] == request_id, timeout, show)
        return ev["params"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("video")
    ap.add_argument("--exe", default="core/target/release/cliper.exe")
    ap.add_argument("--projects", required=True)
    ap.add_argument("--models", default="models")
    ap.add_argument("--songs", type=int, default=2, help="keep only the N songs after the first (0 = all)")
    ap.add_argument("--height", type=int, default=540, help="video rendition height (small = gentle on the CDN)")
    args = ap.parse_args()

    c = Client([os.path.abspath(args.exe), "serve", "--projects", args.projects, "--models", args.models])
    ready = c.events.get(timeout=30)
    assert ready["method"] == "engine.ready", ready
    assert ready["params"]["protocol"] >= 3, ready["params"]
    print("engine.ready:", json.dumps(ready["params"]["system"]["models"]))

    proj = c.call("project.create", {"input": args.video}, timeout=60)
    pid = proj["id"]
    print("project:", pid, "-", proj["source"]["title"])
    assert c.call("project.get", {"id": pid})["id"] == pid
    assert any(s["id"] == pid for s in c.call("project.list"))
    print("vod.check:", c.call("vod.check", {"id": pid}, timeout=60)["status"])

    # error path: export before audio (PRECONDITION) or before video (VIDEO_MISSING)
    try:
        c.call("export.run", {"id": pid})
        raise AssertionError("expected an error")
    except RuntimeError as e:
        assert "PRECONDITION" in str(e) or "VIDEO_MISSING" in str(e), e

    if not proj.get("audio"):
        print("audio.fetch")
        c.wait_job(c.call("audio.fetch", {"id": pid})["job_id"])
    print("detect.run")
    res = c.wait_job(c.call("detect.run", {"id": pid})["job_id"])
    print("   ", res)

    proj = c.call("project.get", {"id": pid})
    settings = {**proj["settings"], "video_height": args.height}
    proj = c.call("project.update_settings", {"id": pid, "settings": settings})
    songs = proj["songs"]
    for s in songs:
        print(f"    song {s['start']:8.1f}-{s['end']:8.1f}")
    if args.songs:
        keep = {s["id"] for s in songs[1:1 + args.songs]}
        for s in songs:
            s["enabled"] = s["id"] in keep
        proj = c.call("songs.set", {"id": pid, "songs": songs})
    ids = [s["id"] for s in proj["songs"] if s["enabled"]]

    # export without video fails with the missing ranges
    try:
        c.call("export.run", {"id": pid, "song_ids": ids[:1]})
        raise AssertionError("expected VIDEO_MISSING")
    except RuntimeError as e:
        assert "VIDEO_MISSING" in str(e), e

    est = c.call("video.estimate", {"id": pid, "song_ids": ids})
    print(f"video.fetch: {est['missing_sec']:.0f} s, about {est['estimated_bytes'] / 1e6:.0f} MB")
    plan = c.call("video.fetch", {"id": pid, "song_ids": ids, "reason": "song", "process": True})
    done = c.wait_request(plan["request_id"])
    assert done["status"] == "done", done
    job = c.wait(lambda e: e["method"] == "job.started" and e["params"]["kind"] == "stems.ensure" and e["params"]["project_id"] == pid, 120)
    print("stems.ensure (started by the engine)")
    print("   ", c.wait_job(job["params"]["job_id"]))
    st = c.call("video.status", {"id": pid})
    print("    video:", [(round(r["start"], 1), round(r["end"], 1), r["state"], r.get("height")) for r in st["ranges"]])
    proj = c.call("project.get", {"id": pid})
    print("    stems:", [(round(s["start"], 1), round(s["end"], 1)) for s in proj["stems"]])
    for s in proj["songs"]:
        if s["enabled"]:
            print(f"    song {s['id']}: {len(s['mutes'])} mute suggestion(s)")
    first = next(s for s in proj["songs"] if s["enabled"])
    v = c.call("video.view", {"id": pid, "t": first["start"] + 1}, timeout=600)
    print("    view:", v["file"], "origin", round(v["origin"], 3))

    # grow the first song by 20 s: only the missing segments, only the new part separated
    c.call("song.update", {"id": pid, "song_id": first["id"], "patch": {"start": max(0.0, first["start"] - 20)}})
    plan = c.call("video.fetch", {"id": pid, "song_ids": [first["id"]], "reason": "topup", "priority": "auto"})
    print(f"top-up: {plan['missing_sec']:.1f} s of {plan['total_sec']:.1f} s")
    if plan["request_id"]:
        c.wait_request(plan["request_id"])
    r = c.wait_job(c.call("stems.ensure", {"id": pid, "song_ids": [first["id"]]})["job_id"])
    print("    separated now:", r["separated"])

    print("export.run")
    res = c.wait_job(c.call("export.run", {"id": pid})["job_id"])
    for e in res["exports"]:
        r = e["result"]
        loud = r.get("loudness") or {}
        print(f"    {r['path']} ({r['duration']:.1f}s, {r['video_codec']}, {loud.get('integrated', float('nan')):.1f} LUFS)")
    u = c.call("storage.usage", {"id": pid})
    print("    storage:", {k: u[k] for k in ("audio", "video", "stems", "unused_video", "cache", "total")})

    c.call("engine.shutdown")
    c.p.wait(timeout=10)
    print("OK")


if __name__ == "__main__":
    main()
