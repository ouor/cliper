# Cliper

치지직·SOOP 노래방송 다시보기에서 노래 구간을 찾아 곡별 동영상으로 뽑는 도구.

1. VOD **오디오만** 빠르게 받는다 (fMP4는 오디오 바이트만 Range로, TS는 가장 가벼운 화질에서).
2. PANNs(ONNX, CUDA)로 노래 구간을 검출한다 (song-split-research v6 포팅).
3. 검출 구간에 **마진을 붙여 그 범위의 영상만** 원본 화질로 받는다.
4. 보컬/반주 분리(UVR MDX-Net) 후, 곡 앞뒤 멘트 구간의 보컬을 지워 곡별 mp4로 export.

핵심 엔진은 Rust 바이너리(`core/`), UI는 Electron(`app/`, 예정)이 JSON-RPC로 엔진을 구동한다.
설계: [docs/PLAN.md](docs/PLAN.md) · IPC 계약: [docs/ipc.md](docs/ipc.md)

## 구조

```
core/                    Rust workspace
  crates/cliper-core/    엔진 라이브러리 (platforms, hls, audio, ml, analysis, project, engine, ipc)
  crates/cliper/         단일 바이너리: `cliper serve` (IPC) + 디버그 CLI
app/src/ipc/             UI용 IPC 타입 (generated/ = ts-rs 생성물, protocol.ts = 메서드 맵)
tools/export_models/     개발 시 1회: PANNs → ONNX 변환, Python 골든 비교 (uv 프로젝트)
tools/ipc_e2e.py         IPC 전체 흐름 E2E 클라이언트
models/                  (gitignore) panns_sed.onnx, panns_labels.json, UVR-MDX-NET-Voc_FT.onnx
runtime/cuda/            (gitignore) CUDA 13 / cuDNN 9 DLL
reference/               (gitignore) 참고 레포 클론
```

## 준비

- Rust (stable), ffmpeg/ffprobe on PATH (또는 `CLIPER_FFMPEG`)
- NVIDIA GPU + 드라이버. ONNX Runtime CUDA EP는 CUDA 13 런타임(cudart/cublas/cufft)과 cuDNN 9가 필요:
  `runtime/cuda/`에 DLL을 두거나 `CUDA_PATH`/`CLIPER_CUDA_DIR`로 지정 (없으면 CPU로 동작).
  cuDNN은 `pip download nvidia-cudnn-cu13 --no-deps`로 받은 wheel의 DLL을 사용.
- 모델 (`models/`):
  - `UVR-MDX-NET-Voc_FT.onnx` — https://github.com/TRvlvr/model_repo/releases/download/all_public_uvr_models/UVR-MDX-NET-Voc_FT.onnx
  - `panns_sed.onnx` + `panns_labels.json` — `Cnn14_DecisionLevelMax` 체크포인트(zenodo 3987831)를 변환:
    ```bash
    cd tools/export_models
    uv run python export_panns.py --ckpt ../../models/Cnn14_DecisionLevelMax.pth --out ../../models
    ```

## 빌드 / 실행

```bash
cd core
cargo build --release
```

```bash
core/target/release/cliper serve --projects projects --models models
```

디버그 CLI (단계별): `resolve`, `fetch-audio`, `detect`, `fetch-video`, `separate`, `talk`, `export` — `cliper --help`.

## 검증

```bash
cd core && cargo test --release
```

```bash
python tools/ipc_e2e.py 15533152 --projects projects --models models --songs 2
```

TS 타입 재생성: `cd core && cargo test -p cliper-core --features ts export_bindings`.

## 참고 구현

`reference/`에 클론 (커밋하지 않음):
- https://github.com/ouor/quick-stream-audio-download
- https://github.com/ouor/song-split-research
