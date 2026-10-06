"""Export PANNs Cnn14_DecisionLevelMax (SED) to ONNX for the Rust core.

Dev-time only. Mirrors song-split-research `experiments/onnx_bench.py`:
waveform (B, N) @ 32 kHz mono → framewise (B, T, 527). The log-mel front end is
inside the graph, so the runtime only feeds PCM.

Also writes `panns_labels.json` with the music/speech/sing label-index groups,
computed exactly like `songsplit.dl_tagger` (case-insensitive substring match).

    uv run python export_panns.py --ckpt ../../models/Cnn14_DecisionLevelMax.pth --out ../../models
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

import numpy as np
import torch
import torch.nn as nn

SR = 32000
MUSIC_KEYS = ["Music", "Musical instrument", "Singing", "Song", "Choir", "Rapping", "Background music"]
SPEECH_KEYS = ["Speech", "Conversation", "Narration", "Male speech", "Female speech"]
SING_KEYS = ["Singing", "Choir", "Rapping", "Yodeling", "Chant"]


def idx(labels, keys):
    return [i for i, l in enumerate(labels) if any(k.lower() in l.lower() for k in keys)]


class Wrap(nn.Module):
    """dict-output model → framewise tensor only."""

    def __init__(self, m):
        super().__init__()
        self.m = m

    def forward(self, x):
        return self.m(x)["framewise_output"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ckpt", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)

    from panns_inference.config import labels
    from panns_inference.models import Cnn14_DecisionLevelMax

    model = Cnn14_DecisionLevelMax(sample_rate=SR, window_size=1024, hop_size=320, mel_bins=64,
                                   fmin=50, fmax=14000, classes_num=527)
    ckpt = torch.load(args.ckpt, map_location="cpu", weights_only=False)
    model.load_state_dict(ckpt["model"])
    model.eval()

    onnx_path = out / "panns_sed.onnx"
    # eval() on the wrapper too: export restores the wrapper's mode afterwards,
    # which would otherwise leave the model in train mode (dropout on).
    torch.onnx.export(
        Wrap(model).eval(), torch.randn(1, SR * 10), str(onnx_path),
        input_names=["waveform"], output_names=["framewise"],
        dynamic_axes={"waveform": {0: "b", 1: "n"}, "framewise": {0: "b", 1: "t"}},
        opset_version=17, dynamo=False,
    )
    print(f"exported -> {onnx_path} ({onnx_path.stat().st_size / 1e6:.0f} MB)")

    groups = {"music": idx(labels, MUSIC_KEYS), "speech": idx(labels, SPEECH_KEYS), "sing": idx(labels, SING_KEYS)}
    meta = {"model": "Cnn14_DecisionLevelMax", "sample_rate": SR, "input": "waveform", "output": "framewise",
            "classes": 527, "groups": groups, "labels": list(labels)}
    (out / "panns_labels.json").write_text(json.dumps(meta, ensure_ascii=False, indent=1), encoding="utf-8")
    print({k: len(v) for k, v in groups.items()})

    # parity check torch vs onnxruntime
    import onnxruntime as ort
    sess = ort.InferenceSession(str(onnx_path), providers=["CPUExecutionProvider"])
    x = (np.random.randn(SR * 10).astype(np.float32) * 0.1)[None, :]
    with torch.no_grad():
        ref = model(torch.from_numpy(x))["framewise_output"].numpy()
    got = sess.run(["framewise"], {"waveform": x})[0]
    print(f"parity max|diff|={np.abs(ref - got).max():.2e} shape={got.shape}")


if __name__ == "__main__":
    main()
