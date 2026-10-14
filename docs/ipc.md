# Cliper engine IPC

The UI never does media work itself. It spawns the Rust engine and talks to it
over **newline-delimited JSON-RPC 2.0 on the child's stdin/stdout**.

```
cliper.exe serve --projects <dir> --models <dir> [--cpu] [--config <file>]
```

`--config` is the app preferences file (keep-awake, video pause); default
`%LOCALAPPDATA%\Cliper\engine.json`. It belongs to the user, not to a
projects folder (A-6).

- One JSON object per line, UTF-8, both directions.
- **stdout carries protocol lines only**; logs go to stderr (`RUST_LOG=info` for more).
- Requests run concurrently; match responses by `id`.
- Types: `app/src/ipc/generated/*.ts` (generated from Rust with ts-rs) and the
  method/notification map in `app/src/ipc/protocol.ts`.
  Regenerate after changing Rust types:
  `cd core && cargo test -p cliper-core --features ts export_bindings`.

Protocol **4** (this document): video is kept per VOD range (segment store),
not per song clip; the protocol-2 leftovers are gone. See
[What changed in protocol 4](#what-changed-in-protocol-4-for-the-ui) and
[protocol 3](#what-changed-in-protocol-3-for-the-ui) at the end.

## Lifecycle

1. Spawn `cliper serve`. The first line is the `engine.ready` notification
   (`{ protocol, system }`); check `system.models` and `system.ffmpeg` and
   guide the user if something is missing. `system.warnings` lists settings
   files that could not be read (set aside as `*.corrupt-<time>`, the last
   good copy or defaults used) — show them once.
   **One engine per projects folder**: the engine holds
   `<projects>/.cliper.lock` while it runs. A second engine on the same folder
   (another app window, `cliper process`, a portable copy) sends
   `engine.failed {protocol, code: "ROOT_LOCKED", message, data: {root}}`
   instead of `engine.ready` and exits — tell the user another Cliper is
   using that folder.
2. Send requests; listen for notifications. Video downloads left from the
   previous run resume by themselves (unless paused, see `video.pause`).
3. On quit, send `engine.shutdown` (cancels jobs) or close stdin.

## Workflow

```
project.create {input}            → Project (VOD URL / chzzk number / soop:<id> / local file path;
                                              reopening returns the existing project)
vod.check {id}                    → VodStatus  (call in the background when a project opens)
audio.fetch {id}                  → job   (audio only, ~3% of the video bytes; 4 h VOD ≈ 1 min;
                                           local file: copies the audio track)
detect.run {id}                   → job   (PANNs on GPU ≈ 15 s for 4 h; creates songs (with
                                           confidence) + candidates)
  songs.set / song.update / songs.match_setlist / detect.rerun / project.update_settings (instant)
video.fetch {id, song_ids | ranges, priority?, process?}   → FetchPlan (queued; only missing
                                           segments; progress via video.changed /
                                           video.request_done)
stems.ensure {id, song_ids? | ranges?} → job (separate what has video and isn't separated,
                                           talk → mute suggestions (unconfirmed), new parts only)
  (video.fetch {process: true} starts stems.ensure by itself when its video is in)
video.view {id, t}                → VideoView (a playable MP4 + its VOD origin, for <video>)
export.run {id, song_ids?, format?, fetch?, stems?, …} → job (mp4 / wav / flac / mp3 per song,
                                           or editor stems + markers; fetch: true = fetch
                                           missing video first)
project.export_metadata {id, format}           (CSV / JSON / chapters / timestamps)
storage.usage / media.drop / project.set_pinned  (disk space, cleanup levels, keep)

queue.add {inputs, through}       batch: audio → detect (→ video + stems at "batch" priority)
                                  for many VODs, sequentially
```

### Review state (what the UI shows as "확인 전 / 확인됨")

- **Songs**: `reviewed` (boundaries checked by the user), `enabled` (false =
  excluded but kept, so detection reruns don't bring it back), `note`, `artist`,
  and `confidence` from the detector (`doubts`: `short`, `long`, `weak_singing`,
  `talk_inside`, `resplit` — show "애매함" when non-empty; `null` for manual songs).
  `detect.rerun` / `detect.run` replace only untouched auto songs: not reviewed,
  no title/artist/note/mutes, enabled, `mute_enabled`, no own fades, not
  exported. (Video on disk no longer protects a song: the video stays on the
  VOD timeline whatever the songs do.)
- **Per-song switches**: `mute_enabled: false` exports the original audio
  (mutes are kept for later); `fade_in` / `fade_out` override
  `settings.song_fade_in/out` for that song (`null` = project default).
- **Candidates**: `detection.candidates` are music-like stretches the detector
  rejected (`too_short`, `no_singing`, `talk`) — draw them as dashed "maybe a
  missed song" bars; adding one = `song.add` with a new manual song.
- **Mutes**: suggestions arrive with `origin: "auto", confirmed: false`.
  **Export applies only confirmed mutes** (unless `include_unconfirmed`).
  Confirm = `mute.upsert` with `confirmed: true`. Mutes created by the user
  (`origin: "manual"`) are confirmed automatically. Suggestions are made **once
  per VOD range**: `project.talk_scanned` lists the ranges already searched;
  separation of new ranges adds suggestions only there, and nothing ever
  removes or re-adds a suggestion in a scanned range — a deleted suggestion
  never comes back (also after cleanup + re-separation). `project.talk` holds
  every talk region found (VOD time).
- **Mute strength**: `gain_db: null` removes the voice; a negative number only
  turns it down (e.g. `-12`). Default for new suggestions: `settings.mute_gain_db`.

Every mutation — from a request or a job — is followed by `project.changed`
with the full new `Project`. The UI can treat that as the single source of truth.
Video coverage is **not** in the project (it changes several times per second
while downloading): it comes with `video.changed` / `video.status`.

### Editing without races

Prefer the **per-item methods** (`song.update` / `song.add` / `song.remove`,
`mute.upsert` / `mute.remove`): they change only what they name, so a job that
adds mute suggestions at the same moment is never overwritten. `songs.set`
(whole-list replacement) is still there for bulk edits and undo.

The editing methods (`song.update/add/remove`, `mute.upsert/remove`,
`songs.set`) accept `base_revision`: the `revision` of the project the client
edited. If the project changed since (`project.revision` is bumped
on every save), the call fails with `CONFLICT` and nothing is written — reload
(or take the latest `project.changed`) and reapply.

## Time model

- **All times are seconds on the VOD timeline** (song `start/end`, mute
  `start/end`, stem spans, video ranges, `talk`).
- VOD time *t* is sample *t·rate* of the **fully decoded analysis audio**
  (`audio.aac` / `audio.mka` decoded from its start). The detector, the
  overview peaks, `audio.window` (sample-exact windows, verified identical to a
  full decode), video coverage and every stem span use this timeline.
- **HLS segments** of the VOD are placed by the analysis audio's frame index:
  segment *i* spans `[bound(i), bound(i+1))`. Requests are rounded **outward**
  to whole segments (+0.1 s margin); `video.estimate` / `video.fetch` return the
  rounded ranges.
- Each rendition's own clock (fMP4 `tfdt` / TS PTS, "media time") maps to VOD
  time as `vod = media − m`, `m` measured once per rendition by
  cross-correlating its audio with the analysis audio (≈ tens of ms; falls back
  to the frame-index prediction, flagged, on silence). Every file the engine
  derives (views, stems, export sources) is placed from media timestamps, so
  they agree with each other to the sample whatever was fetched when.
- **Views** (`video.view`): `video.currentTime = vod − view.origin`, valid for
  `[view.start, view.end]`.
- **Stem spans** (`project.stems[]`): frame *k* of `orig.wav` / `vocals.wav`
  is VOD time `span.start + k / 48000` (`span.start` is on the 48 kHz grid).
- Local recordings: one "segment" = the whole file; `vod = file time − m`.

## Video: what is on disk, what is coming

`video.status {id}` (and the throttled `video.changed {video}` notification,
≤ 4/s per project) return a `VideoState`:

```jsonc
{
  "project_id": "soop-208278235",
  "ranges": [                       // sorted, non-overlapping; gaps = no video
    { "start": 2132.89, "end": 2399.55, "state": "have", "height": 540 },
    { "start": 2599.55, "end": 2616.21, "state": "fetching", "height": 540 },
    { "start": 2616.21, "end": 2707.88, "state": "queued" },
    { "start": 3999.55, "end": 4016.21, "state": "failed", "code": "NETWORK" }
  ],
  "requests": [ { "id": "r…", "priority": "user", "reason": "song", "song_ids": ["s…"],
                  "ranges": [[2590, 2710]], "process": false, "total_sec": 108.3,
                  "have_sec": 16.7, "fetching_sec": 8.3, "failed_sec": 0,
                  "remaining_sec": 91.6, "force": false,
                  "songs": [ { "song_id": "s…", "total_sec": 108.3, "have_sec": 16.7, "failed_sec": 0 } ],
                  "created_at": "…" } ],
  "paused": false,                  // app-wide
  "have_sec": 266.7, "fetching_sec": 16.7, "queued_sec": 91.7, "failed_sec": 16.7,
  "eta_sec": 21,                    // this project's queue at the current speed
  "bytes": 61354596,                // video on disk (not the view cache)
  "local": false,                   // local recording: everything is "have"
  "offline": null,                  // or {since, retry_in_sec, reason}: the network is down,
                                    // downloads wait and retry by themselves (app-wide)
  "warnings": []                    // e.g. a corrupt video/index.json that was set aside
}
```

States map to the map's video lane (14.3): `have` 받음, `fetching` 받는 중,
`queued` 받을 예정, `failed` 받지 못함 (`code`: `NETWORK`, `CDN_TRUNCATED`,
`VOD_GONE`, `AUTH_REQUIRED`, `FORMAT`, `IO`), no range = 없음. **받을 수 없음**
= no range while `project.vod.status == "gone"` (nothing can be fetched any
more). A song's status (▣ / ⟳ / ⛔) is its `[start, end]` against the `have`
ranges (margins never warn).

### Requests and the scheduler

- `video.fetch {id, ranges?, song_ids?, priority?, reason?, process?, height?}`
  queues one request; `song_ids` mean song ± the project's margins. Segments
  already on disk are skipped; segments queued or downloading for another
  request are shared (never fetched twice). Returns a `FetchPlan`
  (`request_id` — `null` when nothing was missing —, `total_sec`,
  `missing_sec`, `estimated_bytes`, rounded `ranges`). A new request clears
  earlier failures inside its ranges (= retry).
- `video.estimate` — same arguments, nothing queued (for tooltips such as
  "11곡 · 약 52분 분량 · 약 3.2GB").
- **Priority**: `now` (what the user looks at, [여기 영상 받기], [먼저 받기]) >
  `user` (default; request order) > `auto` (top-ups after boundary edits) >
  `batch` (overnight queue). Within a priority, requests are served in the
  order they were made, **across projects**. `video.prioritize {id,
  request_id, priority}` moves one (`now` puts it first).
- **At most 4 HTTP requests are in flight in the whole app** (audio and video,
  all projects and jobs together; `system.net_stats` shows the counters).
- **Offline** (R-3): an error that means "no network" (no connection, no
  HTTP status, a cut transfer, resolving fails for network reasons) does
  **not** fail the segment. Every video download of the app pauses and is
  retried after 5 s, 10 s, 20 s … up to 5 min; `VideoState.offline =
  {since, retry_in_sec, reason}` (in every project's state) until a download
  works again. Requests stay queued and keep their order — show "인터넷 연결을
  기다리는 중" instead of failures; nothing to do for the user.
- **Retries**: each segment is tried 4 times (3 retries, 1 / 2 / 4 s apart);
  an expired playback token is re-resolved once; a CDN copy that is truncated
  is fetched from the next lower rendition (that segment only). Then the
  segment is `failed` (kept across restarts). `video.retry {id, ranges?}`
  clears failures and fetches them again (also segments shorter than 0.4 s,
  e.g. the VOD's last one).
- **Rendition identity** (A-1): a rendition is known by its key
  (`"<w>x<h>@<bandwidth>[/<codecs>]"`), never by its position in the
  platform's list (which can change between resolves). `video/v<N>/` is the
  folder number given to a key the first time it is used
  (`video/index.json` `renditions`), never reused. A download looks its key up
  in the freshly resolved list; a key no longer offered fails that segment
  with `FORMAT` (other renditions continue).
- **Rendition**: a missing stretch that touches video on disk, being fetched,
  or an adopted (protocol-2) clip continues that rendition — looking across
  the queued segments of the gap, so the far end of a top-up already uses it;
  the left neighbour wins over the right. Only a stretch with no video next
  to it uses `height` (default `settings.video_height`: the best rendition at
  or below it, else the closest one — e.g. 360 → SOOP's 540p). If the
  neighbour's rendition isn't offered any more (an adopted clip's resolution
  is looked up in the current list), `height` is used and the join is handled
  like any rendition change (separation re-decodes across it; export
  re-encodes the joint). `FetchPlan.estimated_bytes` uses the rendition each
  segment will actually get.
- **Pause**: `video.pause {paused}` stops every video download of the app
  (downloads in progress are dropped and redone later) and is remembered
  across restarts (in the `--config` file). While paused nothing starts,
  top-ups included — except requests a job waits for (`export.run {fetch:
  true}`, the batch queue's video step): they have `RequestStatus.force: true`
  and download anyway (R-11), so an export never waits forever.
- **While a cleanup runs** (`media.drop`), `video.fetch` for that project
  fails with `VIDEO_BUSY` and nothing starts for it.
- **Cancel**: `video.cancel {id, request_id?}` (all of the project's without
  `request_id`) — segments not started are dropped, video already fetched stays.
- **Restart**: pending requests live in `video/index.json` and resume when the
  engine starts.
- **Space**: before queuing, the estimated size (+ 512 MB) must fit on the
  projects drive, else `NO_SPACE` with `data: {need, free}`.
- **Replay gone**: if the VOD can't be resolved (deleted / private / expired),
  the project's requests end (`video.request_done` status `failed`, code
  `VOD_GONE`), `project.vod` becomes `gone` and new `video.fetch` calls fail
  with `VOD_GONE` until a `vod.check` finds it alive again.
- `video.request_done {project_id, request_id, status, failed, code}` ends
  every request: `status` `done` (everything is on disk), `failed` (`failed` =
  ranges that couldn't be fetched, `code` = first reason) or `cancelled`.
- `process: true`: when the request ends, the engine runs `stems.ensure`
  for its songs / ranges (queued after a running one) — also when some
  segments failed (what arrived is separated); the pending separation is kept
  in `video/index.json` across restarts.
- `video.fetch {song_ids}` marks those songs `video_wanted: true` (A-5) unless
  `priority: "auto"` or `reason: "topup"`/`"auto"`: the "user asked for this
  song's video" flag now lives in the project (see protocol 4).

### Playing video (`video.view`)

`video.view {id, t, end?}` → `VideoView {file, path, origin, start, end,
height, kind}`: an MP4 (H.264 + audio) holding the picture at VOD time `t`:

- `kind: "segments"` — a remux of the contiguous same-rendition stretch around
  `t` (whole stretch up to 30 min, else a 30-min window on a 10-minute grid,
  starting at least 10 min before `t`, so nearby seeks share one file),
  built on demand (a long stretch takes seconds; cached, the 3 most recently
  used views per project are kept in `video/views/`). While the stretch grows
  (a top-up is downloading) the existing view is returned as long as it
  holds `t` (and `end`, if given); ask again past its `end` to get the
  longer one. `end` is capped at `t + 30 min`.
- `kind: "legacy"` — a clip adopted from a protocol-2 project
  (`clips/<id>/source.mp4`, played directly).
- `kind: "local"` — a window cut from a local recording.

Play continuously: set `currentTime = vod − origin`; when the playhead leaves
`[start, end]` ask for the view at the new time. If `video.status` shows no
`have` range at `t`, show the cover (받지 않았어요 / 받는 중 / 받지 못함 /
받을 수 없음) instead of calling `video.view` (it fails with `VIDEO_MISSING`).
Serve `file` through the media protocol like the old `clips/<id>/source.mp4`
(whitelist `video/views/*.mp4`).

### Separation (`stems.ensure`)

`stems.ensure {id, song_ids?, ranges?}` (job; default: every enabled song)
separates the parts of the songs ± margins / ranges **that have video** and
aren't separated yet:

- A span that grows is decoded again from the video; the old span is kept
  where its audio matches (it always does for the same rendition) and only the
  new part is separated, with 10 s of already separated context on each side;
  the new (original, vocals) pair takes over in the middle of that context
  with a 1 s raised-cosine crossfade. Measured against a full re-separation:
  ≈ −55 dB (rms, re vocals) across the crossfade, no step in the signal.
- An old span is checked per 0.5 s block; blocks that don't hold the same
  audio (other bytes, or misplaced) are separated again on their own
  (`EnsureResult.reseparated`).
- Across a rendition change each rendition is decoded on its own and placed
  by its own clock (exact to the sample on both sides). The two renditions'
  segments may split the audio differently, so the joint can have a few tens
  of ms without audio (silence with 5 ms fades); the timing never shifts.
- Result `EnsureResult {separated, reseparated, spans, suggestions,
  suggestions_by_song, no_video}` — `suggestions_by_song` maps song id → count.
- **Span length cap** (R-2): a span is at most 30 min. Where songs ± margins
  form a longer separated stretch, spans **overlap by 20 s** instead of
  merging; the engine crossfades across the overlap when it reads stems
  (export). A player reading `stems/<span>/` itself: at a VOD time covered by
  two spans, use the later span (or crossfade over the overlap). Spans made
  before the per-rendition placement fix are checked again where they cross a
  rendition change (`StemSpan.version`).
- Spans are **immutable folders**: growing one writes `stems/<new id>/` and
  removes the old one after `project.changed` lists the new span. Keep reading
  the old files until you switch.
- Talk is searched only in `separated − talk_scanned` (→ `suggestions` new mute
  suggestions, unconfirmed). `settings.suggest_mutes: false` skips it (and
  doesn't mark the range scanned).

### Export from the segment store

`export.run` finds each song's video in whatever was fetched (any number of
requests, renditions, adopted clips) and cuts it exactly: same frames and
audio as if fetched in one piece (video duration within 1 frame of the song,
audio = the render formula to the 24-bit LSB; checked in the E2E runs).

- Songs whose `[start, end]` lacks video: without `fetch` the call fails at
  once with `VIDEO_MISSING`, `data: {ranges, songs: [{song_id, ranges}], vod_gone}`; with
  `fetch: true` their video is queued (`reason: "export"`, downloads even
  while video is paused), the other songs are exported first, then these as
  their video arrives. A song whose video can't
  be fetched (replay gone, failures) ends in `result.failed[]`
  (`{song_id, error}`, error `VIDEO_MISSING` with `data.vod_gone`) — the job
  succeeds if any song was exported.
- Stems are separated on the fly where missing (same as `stems.ensure`).
- A song crossing two renditions (or an adopted clip and new segments) is
  joined on the VOD clock and re-encoded (`video_codec: "copy"` can't join two
  streams; progress note `reencode_joined`). The joint keeps the larger
  rendition's size, then `export_height` / `video_bitrate_k` apply as usual,
  and frame times are kept as they are (no snapping to one frame grid).
- `audio_overrides`: the file's t = 0 is the start of that song's editor
  hand-off files (song start − `handles`, default 3 s).

## Storage (S-E8)

`storage.usage {id}` → `StorageUsage {audio, video, stems, unused_video, cache,
orphan, exports, total, pinned, video_sec, updated_at}` (bytes; `total` = audio
+ video + stems + cache + orphan; `exports` is informational, never cleaned).
`orphan` = leftovers nothing refers to (A-4: stem folders of no span, views
not in the index, interrupted `.part`/`.tmp` writes, e.g. after a crash);
those untouched for an hour are removed when the project is loaded and by
every `media.drop`.
`storage.usage {}` → `{projects: StorageUsage[], free, root}` (free bytes on the
projects drive).

`media.drop {id, level, force?}` → `{project, freed_bytes}`; levels, each
including the previous ones' effect only where stated:

| level | deletes | keeps |
|---|---|---|
| `unused` | video no **enabled** song's `[start − margin, end + margin]` touches (excluded songs don't count; an adopted clip is deleted only if no song touches it) + the caches (views, audio windows) — i.e. `unused_video + cache` | everything else |
| `stems` | separated audio (all spans) + view cache | video, audio |
| `video` | all video (segments, adopted clips) + separated audio + view cache | audio |
| `audio` | all of the above + the analysis audio (`project.audio.dropped = true`) and audio windows | `overview.peaks`, `probs.bin`, the detection |

After level `audio`, `audio.fetch` brings the audio back; if it is the same
audio (same rendition and the same AAC frames in every segment; local: same
track and length) the detection and `probs.bin` are kept, so the map's curves
and "노래 겹침" come back without detecting again. Otherwise (a different
track, the VOD changed) they are dropped as on any new audio.

Decisions are never deleted: songs, boundaries, titles, notes, review state,
mutes (with their state), `talk` / `talk_scanned`, settings, export history.
Exported files are never deleted. Nothing is deleted automatically
(leftovers aside, see `orphan`).
`media.drop` fails with `PINNED` on a kept project unless `force`, with
`BUSY` while the project has a job, and with `VIDEO_BUSY` while video is
queued or downloading for it (`video.cancel` first). The check and the
"cleaning up" mark are one step, so no download can start in between; the
project and `video/index.json` stop referring to the files **before** they
are deleted (a crash in between leaves only leftovers).
`project.set_pinned {id, pinned}` sets 📌.

## Project locations

Projects are created in the projects folder (`serve --projects`), but
`project.list` also covers projects elsewhere, remembered in the projects
folder's `locations.json` (filled once from the old `engine.json`):

- `projects.add_root {path}` → `{projects, skipped}`: list every project in
  another folder — call it with the previous folder after the user picks a new
  projects folder (plan 14.9: old projects keep opening where they are). The
  other locations that folder itself listed (its own `add_root` folders and
  `project.open` projects) come along, so nothing is lost by switching.
- Export records (`song.export.path`) that pointed into a project's own
  `exports/` follow the project when its folder moved or was copied.
- `project.open {path}` → `Project`: one project folder from elsewhere (moved by
  hand, another drive — [다른 위치의 작업 열기]).
- `projects.remove_location {path}` → bool (nothing is deleted),
  `projects.locations` → `{root, roots, projects}`.
- Ids must be unique: the projects folder wins, then roots, then single
  folders; a clash is reported in `skipped` / as `DUPLICATE_PROJECT` with
  `data.dir`.
- `ProjectSummary.dir` is each project's absolute folder: resolve media paths
  against it (not `projects root + id`). Everything inside a project stays
  relative, so these projects work exactly like local ones (video, stems,
  exports into their own `exports/`), and `project.delete` deletes their folder.

## Replay check (S-E9)

`vod.check {id}` → `VodStatus {status: "alive" | "gone" | "unknown", checked_at,
code}` (also stored as `project.vod`, and in `ProjectSummary.vod`). Opening a
project never waits for it; call it in the background. `unknown` with `code`
`NETWORK` = offline, `AUTH_REQUIRED` = login needed. Local recordings: `gone`
when the file is missing.

## Files (read directly by the UI)

Project directory: `<projects>/<project id>/` — every path stored in
`project.json` / `video/index.json` is relative to it, so the folder can be
moved as a whole.

| file | content |
|------|---------|
| `project.json` | state (do not write it from the UI — use the API) |
| `overview.peaks` | whole-VOD waveform, 20 peaks/s — grows **during** `audio.fetch` (re-read when `progress.peaks_until` advances), final after the job; rewritten by `detect.run` |
| `cache/window_*.wav` | `audio.window` results (the 24 newest are kept) |
| `probs.bin` | PANNs curves (CLPROBS1) — for a diagnostic lane |
| `project.json.bak-@<n>` | the file as it was before a schema migration from `@<n>` (kept) |
| `video/index.json` | engine bookkeeping (rendition table, requests, failures, alignment, adopted clips, views) — read `video.status` instead; `.bak` = the previous save, `.bak-v1` = before the rendition table, `.corrupt-<time>` = a file that could not be read |
| `video/v<rendition>/<index>.m4s` \| `.ts` | downloaded HLS segments, exactly the CDN's bytes (init: `init.mp4`) |
| `video/views/<key>.mp4` | playable views (`video.view`), rebuilt on demand |
| `stems/<span id>/orig.wav`, `vocals.wav` | 48 kHz stereo float32 WAV of a stem span (`project.stems[].dir`) |
| `stems/<span id>/orig.peaks`, `vocals.peaks` | span waveforms, 100 peaks/s |
| `clips/<clip id>/source.mp4` | video adopted from a protocol-2 project (a `legacy` view); its stems may live in the same folder (`stems[].dir = "clips/<id>"`) |
| `exports/<name>.<ext>` | results (unless `settings.output_dir`); `<name>` from `settings.filename_template`, never overwritten (`_v2`, `_v3`…) |
| `<projects>/locations.json` | other project roots / folders listed (`projects.*`) |
| `<projects>/.cliper.lock` | held by the running engine (one engine per projects folder) |
| `exports/<name>_stems/` | editor hand-off: `*_original/processed/vocals/accompaniment.wav` (same start & length, 32-bit float), `*_markers.csv`, `<name>.rpp` (Reaper), optional `*_reference.mp4` |

`source.notices` are remarks about the source as codes (`multipart {parts}`,
`multitrack {tracks}`, `audio_only`); `source.notes` is the English text for logs.

Local recordings: the project's `source.platform` is `"local"` and `source.url`
is the file path; `audio.mka` replaces `audio.aac`; the whole recording counts
as video on disk (`VideoState.local`), views are cuts of it
(`settings.audio_track` selects the audio track of a multitrack file). A
recording without video can only be exported as wav/flac/mp3.

**Accompaniment is not stored**: it is `orig − vocals`. The live preview of the
edit is `orig − (1 − g(t))·vocals` with `g` = the mute envelope: 1 outside
mutes; inside a mute, a raised-cosine `fade_in` from 1 down to the floor, the
floor, then a raised-cosine `fade_out` back to 1 — fades lie *inside*
`[start, end]`, each clamped to half the mute's length. The floor is 0 for
`gain_db: null`, else `10^(gain_db/20)`; where mutes overlap the smaller gain
wins. Suggested mutes put their fades *outside* the detected speech
(`start = speech − fade`), so the voice is fully removed from its first
syllable. Export additionally applies
`settings.song_fade_in/out` (raised cosine, min 5 ms) at the song edges and
optional loudness normalisation. Without normalisation, a song whose render
would exceed full scale is turned down just enough (`ExportResult.clip_guard_db`).
With it, `loudness.mode` says whether loudnorm stayed `linear` or had to go
`dynamic` (the gain would break the true-peak ceiling). An AudioWorklet doing
the same math previews the result sample-accurately (loudness aside).

### `CLPEAKS1` (little endian)

```
magic  8 bytes  "CLPEAKS1"
rate   f64      peaks per second
count  u32
data   count × (min i16, max i16)     amplitude × 32767, mono (channel mean)
```

### `CLPROBS1` (little endian)

```
magic    8 bytes "CLPROBS1"
fps      f64     (10)
duration f64     seconds
n        u32
music    n × f32
speech   n × f32
sing     n × f32
```

Detection uses `margin = music − speech` with `Params.pann_hi / pann_lo`
thresholds — useful as a diagnostic lane next to the song ribbon.

## Methods

See `protocol.ts` for exact shapes.

| method | kind | notes |
|--------|------|-------|
| `system.info` | request | versions, ffmpeg, CUDA dirs, model presence, `gpu` (`{name, vram_mb, cuda_ok, reason}`; `cuda_ok: null` until tried), `keep_awake`, `cookies`, `cuda_addon` (`{version, url, size, sha256, dir, installed, needed}` — `needed`: an NVIDIA GPU is present and no CUDA runtime was found → run `addon.install_cuda`) |
| `system.net_stats` | request | `{reset?}` → `NetStats {limit: 4, in_flight, peak, requests}` (app-wide HTTP counters) |
| `addon.install_cuda` | job | `{zip?}` project `_system`. With `zip` (a local copy of the add-on zip, e.g. from a private release) it verifies and unpacks that file (stages `addon.verify` → `addon.extract`); otherwise it downloads the CUDA runtime add-on (≈ 1 GB, pinned SHA-256, resumes dropped connections) from the v0.1.0 release and unpacks it to `%LOCALAPPDATA%\Cliper\addons\cuda-v1`. Progress stages `addon.download` (bytes) → `addon.extract`. Result `{dir, restart_required: true}`: restart the engine, then `system.probe_gpu`. Errors: `NETWORK`, `ADDON` (checksum / archive) |
| `addon.remove_cuda` | request | delete the installed add-on (effective at the next start) |
| `system.probe_gpu` | request | starts the CUDA provider now (loads the detection model, a few s) → `SystemInfo` with `gpu.cuda_ok` known |
| `system.set_cookies` | request | `{cookies: {NID_AUT, NID_SES, …} \| null}` for private/membership VODs → `CookieStatus`. Stored encrypted for the Windows user (DPAPI); survive restarts |
| `system.cookies_status` | request | → `CookieStatus` (`set, names, platforms, saved_at, encrypted, valid, checked_at, account`) — values are never returned |
| `system.check_cookies` | request | asks chzzk whether the cookies are still logged in → `CookieStatus` (`valid`, `account`). SOOP can't be checked (`valid: null`) |
| `system.set_keep_awake` | request | `{enabled}` keep the PC awake while jobs run (persisted in the `--config` file; `serve --no-keep-awake` overrides) |
| `project.list` / `get` / `create` / `delete` | request | `create` takes a URL, chzzk number, `soop:<id>` or a local file path (`source.audio_tracks` lists its audio streams). `list` → `ProjectSummary` with `unreviewed`, `unconfirmed_mutes`, `separated_songs` (enabled songs fully separated), `pinned`, `vod`, `dir` (absolute folder), `has_audio` (false after cleanup level `audio`), `has_detection`, `last_error`. Opening an older project migrates it (see the end); a project written by a newer Cliper fails with `SCHEMA_NEWER` and is not touched |
| `project.update_settings` | request | margins (0–300 s; "앞뒤 여유", per project), rendition height (for video fetched from now on), mute fade, encoder, output dir, `audio_track` (local; run `audio.fetch` again after changing it) |
| `project.set_pinned` | request | `{id, pinned}` → Project |
| `song.update` | request | `{song_id, patch, base_revision?}` partial Song (not `id/export/confidence`; unknown fields → `INVALID_PARAMS`; `mutes` replaces the list; `video_wanted` / `listened` are settable) → Project |
| `song.add` | request | `{song, base_revision?}` → `{project, song_id}` |
| `song.remove` | request | `{song_id, base_revision?}` → Project |
| `mute.upsert` | request | `{song_id, mute, base_revision?}` add (id `""`/unknown) or replace → `{project, mute_id}` |
| `mute.remove` | request | `{song_id, mute_id, base_revision?}` → Project |
| `songs.set` | request | full replacement of `songs` (keep ids; new = `""`); validated; `base_revision?` |
| `vod.check` | request | → `VodStatus` (see above) |
| `project.open` | request | `{path}` → Project (a project folder elsewhere; remembered); `DUPLICATE_PROJECT` (`data.dir`) on a duplicate id |
| `projects.add_root` / `projects.remove_location` / `projects.locations` | request | other folders whose projects `project.list` includes (see Project locations) |
| `video.status` | request | `{id}` → `VideoState`; `{}` → `{paused, projects: {id: VideoState}}` (loaded projects) |
| `video.fetch` | request | `{id, ranges?, song_ids?, priority?, reason?, process?, height?}` → `FetchPlan`. Errors `PRECONDITION` (no audio yet), `VOD_GONE`, `NO_SPACE`, `VIDEO_BUSY` (cleanup running), `INVALID_PARAMS` |
| `video.estimate` | request | same params → `FetchPlan` (nothing queued) |
| `video.cancel` | request | `{id, request_id?}` → `{cancelled}` |
| `video.prioritize` | request | `{id, request_id, priority}` |
| `video.retry` | request | `{id, ranges?}` → `FetchPlan \| null` (failed segments no request holds get a new one) |
| `video.pause` | request | `{paused}` app-wide, persisted |
| `video.view` | request | `{id, t, end?}` → `VideoView`; `VIDEO_MISSING` if there's no video at `t` |
| `stems.ensure` | job | `{id, song_ids?, ranges?}` → `EnsureResult` |
| `storage.usage` | request | `{id}` → `StorageUsage`; `{}` → `{projects, free, root}` |
| `media.drop` | request | `{id, level: unused \| stems \| video \| audio, force?}` → `{project, freed_bytes}`; `PINNED`, `BUSY` (job), `VIDEO_BUSY` (video requests) |
| `audio.window` | request | `{start, duration, sample_rate?=48000, channels?=2}` VOD seconds → `{path, file, …}` float32 WAV of the analysis audio, sample-exact |
| `detect.default_params` / `detect.rerun` | request | rerun is instant (cached probabilities) |
| `audio.fetch` | job | |
| `detect.run` | job | `{params?, retag?}` |
| `export.run` | job | `ExportOptions`: `song_ids?`, `format?` (mp4/wav/flac/mp3), `fetch?` (fetch missing video first), `video_codec?` ("copy" = fast), `export_height?` / `video_bitrate_k?` / `audio_bitrate_k?` (per-run overrides of the settings below), `include_unconfirmed?`, `apply_mutes?` (false = original audio for this run only), `stems?` + `also_render?`, `handles?`, `reference_video?`, `audio_overrides?` (song id → audio file aligned to the hand-off files' start). Result `{exports, stems, failed}` |
| `songs.match_setlist` | request | `{text, tolerance?=150, apply?, overwrite?}` → `SetlistMatch` |
| `project.export_metadata` | request | `{format: csv/json/chapters/timestamps, include_disabled?, path?}` → `{text, path}` |
| `queue.add` / `list` / `cancel` / `clear` | request | batch of inputs; `through`: `detect` (default) or `clips` (= video at priority `batch` + stems); progress via `queue.changed` + the entry's `job_id` |
| `job.list` / `job.cancel` | request | |
| `engine.shutdown` | request | |

`clips.fetch` is no longer a method; it remains only as the job kind of the
batch queue's video step (`queue.add {through: "clips"}`: fetch songs ±
margins at `batch` priority, then separate; result `{video_failed, stems}`).

Only one job **per kind per project** runs at a time, and `audio.fetch`
excludes `detect.run`, `clips.fetch`, `stems.ensure`, `export.run` and
`audio.window` on the same project (`BUSY` otherwise; the message names the
running job). Video downloads aren't jobs: they run beside everything, and
editing songs while they run is fine. `media.drop` is `BUSY` while a job runs
and `VIDEO_BUSY` while video is being fetched for the project. Settings are validated
(`INVALID_PARAMS`: ranges, `video_codec` ∈ `h264_nvenc` / `libx264` / `copy`).
Export size and rate: `settings.export_height` (144–4320, e.g. 1080/720/480/360;
aspect kept, even width, never upscaled; null = fetched size) and
`settings.video_bitrate_k` (100–100000 kbps; VBR with maxrate 1.5× and bufsize
2×; null = constant quality `video_quality`). Both are ignored with `copy`.
`audio_bitrate_k` (32–512) is the mp4 AAC rate. Typical presets:
1080p 4000/128, 720p 2000/128, 480p 1000/96, 360p 600/96.
Project ids are plain names (no path separators).

## Notifications

| method | params |
|--------|--------|
| `engine.ready` | `{protocol, min_client_protocol, system}` — a client built for protocol *v* is compatible when `min_client_protocol ≤ v ≤ protocol` |
| `engine.failed` | `{protocol, code, message, data?}` — sent **instead of** `engine.ready` when the engine can't start (`ROOT_LOCKED`); the process then exits |
| `job.started` | `{job_id, kind, project_id, items}` — `items`: song ids (`clips.fetch`, `export.run`) in processing order, else `[]` |
| `job.progress` | `{job_id, kind, project_id, progress}` — ≤10/s per job (notes always pass). `progress`: `stage, done, total, bytes?, item?, item_index?, items_total?, eta_sec?` (current stage), `peaks_until?` (audio.fetch), `note?` `{code, args?}`, `message?` (English, for logs — don't show it) |
| `job.done` | `{job_id, kind, project_id, result}` (see `JobResults`) |
| `job.failed` | `{job_id, kind, project_id, error: {code, message, data?}}` |
| `project.changed` | `{project}` |
| `queue.changed` | `{queue}` (all entries) |
| `video.changed` | `{video: VideoState}` — a project's coverage / requests changed (≤ 4/s per project) |
| `video.request_done` | `{project_id, request_id, status: done \| failed \| cancelled, failed: [[start, end]], code}` |

`progress.item` carries the song id (`export.run`, `clips.fetch`) the event is
about, so per-song progress can be shown. Progress `stage` values:
`audio.fetch`, `detect.tag`, `video.fetch` (jobs waiting for video: done/total
in ms of video), `separate`, `talk`, `export.run`, `export.render`,
`export.encode` (done/total in ms), `export.stems`.

Note codes: `copy_audio_track`, `find_talk`, `fallback_segments {count}`,
`render_audio`, `measure_loudness`, `write_stems`, `write_reference_video`,
`decode`, `silent_segments {count}` (segments without audio filled with
silence, keeping the timeline), `gpu_out_of_memory` (separation redone on the
CPU), `stems_reseparated` (old stems didn't match the video; separated again),
`reencode_joined` (an export joining two video streams can't stream-copy).
Unknown codes may appear later — ignore them.

A job that panics fails with `INTERNAL` like any other error. Closing stdin
or `engine.shutdown` cancels all jobs; the engine's ffmpeg children run in a
kill-on-close Job Object, so they never outlive it.

**GPU memory**: the separator picks its batch size from free VRAM, halves it
on out-of-memory, then frees the detection model and retries, then separates
on the CPU (`gpu_out_of_memory`).

While any job runs the engine keeps Windows from going to sleep (unless
`system.set_keep_awake {enabled: false}`).

A failed job (not a cancelled one) is also stored as `project.last_error`
(`{kind, code, message, at}`) and cleared when a job of that kind succeeds.

### Resuming after a restart

Video requests resume by themselves (segments on disk are kept; a segment
being downloaded starts over); the app-wide pause is remembered. Jobs don't
survive the engine: `audio.fetch` restarts from scratch (about a minute for
4 h; it downloads to a `.part` file, so the previous audio stays valid until
the new one is complete, and it drops `probs.bin` so the next `detect.run`
re-tags); `stems.ensure` / `clips.fetch` redo only what isn't separated;
`export.run` redoes the songs it is asked for (finished ones have
`song.export`). So re-issuing the same requests resumes the work.

## Errors

JSON-RPC `error = {code, message, data: {code, …details}}`; `code` −32602 for
invalid params, −32601 for unknown methods, −32000 otherwise. `data.code` is
the engine code; some errors add details next to it:

| code | meaning / suggested UI |
|------|------------------------|
| `INVALID_PARAMS` | bug or bad input |
| `NOT_FOUND` | project / song / request id unknown |
| `BUSY` | a job of that kind (or one that excludes it) is running for the project |
| `VIDEO_BUSY` | video is queued / downloading for the project (`media.drop`, `project.delete`: cancel it first), or a cleanup is running (`video.fetch`: retry shortly) |
| `CONFLICT` | `base_revision` is stale — reload and reapply (only this) |
| `DUPLICATE_PROJECT` | `project.open`: a project with the same id is already listed from `data.dir` |
| `SCHEMA_NEWER` | the project (or its `video/index.json`) was written by a newer Cliper: update the app; nothing was changed |
| `ROOT_LOCKED` | (`engine.failed`) another engine uses the projects folder `data.root` |
| `PRECONDITION` | step order (e.g. video before audio.fetch) — show what to do next |
| `UNRECOGNIZED_INPUT` | not a chzzk/SOOP VOD reference |
| `VOD_NOT_FOUND` | deleted/unknown VOD (creating a project) |
| `VOD_GONE` | the replay is gone: only video already downloaded can be used |
| `VIDEO_MISSING` | no video for a range: `data.ranges` always; export adds `data.songs [{song_id, ranges}]` and `data.vod_gone` — offer [받고 내보내기] (`fetch: true`) or, if `vod_gone`, [받은 범위에 맞추기] |
| `NO_SPACE` | `data.need`, `data.free` bytes — offer [정리하기] |
| `PINNED` | the project is kept (📌) — unpin first or confirm (`force`) |
| `AUTH_REQUIRED` | needs login cookies → `system.set_cookies` |
| `NETWORK` | retry |
| `CDN_TRUNCATED` | the CDN holds a broken copy; lower rendition or retry later |
| `FORMAT` | unexpected stream/playlist |
| `FFMPEG_MISSING` / `FFMPEG_FAILED` | install/bundle ffmpeg; show stderr tail |
| `MODEL_MISSING` / `MODEL` | model files / ONNX Runtime problem |
| `CANCELLED` | user cancelled |
| `IO` / `INTERNAL` | unexpected |

## Example

```jsonc
→ {"jsonrpc":"2.0","id":1,"method":"project.create","params":{"input":"https://chzzk.naver.com/video/15533152"}}
← {"jsonrpc":"2.0","method":"project.changed","params":{"project":{…}}}
← {"jsonrpc":"2.0","id":1,"result":{"id":"chzzk-15533152","source":{…},"songs":[],…}}
→ {"jsonrpc":"2.0","id":2,"method":"audio.fetch","params":{"id":"chzzk-15533152"}}
← {"jsonrpc":"2.0","method":"job.started","params":{"job_id":"j…","kind":"audio.fetch","project_id":"chzzk-15533152","items":[]}}
← {"jsonrpc":"2.0","id":2,"result":{"job_id":"j…"}}
← {"jsonrpc":"2.0","method":"job.progress","params":{…,"progress":{"stage":"audio.fetch","done":120,"total":3634,"bytes":12800000}}}
← {"jsonrpc":"2.0","method":"job.done","params":{…,"result":{"audio":{…}}}}
   … detect.run …
→ {"jsonrpc":"2.0","id":3,"method":"video.fetch","params":{"id":"chzzk-15533152","song_ids":["s…"],"process":true}}
← {"jsonrpc":"2.0","id":3,"result":{"request_id":"r…","total_sec":236.0,"missing_sec":236.0,"estimated_bytes":144000000,"ranges":[[…]]}}
← {"jsonrpc":"2.0","method":"video.changed","params":{"video":{"ranges":[{"start":…,"end":…,"state":"fetching","height":1080},…],…}}}
← {"jsonrpc":"2.0","method":"video.request_done","params":{"project_id":"chzzk-15533152","request_id":"r…","status":"done","failed":[],"code":null}}
← {"jsonrpc":"2.0","method":"job.started","params":{"job_id":"j…","kind":"stems.ensure",…}}
```

`tools/ipc_e2e.py` is a runnable reference client covering the whole flow.

## Headless use

`cliper process <inputs…> [--clips] [--export --format mp4|wav|flac|mp3]`
runs the same queue without a UI (one JSON line per input on stdout) — for
scripts that watch channels for new VODs. `--clips` fetches the songs' video
at `batch` priority and separates it. It takes the projects-folder lock too,
so it can't run while the app uses the same folder (`ROOT_LOCKED`); its
preferences come from `%LOCALAPPDATA%\Cliper\engine.json`.

## What changed in protocol 4 (for the UI)

Engine `protocol` = `min_client_protocol` = 4 (review fixes of cycle 2,
`docs/reviews/cycle2/`). A protocol-3 app shows its version notice.

**Removed** (A-8): methods `clips.plan`, `clips.fetch`, `clip.extend`,
`clip.drop_media`, `project.drop_media` (→ `METHOD_NOT_FOUND`; use
`video.fetch` / `stems.ensure` / `media.drop {level: "video"}`); fields
`Project.clips`, `Song.clip_id`; types `Clip`, `Alignment`, `VideoClip`,
`Stems`; `detect.run`'s result `clips`. `ProjectSummary.clips_ready` is renamed
`separated_songs`.

**Error codes split**: `DUPLICATE_PROJECT` (was `CONFLICT` from
`project.open`; `CONFLICT` now only means a stale `base_revision`) and
`VIDEO_BUSY` (was `BUSY` from `media.drop` / `project.delete` when video is
downloading; also `video.fetch` during a cleanup). `VIDEO_MISSING.data` always
has `ranges`. New `SCHEMA_NEWER`, `ROOT_LOCKED`.

**New in the project** (A-5): `Project.uid` (random, made once; a copied
folder keeps it — key app-side caches by it instead of the `created_at`
check), `Song.video_wanted` (set by `video.fetch {song_ids}` for user
requests; settable with `song.update`), `Song.listened` (set it with
`song.update` when the user has listened to a song in review). Fields a newer
Cliper wrote are kept as they are.

**Video state**: `VideoState.offline` (network down, waiting — see Requests),
`VideoState.warnings`, `RequestStatus.force`. `StorageUsage.orphan`.
`SystemInfo.prefs_path`, `SystemInfo.warnings`.

**Engine start**: `serve --config <file>` for the app preferences (default
`%LOCALAPPDATA%\Cliper\engine.json`; the old `<projects>/engine.json` is read
once to fill it); the location list moved to `<projects>/locations.json`. A
second engine on the same projects folder gets `engine.failed` /
`ROOT_LOCKED`.

**Project files**: schema `cliper/project@3`. Opening an older project backs it
up as `project.json.bak-@<n>` and migrates it (`@1` clips → segment store,
`@2` → drop the clip fields, add `uid`). Video index version 2 (rendition
table); index v1 is kept as `index.json.bak-v1`.

**What the UI must change**

1. Handle `engine.failed` (before any `engine.ready`): `ROOT_LOCKED` → "다른
   Cliper가 이 작업 폴더를 쓰고 있어요" with `data.root`; don't restart the
   engine in a loop.
2. Show `system.warnings` once (a settings file was unreadable and set aside).
3. Video lane / jobs panel: when `VideoState.offline` is set, show "인터넷
   연결을 기다리는 중 · {retry_in_sec}초 뒤 다시" for queued video instead of
   failures or a stuck spinner; requests resume by themselves.
4. Replace the userData flags with the project's: `videoSongs` →
   `Song.video_wanted` (the engine sets it on `video.fetch {song_ids}`; the
   app no longer needs `markVideoSongs`; clear it with `song.update` if the
   user un-asks), `listened` → `Song.listened` (write with `song.update`), and
   key per-project UI state by `Project.uid` (drop the `created_at` QA-5
   check). One-time import: for projects with a stored `videoSongs` /
   `listened` list, write them with `song.update` once.
5. `media.drop` / `project.delete`: treat `VIDEO_BUSY` as "영상을 받는
   중이에요 — 받기를 멈춘 뒤 다시"; `video.fetch` `VIDEO_BUSY` → retry after
   the cleanup ends.
6. Storage screen: show `orphan` (e.g. "남은 임시 파일") inside the total.
7. Jobs panel: a request with `force: true` downloads while paused — label it
   (e.g. "내보내기용 · 일시정지 중에도 받는 중").
8. Stems player: spans may overlap by 20 s (the 30-min cap); at a time two
   spans cover, read the later span.
9. Already done for typecheck in this round (no behaviour change):
   `protocol.ts` (version 4, removed methods, `engine.failed`), `edits.ts`
   (new songs get `video_wanted/listened: false`), `locations.ts`
   (`DUPLICATE_PROJECT`), `Home.tsx`/`projects.ts` (`separated_songs`),
   `DetectPopover.tsx` (no clip check), the main allowlist and timeouts.

## What changed in protocol 3 (for the UI)

Engine `protocol` = `min_client_protocol` = 3: a protocol-2 app shows its
version notice. Moving the UI over:

**Gone / changed in `Project`**

- `clips` is always `[]` and `Song.clip_id` always `null` (fields kept so old
  code still parses). There is no clip planning any more: drop "neighbouring
  songs come along", `clip.extend` ("다시 받아야 해요" dialog) and per-clip
  progress.
- Video coverage is not in the project: use `video.status` + `video.changed`.
- Stems: `project.stems: StemSpan[]` (VOD-addressed, see the time model)
  replaces `clip.stems` / `clip.offset`; the audio player loads a window of
  the span covering the song (`stems/<span>/orig.wav` + `vocals.wav`; frame
  `round((t − span.start)·48000)`). Spans change identity when they grow —
  switch on `project.changed`.
- `clip.talk` → `project.talk`; `project.talk_scanned` is new.
- New: `project.pinned`, `project.vod`, `audio.dropped`; `ProjectSummary.pinned`,
  `.vod`; `clips_ready` now = enabled songs fully separated.
- `settings.margin_before/after` are the "앞뒤 여유" (0–300 s); set both to the
  same value (Q-S3). They only shape requests from now on.

**Player** (`video.view`): replace "the clip's `source.mp4` + `vod_start +
offset`" with `view.file` + `view.origin`; follow the playhead with
`video.view` (seamless inside a stretch, cover outside). The media protocol
whitelist needs `video/views/*.mp4` and `stems/<id>/{orig,vocals}.{wav,peaks}`
(legacy views and stems stay under `clips/<id>/`).

**Fetching**: `clips.fetch` still works (fetch + separate songs), but the
plan's flows map to `video.fetch`: [확인한 곡 영상 받기] = `song_ids`, user
priority, `process: true`; the selected song / [여기 영상 받기] (±60 s around
the playhead) / [먼저 받기] = priority `now`; boundary top-ups (3 s after the
last edit, ≤ 5 min, only songs whose video was requested before) = priority
`auto`, reason `topup`; [영상 맞추기] = the songs' ranges; [받은 곡에도 적용]
(margin change) = `song_ids` of those songs; overnight = `batch`. Show
`video.estimate` numbers in tooltips; the jobs panel line per project comes
from `VideoState.requests` / `eta_sec`; [일시정지] = `video.pause`.
When boundaries shrink, cancel the unstarted top-up with `video.cancel`
(fetched video stays).

**Separation / suggestions**: call `stems.ensure {song_ids}` when entering
멘트 다듬기 for a song whose range has video but isn't (fully) separated, or
use `process: true`. Parts not separated yet play the original
(`orig − vocals` unavailable there) — show the badge from 14.8.

**Export**: `export.run {fetch: true}` = [받고 내보내기]; without it a song
lacking video fails with `VIDEO_MISSING` and its ranges. `result.failed[]`
lists songs that couldn't be exported while others were.

**Storage / reopen**: `storage.usage`, `media.drop` (4 levels), `project.set_pinned`;
`vod.check` in the background on open; `project.vod.status == "gone"` → banner,
no fetch buttons.

**After protocol 3 shipped (C2-1..C2-3)**: top-ups next to an adopted clip
keep its rendition (no `reencode_joined` / re-separation for those songs);
`RequestStatus.songs[]` gives per-song progress inside a request;
`ProjectSummary.dir` + `project.open` / `projects.add_root` for projects
outside the projects folder — the media protocol must resolve a project's
files under `dir`, and after switching the projects folder the app calls
`projects.add_root {old folder}` once (the list lives in the new folder's
`engine.json`; `locations.json` since protocol 4).

**Migration of existing projects**: opening a protocol-2 project converts it
in place, once, without any download or separation: each downloaded clip's
`source.mp4` stays where it is and covers the HLS segments it was made from
(a `legacy` view), its stems become a span in `clips/<id>/`, its talk regions
go to `project.talk` and its range to `talk_scanned` (deleted suggestions stay
deleted). Planned (not downloaded) clips vanish. Growing such a song fetches
only the new segments (same rendition) and separates only the new part.
