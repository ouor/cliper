# Cliper 데스크톱 앱 (Electron)

치지직·SOOP 노래방송 다시보기(또는 OBS 녹화본)에서 곡을 찾고, 멘트를 지우고, 곡별 파일로 내보내는 Windows 앱의 UI입니다.
미디어 처리는 모두 Rust 엔진(`core/`, `cliper.exe serve`)이 하고, 이 앱은 엔진을 자식 프로세스로 띄워 JSON-RPC(stdin/stdout NDJSON)로 구동합니다.

- 스택: **Electron 44 + electron-vite 5 (Vite 7) + React 19 + TypeScript 5.9 + zustand**
- 아이콘 lucide-react, 글꼴 Pretendard(번들 포함, CJK 폴백: 맑은 고딕 / Yu Gothic UI / Meiryo)
- 기획: `docs/product/desktop-app-plan.md` · IPC: `docs/ipc.md` · 구현 메모: `docs/product/ui-implementation-notes.md` · 엔진 요청: `docs/product/engine-requests.md`

## 실행

사전 준비(이미 되어 있으면 생략):

```powershell
cd core; cargo build --release          # core/target/release/cliper.exe
# models/ (UVR-MDX-NET-Voc_FT.onnx, panns_sed.onnx, panns_labels.json), ffmpeg(PATH), CUDA 런타임
```

```powershell
cd app
npm install
npm run dev        # 개발 모드 (Vite HMR). 첫 실행이면 첫 실행 마법사가 뜹니다
npm run build      # out/ 에 main · preload · renderer 빌드
npm start          # 빌드 결과 실행 (electron-vite preview)
npm run typecheck  # main/preload + renderer 타입 검사
```

### 환경 변수 (개발·검증용)

| 변수 | 의미 |
|---|---|
| `CLIPER_PROJECTS` | 작업 폴더(엔진 `--projects`)를 강제로 지정 |
| `CLIPER_SKIP_FIRST_RUN=1` | `CLIPER_PROJECTS`와 함께 쓰면 첫 실행 마법사를 건너뜀 |
| `CLIPER_USER_DATA` | 앱 설정 폴더(userData)를 바꿈 — 깨끗한 프로필로 시험할 때 |
| `CLIPER_ENGINE` / `CLIPER_MODELS` | 엔진 exe / 모델 폴더 (기본: 저장소의 `core/target/release/cliper.exe`, `models/`). 앱 자체는 ffmpeg를 직접 부르지 않습니다 |

예: 이미 처리된 프로젝트 폴더로 바로 열기

```powershell
$env:CLIPER_PROJECTS="C:\path\to\projects"; $env:CLIPER_SKIP_FIRST_RUN="1"; npm start
```

### 자동 점검 도구

- `node scripts/devdriver.mjs` — 빌드된 앱을 Playwright(Electron)로 띄우고 `127.0.0.1:7788`에 조작 API(eval / shot / key / type / click / mouse / resize / close / quit)를 엽니다. 요청마다 시작 때 만든 토큰(`<tmp>/cliper-devdriver.token`)을 `x-devdriver-token` 헤더로 보내야 하고, `Origin` 헤더가 있거나 JSON이 아닌 요청은 거부합니다.
- `node scripts/check.mjs scripts/checks/dsp-check.js` — 실행 중인 앱에서 미리듣기 AudioWorklet을 OfflineAudioContext로 렌더링해 네 가지 듣기 모드가 `docs/ipc.md`의 수식과 일치하는지 확인합니다(최대 오차 < 1e-6).
- 렌더러에는 점검용 핸들 `window.__cliper`(player, 상태 저장소)가 있습니다. 렌더러 자신의 상태만 노출하며 권한은 없습니다.

## 구조

