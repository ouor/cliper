# Cliper

치지직·SOOP 노래방송 다시보기에서 노래 구간을 찾아 곡별 동영상으로 뽑는 도구.

1. VOD 오디오만 빠르게 받는다 (HLS 세그먼트에서 오디오 바이트만 Range로).
2. PANNs(ONNX CUDA)로 노래 구간을 검출한다.
3. 검출 구간에 마진을 붙여 그 범위의 영상만 받는다.
4. 보컬/반주 분리 후, 곡 앞뒤 멘트 구간의 보컬을 지워 곡별 mp4로 export.

핵심 엔진은 Rust 바이너리(`core/`), UI는 Electron(`app/`, 예정) — 설계는 [docs/PLAN.md](docs/PLAN.md).

참고 구현(`reference/`에 클론, 커밋하지 않음):
- https://github.com/ouor/quick-stream-audio-download
- https://github.com/ouor/song-split-research
