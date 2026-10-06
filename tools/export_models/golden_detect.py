"""Golden reference for the Rust detector: song-split-research pipeline on one file.

Decodes like `songsplit.audio.decode_mono` (ffmpeg → 32 kHz mono f32), runs the
exported ONNX with `dl_tagger.PannTagger.probs` chunking/grouping/interp, then
the reference `songsplit.segment.segment` (v6 defaults). Writes probs + segments
as JSON; optionally compares against Rust's `--probs-json` output.

    uv run python golden_detect.py <audio> --model ../../models/panns_sed.onnx \
        --out golden.json [--rust rust_probs.json]
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

import numpy as np

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "reference" / "song-split-research" / "src"))
from songsplit.segment import Params, segment  # noqa: E402

SR = 32000
FPS = 10.0


def decode_mono(path: str) -> np.ndarray:
    p = subprocess.run(["ffmpeg", "-v", "error", "-i", path, "-vn", "-ac", "1", "-ar", str(SR), "-f", "f32le", "-"],
                       capture_output=True, check=True)
    return np.frombuffer(p.stdout, dtype=np.float32)


def probs(sess, groups, sig):
    chunk = 60 * SR
    fw = []
    for i in range(0, len(sig), chunk):
        seg = sig[i:i + chunk]
        if len(seg) < SR // 2:
            if fw:
                fw.append(fw[-1][:1])
            continue
        fw.append(sess.run(["framewise"], {"waveform": seg[None, :]})[0][0])
    fw = np.concatenate(fw, axis=0)
    dur = len(sig) / SR
    n = int(dur * FPS)
    t_src = np.linspace(0, n / FPS, len(fw))
    t_dst = np.arange(n) / FPS
    grp = lambda idx: np.interp(t_dst, t_src, fw[:, idx].max(axis=1))
    return {"music": grp(groups["music"]), "speech": grp(groups["speech"]), "sing": grp(groups["sing"])}, dur


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("audio")
    ap.add_argument("--model", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--rust", help="Rust --probs-json output to compare")
    ap.add_argument("--providers", default="CPUExecutionProvider")
    args = ap.parse_args()

    import onnxruntime as ort
    groups = json.loads(Path(args.model).with_name("panns_labels.json").read_text(encoding="utf-8"))["groups"]
    sess = ort.InferenceSession(args.model, providers=args.providers.split(","))
    sig = decode_mono(args.audio)
    pr, dur = probs(sess, groups, sig)
    z = {"fps": FPS, "dur": dur, "pann_music": pr["music"], "pann_speech": pr["speech"], "pann_sing": pr["sing"]}
    segs = segment(z, Params())
    out = {"duration": dur, "segments": [[float(a), float(b)] for a, b in segs],
           "music": pr["music"].astype(np.float32).tolist(), "speech": pr["speech"].astype(np.float32).tolist(),
           "sing": pr["sing"].astype(np.float32).tolist()}
    Path(args.out).write_text(json.dumps(out), encoding="utf-8")
    print(f"python: {len(segs)} segments, dur={dur:.1f}")
    for a, b in segs:
        print(f"  {a:9.1f} ~ {b:9.1f}")

    if args.rust:
        r = json.loads(Path(args.rust).read_text(encoding="utf-8"))
        n = min(len(r["music"]), len(pr["music"]))
        print(f"len python={len(pr['music'])} rust={len(r['music'])}")
        for k in ("music", "speech", "sing"):
            d = np.abs(np.asarray(r[k][:n]) - pr[k][:n])
            print(f"  {k:>6}: mean|d|={d.mean():.2e} max|d|={d.max():.2e}")
        # segments from Rust's probs through the *Python* segmenter (isolates segmenter parity)
        zr = {"fps": FPS, "dur": r["duration"], "pann_music": np.asarray(r["music"]),
              "pann_speech": np.asarray(r["speech"]), "pann_sing": np.asarray(r["sing"])}
        print("python segment(rust probs):", [[round(a, 1), round(b, 1)] for a, b in segment(zr, Params())])


if __name__ == "__main__":
    main()
