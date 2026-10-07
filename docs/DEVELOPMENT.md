# Cliper 개발 문서

Cliper를 직접 빌드하거나 고치려는 분을 위한 문서예요. 사용법은 [README](../README.md)를 봐 주세요.

## 구조

핵심 처리는 전부 Rust 엔진(`core/`)이 하고, 데스크톱 UI(`app/`, Electron)는 엔진을 자식 프로세스로 띄워 JSON-RPC로 조종해요.

```
core/                    Rust workspace
  crates/cliper-core/    엔진 라이브러리
    platforms/           치지직·SOOP VOD 정보와 재생 주소
    hls/                 m3u8, fMP4/TS 파서, 오디오만 받기, 구간 영상 받기
    ml/                  ONNX Runtime(CUDA → CPU), PANNs 태거, MDX-Net 분리기
    analysis/            노래 구간 검출(song-split-research v6 포팅), 멘트 찾기
    audio/               ffmpeg 디코드, STFT, 정렬, 파형, WAV
    project/             project.json, 클립 계획, 파일 이름, 세트리스트, 곡 목록 내보내기
    engine.rs            작업(job), 프로젝트 변경, IPC 메서드
    ipc/server.rs        `cliper serve`: stdin/stdout NDJSON JSON-RPC 2.0
    addon.rs             CUDA 런타임 애드온 받기·설치
  crates/cliper/         단일 바이너리: `cliper serve` + 헤드리스 `process` + 단계별 디버그 CLI
app/                     Electron + electron-vite + React + TypeScript
  src/ipc/               IPC 타입(generated/ = ts-rs 생성물, protocol.ts = 메서드 맵)
tools/export_models/     PANNs → ONNX 변환, Python 골든 비교 (uv 프로젝트, 개발 시 1회)
tools/ipc_e2e.py         IPC 전체 흐름 E2E 클라이언트
docs/                    IPC 계약, 기획, 리뷰, 인터뷰
models/                  (git 밖) panns_sed.onnx, panns_labels.json, UVR-MDX-NET-Voc_FT.onnx
runtime/cuda/            (git 밖) CUDA 13 / cuDNN 9 DLL
```

처리 흐름:

1. **오디오만 받기**: fMP4 VOD는 오디오 바이트만 Range로, TS VOD는 가장 가벼운 화질에서 오디오만 뽑아요. 4시간 VOD가 1분 안팎이에요.
2. **노래 구간 찾기**: PANNs(ONNX)로 music/speech/sing 확률을 만들고 v6 분할기로 곡을 나눠요. 4시간에 GPU로 15초쯤 걸려요.
3. **구간 영상 받기**: 곡 ± 여유(기본 30초) 범위의 세그먼트만 원본 화질로 받아요. 오디오와는 세그먼트 색인으로 예측하고 상호상관으로 확인해서 맞춰요.
4. **보컬 분리 → 멘트 제안**: UVR MDX-Net Voc_FT로 보컬을 분리하고, 보컬 스템에서 말소리 구간을 찾아 뮤트를 제안해요.
5. **내보내기**: `원본 − (1 − g)·보컬`로 렌더해서 곡별 mp4/wav/flac/mp3로 내보내요. 뮤트가 없는 곳은 원본과 비트 단위로 같아요.

IPC 계약은 [docs/ipc.md](ipc.md)에 있어요.

## 준비

- Windows 10/11 64비트
- Rust stable(1.85 이상), Node.js 20 이상
- ffmpeg/ffprobe가 PATH에 있거나 `CLIPER_FFMPEG`로 지정
- (GPU) NVIDIA 드라이버 R580 이상. CUDA 13 런타임(cudart/cublas/cufft)과 cuDNN 9 DLL을 `runtime/cuda/`에 두거나 `CLIPER_CUDA_DIR`로 지정해요. 없으면 CPU로 동작해요. cuDNN은 `pip download nvidia-cudnn-cu13 --no-deps`로 받은 wheel 안의 DLL을 써요.
- 모델(`models/`):
  - `UVR-MDX-NET-Voc_FT.onnx`: https://github.com/TRvlvr/model_repo/releases/download/all_public_uvr_models/UVR-MDX-NET-Voc_FT.onnx
  - `panns_sed.onnx` + `panns_labels.json`: `Cnn14_DecisionLevelMax` 체크포인트(zenodo 3987831)를 변환해요.
    ```bash
    cd tools/export_models
    uv run python export_panns.py --ckpt ../../models/Cnn14_DecisionLevelMax.pth --out ../../models
    ```

