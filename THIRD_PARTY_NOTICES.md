# Third-party notices

Cliper itself is released under the [MIT License](LICENSE). The release builds also ship the components below, each under its own license. Those terms apply to the component, not to Cliper.

## Bundled in the app (`cliper-v*-amd64.exe`)

| Component | Where it is in the app | License | Source |
|---|---|---|---|
| FFmpeg 8 (gyan.dev "full" build) | `resources/ffmpeg/ffmpeg.exe`, `ffprobe.exe` | GPL v3 (built with `--enable-gpl --enable-version3`) | https://ffmpeg.org, build and source archives: https://www.gyan.dev/ffmpeg/builds/ |
| ONNX Runtime | `resources/engine/onnxruntime_*.dll`, linked into `cliper.exe` | MIT | https://github.com/microsoft/onnxruntime |
| DirectML | `resources/engine/DirectML.dll` | Microsoft DirectML license (redistributable) | https://www.nuget.org/packages/Microsoft.AI.DirectML |
| PANNs `Cnn14_DecisionLevelMax` (converted to `panns_sed.onnx`, with `panns_labels.json`) | `resources/models/` | Code: MIT. Weights: see the Zenodo record | https://github.com/qiuqiangkong/audioset_tagging_cnn, weights: https://zenodo.org/records/3987831 |
| UVR MDX-Net `UVR-MDX-NET-Voc_FT.onnx` | `resources/models/` | Distributed by the Ultimate Vocal Remover project; see its terms | https://github.com/TRvlvr/model_repo, https://github.com/Anjok07/ultimatevocalremovergui |
| Electron / Chromium | the app itself | MIT (Electron), plus the Chromium licenses | `LICENSE.electron.txt` and `LICENSES.chromium.html` next to `Cliper.exe` in the unpacked app |
| Pretendard | bundled font | SIL Open Font License 1.1 | https://github.com/orioncactus/pretendard |

FFmpeg runs as a separate program; Cliper calls it on the command line and does not link to it. Its source code is available from the links above.

The Rust crates and npm packages that are compiled into the app keep their own licenses, listed in `core/Cargo.lock` and `app/package-lock.json`.

## GPU add-on (`cliper-cuda-addon-v1-amd64.zip`, downloaded separately)

| Component | License | Source |
|---|---|---|
| NVIDIA CUDA runtime 13 (cudart, cuBLAS, cuFFT) | NVIDIA CUDA Toolkit EULA (redistributable files) | https://docs.nvidia.com/cuda/eula/ |
| NVIDIA cuDNN 9 | NVIDIA cuDNN Software License Agreement | https://docs.nvidia.com/deeplearning/cudnn/latest/reference/eula.html |