```
app/
├─ electron.vite.config.ts      main / preload / renderer 빌드, 빌드용 엄격한 CSP 주입
├─ src/
│  ├─ ipc/                      엔진 IPC 계약 (protocol.ts, generated/* — Rust에서 생성, 직접 수정 금지)
│  ├─ shared/                   main·preload·renderer 공용 타입 (bridge.ts, settings.ts)
│  ├─ main/
│  │  ├─ index.ts               창, IPC 핸들러, 설정/상태 저장(userData), 닫기 확인, 알림
│  │  ├─ engine.ts              엔진 자식 프로세스: NDJSON JSON-RPC, 요청-응답 매칭, 알림 전달,
│  │  │                         비정상 종료 시 재시작(2분에 3회까지), stderr 로그(userData/logs/engine.log)
│  │  └─ media.ts               cliper-media:// 프로토콜 (허용 목록 파일만, Range 지원)
│  ├─ preload/index.ts          contextBridge로 window.cliper 노출 (sandbox, contextIsolation)
│  └─ renderer/
│     ├─ App.tsx · main.tsx     부팅, 엔진 상태 배너, 전역 단축키, 오버레이
│     ├─ audio/
│     │  ├─ player.worklet.ts   미리듣기 AudioWorklet (원본/결과/지운 목소리/반주, 페이드·강도·겹침 수식)
│     │  └─ player.ts           AudioContext, 소스(방송 전체 스트림 / 곡 창 stems), J/K/L, 구간 반복, 영상 동기화
│     ├─ canvas/draw.ts         캔버스 공통 (DPR, 테마 색, 눈금, mipmap 파형, 곡선, 빗금)
│     ├─ lib/                   rpc(타입 있는 call), errors(오류→한국어), song(라벨·상태·링크·설명란),
│     │                         edits(곡/멘트 편집 계산), pipeline(작업 연결), keys(단축키), peaks, format
│     ├─ state/                 zustand: app(설정·화면·토스트·대화상자), projects(낙관적 편집·되돌리기),
│     │                         jobs(작업·ETA·대기열·이어하기), ws(작업공간 선택/보기)
│     ├─ screens/               FirstRun, Home, Workspace, Settings, HelpOverlay, CookiesDialog
│     │  └─ workspace/          SongList, SongDetail(+EdgeScopes), OverviewMap, DetectPopover, SetlistDialog,
│     │                         TalkEditor(+DetailTimeline), ExportView, Transport
│     ├─ components/            공통 UI(Modal, Toasts, DialogHost, Progress), StatusBar, JobsPanel
│     └─ styles/                tokens.css(디자인 토큰, 어둡게/밝게), base.css(컨트롤), app.css(화면)
└─ scripts/                     devdriver.mjs, check.mjs, checks/dsp-check.js
```

### 보안 모델

- 렌더러: `contextIsolation: true`, `nodeIntegration: false`, `sandbox: true`. 창 열기·외부 이동 차단(https 링크만 기본 브라우저로), 권한 요청은 알림·오디오 출력 선택만 허용.
- 렌더러는 `file://`이 아니라 `app://cliper/`(main의 `protocol.handle`)에서 열리고, CSP는 응답 헤더로도 붙습니다(`src/main/app-protocol.ts`). 개발 모드에서만 Vite 서버와 HMR용 완화.
- 창은 어떤 탐색도 하지 않습니다(`will-navigate`·`will-frame-navigate`·`will-redirect` 차단). 홈의 드롭 영역 밖에 파일을 놓아도 무시됩니다.
- 앱은 사용자당 하나만 실행됩니다(single-instance lock). 두 번째 실행은 기존 창을 앞으로 가져옵니다.
- IPC는 우리 창·우리 페이지(`app://cliper` 또는 개발 서버)에서 온 것만 처리합니다. 엔진 메서드는 앱이 쓰는 것만 허용하고, 임의 위치에 쓰는 인자(`export_metadata.path`, `output_dir`)는 사용자가 대화상자에서 고른 경로만 받습니다. `settings:set`은 키와 형식을 검사합니다.
- `openPath`/`showItem`은 작업 폴더·로그 폴더·엔진이 알려 준 출력 폴더·사용자가 저장한 파일 아래만, 실행 파일 확장자는 거부합니다.
- 브리지: 엔진 호출(`call`)과 알림 구독, 설정/작은 상태 저장, 폴더·파일 대화상자, 경로 열기/폴더 표시, 디스크 여유 공간. 임의 파일 읽기는 없고, 쿠키 파일은 **사용자가 대화상자에서 고른 파일만** 1MB 이하로 읽습니다.
- `cliper-media://project/<프로젝트 id>/<경로>`: 작업 폴더 안의 **허용 목록 파일만**(`overview.peaks`, `probs.bin`, `clips/<id>/{source.mp4, orig.wav, vocals.wav, orig.peaks, vocals.peaks}`) 제공. id는 경로 구분자·예약 문자·`..` 불가, 해석 후 작업 폴더 안인지 다시 확인. Range 요청 지원(영상 탐색).
- 곡 검수 미리듣기(영상 받기 전)는 엔진 `audio.window`가 만든 `cache/window_*.wav`(전체 디코드와 샘플 단위로 같음)를 같은 프로토콜로 6초씩 읽습니다. 허용 목록에 `cache/window_<숫자>.wav`만 추가했습니다.