## 빌드와 실행

엔진:

```bash
cd core
cargo build --release
```

```bash
core/target/release/cliper serve --projects projects --models models
```

앱(개발 모드):

```bash
cd app
npm install
npm run dev
```

앱 개발에 필요한 환경 변수와 점검 도구는 [app/README.md](../app/README.md)에 있어요.

디버그 CLI(단계별): `resolve`, `fetch-audio`, `detect`, `fetch-video`, `separate`, `talk`, `export`. `cliper --help`로 확인해요.

헤드리스 일괄 처리: `cliper process <링크·번호·로컬 파일…> [--clips] [--export --format mp4|wav|flac|mp3]`.

## 테스트

```bash
cd core
cargo test --release
cargo clippy --release --all-targets
```

- 엔진 테스트(`engine_tests.rs`)는 임시 폴더에 프로젝트를 만들어 편집·동시성·작업 관리·IPC·대기열·정렬·오디오 구간을 확인해요. ffmpeg나 모델 파일이 없으면 해당 테스트는 건너뛰어요.
- IPC 전체 흐름(네트워크 사용): `python tools/ipc_e2e.py 15533152 --projects <폴더> --models models --songs 2`
- 앱: `cd app && npm run typecheck && npm run build`

TS 타입 다시 만들기: `cd core && cargo test -p cliper-core --features ts export_bindings`

## 배포 파일 만들기

```bash
cd core && cargo build --release
cd ../app && npm run dist:win
```

- `app/dist/cliper-v0.1.0-amd64.exe`(포터블)과 `app/dist/cliper-v0.1.0-amd64-installer.exe`(설치 파일)이 나와요. 엔진, ffmpeg, 모델이 들어 있고 CUDA는 빠져 있어요.
- 처음 빌드할 때 electron-builder가 NSIS·7zip 도구를 받아요.
- 자세한 패키징 구성은 [app/README.md](../app/README.md)의 "패키징"을 봐 주세요.

### CUDA 애드온

CUDA 13 + cuDNN 9 런타임(DLL 14개, 압축 약 1 GB)은 앱에 넣지 않고 v0.1.0 릴리즈에 `cliper-cuda-addon-v1-amd64.zip`으로 따로 올려요. NVIDIA 그래픽카드가 있는데 CUDA 런타임이 없으면 앱이 엔진의 `addon.install_cuda`로 자동으로 받아요. 받은 파일은 SHA-256을 확인한 뒤 `%LOCALAPPDATA%\Cliper\addons\cuda-v1`에 풀어요. 이후 버전도 같은 애드온을 써요.

- URL, 크기, SHA-256은 `core/crates/cliper-core/src/addon.rs`에 고정돼 있어요. zip을 다시 만들면 이 값도 같이 바꾸고, 새 이름(`cuda-v2`)으로 올려야 해요.
- 저장소가 비공개인 동안은 릴리즈 파일을 로그인 없이 받을 수 없어 자동 다운로드가 실패해요. 브라우저로 받은 zip을 앱의 **zip 파일로 설치**(엔진 `addon.install_cuda {zip}`)로 넣으면 같은 해시 검사를 거쳐 설치돼요.
- QA: `CLIPER_CUDA_ADDON_URL`로 로컬 서버의 같은 파일을 가리키고(해시는 그대로 검사), `CLIPER_ADDON_DIR`로 설치 위치를 바꿀 수 있어요.

## 참고 구현

`reference/`에 클론해 두고 포팅했어요(커밋하지 않음).

- https://github.com/ouor/quick-stream-audio-download: 치지직·SOOP 오디오만 받기
- https://github.com/ouor/song-split-research: PANNs 기반 노래 구간 검출

## 문서

- [docs/ipc.md](ipc.md): 엔진 IPC 계약
- [docs/PLAN.md](PLAN.md): 설계와 구현하며 알게 된 것
- [docs/product/desktop-app-plan.md](product/desktop-app-plan.md): 데스크톱 앱 기획
- [docs/reviews/](reviews/): Rust·ML·Electron 코드 리뷰와 처리 결과