### 엔진 연동 요점

- 엔진은 앱 시작 시(작업 폴더가 정해져 있으면) `serve --projects <작업 폴더> --models <models>`로 실행. 작업 폴더나 처리 장치(CPU)를 바꾸면 다시 시작합니다. 처리 우선순위 "낮게"는 OS 프로세스 우선순위로 적용.
- `project.changed`를 진실의 원천으로 씁니다. 곡 편집은 **대기 중 편집(pending op)**으로 화면에 즉시 반영하고(엔진 최신 상태 위에 다시 적용), 순서대로 엔진에 보냅니다. 한 곡만 바꾸는 편집은 전후를 비교해 바뀐 것만 `song.update` / `song.add` / `song.remove` / `mute.upsert` / `mute.remove`로 보내고, 여러 곡에 걸친 편집(나누기·합치기·여러 곡 켜고 끄기)은 `songs.set` 한 번으로 원자적으로 보냅니다. 모두 `base_revision`을 붙입니다. `CONFLICT`면 최신 상태를 받아 같은 편집을 다시 적용해 재시도합니다. 엔진이 새로 붙인 id(`song.add`, 새 `mute.upsert`)는 별칭으로 기록해 선택 상태와 되돌리기 기록이 따라가게 합니다(`state/projects.ts`).
- 되돌리기는 그 편집이 바꾼 필드와 그 편집이 추가·삭제·변경한 멘트 구간만 이전 상태로 되돌립니다. 그사이 작업이 넣은 멘트 제안 같은 다른 변경은 지우지 않습니다.
- 같은 종류 작업이 돌고 있어 `BUSY`가 오면 앱 대기열에 넣고, 앞 작업이 끝나면 자동으로 시작합니다.
- 소리 받기 → 곡 찾기, 영상 받기 → 내보내기(밤새 처리) 같은 연결은 앱이 작업 완료 알림으로 이어 줍니다. 곡을 정해서 받을 때는 `clips.fetch {song_ids, exact: true}`로 이웃 곡을 함께 받지 않습니다.
- 진행 표시는 `job.started.items`, `progress.item_index/items_total/eta_sec`, `progress.note.code`(한국어 문장은 앱이 만듦)를 씁니다. 소리 받는 동안은 `progress.peaks_until`에 맞춰 `overview.peaks`를 다시 읽어 파형을 채웁니다.
- 진행 중 작업 요청은 userData에 기록해 두었다가, 앱이 작업 중에 닫히면 다음 실행 때 "지난번에 멈춘 작업 [이어서 하기]"로 다시 요청합니다.

## 패키징

설정: `electron-builder.config.cjs`. 결과물은 `dist/`(git에 들어가지 않음).

```sh
npm run pack:win         # dist/win-unpacked (설치 파일 없이 빠른 확인)
npm run dist:win         # dist/cliper-v0.1.0-amd64.exe (포터블) + dist/cliper-v0.1.0-amd64-installer.exe (NSIS)
npm run pack:cuda-addon  # ../var/release의 정본 애드온 zip을 엔진에 고정된 크기·sha256으로 확인하고 dist/로 복사
```

릴리스 빌드는 **CPU 전용**입니다(각 약 605MB). CUDA 런타임은 v0.1.0 GitHub 릴리스에 `cliper-cuda-addon-v1-amd64.zip`(1,012,083,860바이트)으로 따로 올립니다. NVIDIA 그래픽카드가 있는데 CUDA 런타임이 없는 PC에서는 앱이 자동으로 받아 설치합니다. 엔진 job은 `addon.install_cuda`이고, 받은 뒤 SHA-256을 확인해 `%LOCALAPPDATA%\Cliper\addons\cuda-v1`에 풀며, 앱이 엔진을 다시 시작합니다. 엔진이 이 zip의 크기와 SHA-256을 고정해 두었기 때문에 **zip을 새로 만들면 안 됩니다**(다시 만든 zip은 해시가 달라 거부돼요). `pack:cuda-addon`은 정본 파일을 확인해 복사만 합니다. QA: `CLIPER_CUDA_ADDON_URL`(받을 주소, 해시는 그대로 고정)과 `CLIPER_ADDON_DIR`(설치 위치)로 로컬 서버에서 시험할 수 있습니다. 저장소가 비공개라 자동 받기가 실패하면(NETWORK, 404/401 포함) 카드가 릴리스 페이지 열기와 **zip 파일로 설치**를 보여 줍니다. zip 설치는 `addon.install_cuda {zip}`이고, main은 방금 파일 대화상자에서 고른 `.zip` 경로만 엔진에 넘깁니다(단계 `addon.verify` → `addon.extract`). 창·작업 표시줄 아이콘은 `resources/icon.png`(asar 안), 설치 파일 아이콘은 `build/icon.ico`입니다.

패키저가 기대하는 것(모두 git 밖에 있고, 빌드할 때 `resources/`로 복사됩니다):

| resources/ | 내용 | 원본 (환경 변수로 바꿀 수 있음) |
|---|---|---|
| `engine/` | `cliper.exe` + 옆의 DLL(onnxruntime 공급자, DirectML) | `CLIPER_PACK_ENGINE_DIR`, 기본 `../core/target/release` (`cargo build --release`) |
| `engine/cuda/` | `CLIPER_PACK_CUDA=1`일 때만(내부 확인용). CUDA 13 + cuDNN 9 DLL. 엔진이 `<exe 폴더>/cuda`를 찾고, 앱도 `CLIPER_CUDA_DIR`로 넘깁니다 | `CLIPER_PACK_CUDA_DIR`, 기본 `../runtime/cuda` |
| `ffmpeg/` | `ffmpeg.exe`, `ffprobe.exe`(공유 빌드면 DLL도). 앱이 `CLIPER_FFMPEG`로 넘기고, 엔진은 그 옆에서 ffprobe를 찾습니다 | `CLIPER_PACK_FFMPEG_DIR`, 기본은 PATH에 있는 ffmpeg의 폴더 |
| `models/` | `*.onnx`, `panns_labels.json` (`.pth`는 넣지 않음) | `CLIPER_PACK_MODELS_DIR`, 기본 `../models` |

빠진 파일이 있으면 빌드가 처음에 어떤 파일이 없는지 알려 주고 멈춥니다.

- 패키지 빌드는 `resources/` 아래 경로만 씁니다. `CLIPER_*` 환경 변수와 `ELECTRON_RENDERER_URL`은 개발 빌드에서만 적용됩니다. 저장소 경로로 되돌아가지 않습니다.
- Electron은 npm이 설치한 `node_modules/electron/dist`를 그대로 씁니다(`electronDist`). 렌더러와 main은 electron-vite가 묶으므로 asar에는 `out/`과 `package.json`만 들어갑니다. 그래서 런타임 패키지도 devDependencies에 있습니다.
- Electron fuses: RunAsNode·NODE_OPTIONS·`--inspect` 인자 끔, OnlyLoadAppFromAsar·쿠키 암호화 켬, `GrantFileProtocolExtraPrivileges` 끔(렌더러는 `app://cliper`에서 옵니다). `afterPack`에서 `default_app.asar`를 지웁니다.
- 아직 꺼 둔 것: 실행 파일 편집과 서명(`signAndEditExecutable: false`, 아이콘·버전 정보·코드 서명에는 GitHub에서 받는 winCodeSign 묶음이 필요), 그리고 그에 기대는 `EnableEmbeddedAsarIntegrityValidation`. 릴리스 파이프라인에서 서명 도구와 함께 켜세요.
- NSIS·포터블 대상을 처음 빌드할 때 electron-builder가 GitHub(electron-userland/electron-builder-binaries)에서 NSIS·7zip 도구를 받아 `%LOCALAPPDATA%\electron-builder\Cache`에 둡니다. `pack:win`은 받지 않습니다.
- 포터블 exe는 실행할 때마다 임시 폴더에 약 1.3GB를 풀기 때문에 시작이 몇 초 걸립니다.
- QA: `CLIPER_PACK_INSPECT=1`로 만든 빌드만 `--inspect`를 허용하므로, `DEVDRIVER_EXE=dist/win-unpacked/Cliper.exe DEVDRIVER_ARGS=--user-data-dir=<임시 폴더> node scripts/devdriver.mjs`로 조종할 수 있습니다. 배포 빌드는 이 퓨즈가 꺼져 있습니다.
- 확인한 것(패키지 빌드, PATH에서 ffmpeg를 뺀 상태): `app://cliper`에서 로드, 엔진·모델·ffmpeg를 `resources/`에서 찾음, asar 안의 AudioWorklet 로드와 재생.
