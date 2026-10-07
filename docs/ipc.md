# Cliper engine IPC

The UI never does media work itself. It spawns the Rust engine and talks to it
over **newline-delimited JSON-RPC 2.0 on the child's stdin/stdout**.

```
cliper.exe serve --projects <dir> --models <dir> [--cpu]
```

- One JSON object per line, UTF-8, both directions.
- **stdout carries protocol lines only**; logs go to stderr (`RUST_LOG=info` for more).
- Requests run concurrently; match responses by `id`.
- Types: `app/src/ipc/generated/*.ts` (generated from Rust with ts-rs) and the
  method/notification map in `app/src/ipc/protocol.ts`.
  Regenerate after changing Rust types:
  `cd core && cargo test -p cliper-core --features ts export_bindings`.

## Lifecycle

1. Spawn `cliper serve`. The first line is the `engine.ready` notification
   (`{ protocol, system }`); check `system.models` and `system.ffmpeg` and
   guide the user if something is missing.
2. Send requests; listen for notifications.
3. On quit, send `engine.shutdown` (cancels jobs) or close stdin.

## Workflow

```
project.create {input}            → Project (VOD URL / chzzk number / soop:<id> / local file path;
                                              reopening returns the existing project)
audio.fetch {id}                  → job   (audio only, ~3% of the video bytes; 4 h VOD ≈ 1 min;
                                           local file: copies the audio track)
detect.run {id}                   → job   (PANNs on GPU ≈ 15 s for 4 h; creates songs (with
                                           confidence) + candidates + planned clips)
  songs.set / songs.match_setlist / detect.rerun / project.update_settings   (instant)
clips.fetch {id, song_ids?}       → job   (video of songs ± margins, stems, talk → mute
                                           suggestions, unconfirmed)
  songs.set (in/out, mutes: confirm / strength / fades)  (instant)
  clip.extend + clips.fetch       (when a song must grow beyond its fetched clip)
export.run {id, song_ids?, format?, stems?, …} → job   (mp4 / wav / flac / mp3 per song, or
                                                       editor stems + markers)
project.export_metadata {id, format}           (CSV / JSON / chapters / timestamps)

queue.add {inputs, through}       batch: audio → detect (→ clips) for many VODs, sequentially
```

### Review state (what the UI shows as "확인 전 / 확인됨")

- **Songs**: `reviewed` (boundaries checked by the user), `enabled` (false =
  excluded but kept, so detection reruns don't bring it back), `note`, `artist`,
  and `confidence` from the detector (`doubts`: `short`, `long`, `weak_singing`,
  `talk_inside`, `resplit` — show "애매함" when non-empty; `null` for manual songs).
  `detect.rerun` / `detect.run` replace only untouched auto songs: not reviewed,
  no title/artist/note/mutes, enabled, `mute_enabled`, no own fades, not
  exported, and not inside a clip whose video was downloaded (a merely
  *planned* clip doesn't count).
- **Per-song switches**: `mute_enabled: false` exports the original audio
  (mutes are kept for later); `fade_in` / `fade_out` override
  `settings.song_fade_in/out` for that song (`null` = project default).
- **Candidates**: `detection.candidates` are music-like stretches the detector
  rejected (`too_short`, `no_singing`, `talk`) — draw them as dashed "maybe a
  missed song" bars; adding one = `song.add` with a new manual song.
- **Mutes**: suggestions arrive with `origin: "auto", confirmed: false`.
  **Export applies only confirmed mutes** (unless `include_unconfirmed`).
  Confirm = `mute.upsert` with `confirmed: true`. Mutes created by the user
  (`origin: "manual"`) are confirmed automatically. Re-running `clips.fetch`
  refreshes only unconfirmed suggestions.
- **Mute strength**: `gain_db: null` removes the voice; a negative number only
  turns it down (e.g. `-12`). Default for new suggestions: `settings.mute_gain_db`.

Every mutation — from a request or a job — is followed by `project.changed`
with the full new `Project`. The UI can treat that as the single source of truth.

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

- **All times in `Project` are seconds on the VOD timeline** (song `start/end`,
  mute `start/end`, clip `start/end`, `clip.talk`).
- VOD time *t* is sample *t·rate* of the **fully decoded analysis audio**
  (`audio.aac` / `audio.mka` decoded from its start). The detector, the
  overview peaks and clip alignment all use that timeline. `audio.window`
  returns sample-exact windows of it (verified identical to a full decode), so
  songs can be previewed before any video is fetched.
- A clip's files (`source.mp4`, `orig.wav`, `vocals.wav`, peaks) have their own
  time base starting at 0 (the file's timeline: stems are padded if the audio
  stream starts after the video). Convert with
  `clip_time = vod_time − clip.video.vod_start − clip.offset`.
- `offset` is **predicted** from the segment index and **confirmed** by
  cross-correlating the clip audio with the analysis audio (typically tens of
  ms). `clip.alignment = {method, corr, predicted, ok}`: `ok: false` (method
  `"predicted"`) means the audio couldn't be matched (e.g. silence) — flag the
  clip's songs for a timing check.
- Clips are planned as songs ± margins, merged when they overlap, but a merged
  clip stops growing at 20 min (`MAX_CLIP_SEC`). Separation streams, so memory
  doesn't depend on clip length.
- `clip.video.duration` is the fetched span; a song (and its mutes) must lie in
  `[vod_start + offset, vod_start + offset + duration]` to be exportable. If the
  user drags a song edge outside, call `clip.extend` then `clips.fetch`.

## Files (read directly by the UI)

Project directory: `<projects>/<project id>/`

| file | content |
|------|---------|
| `project.json` | state (do not write it from the UI — use the API) |
| `overview.peaks` | whole-VOD waveform, 20 peaks/s — grows **during** `audio.fetch` (re-read when `progress.peaks_until` advances), final after the job; rewritten by `detect.run` |
| `cache/window_*.wav` | `audio.window` results (the 24 newest are kept) |
| `probs.bin` | PANNs curves (CLPROBS1) — for a diagnostic lane |
| `clips/<clip id>/source.mp4` | video for preview (`<video>`) |
| `clips/<clip id>/orig.wav`, `vocals.wav` | 48 kHz stereo float32 WAV stems (paths also in `clip.stems`) |
| `clips/<clip id>/orig.peaks`, `vocals.peaks` | clip waveforms, 100 peaks/s |
| `exports/<name>.<ext>` | results (unless `settings.output_dir`); `<name>` from `settings.filename_template`, never overwritten (`_v2`, `_v3`…) |
| `exports/<name>_stems/` | editor hand-off: `*_original/processed/vocals/accompaniment.wav` (same start & length, 32-bit float), `*_markers.csv`, `<name>.rpp` (Reaper), optional `*_reference.mp4` |

`source.notices` are remarks about the source as codes (`multipart {parts}`,
`multitrack {tracks}`, `audio_only`); `source.notes` is the English text for logs.

Local recordings: the project's `source.platform` is `"local"` and `source.url`
is the file path; `audio.mka` replaces `audio.aac`; clips are stream-copied
cuts of the recording (`settings.audio_track` selects the audio track of a
multitrack file). A recording without video can only be exported as wav/flac/mp3.

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
| `system.info` | request | versions, ffmpeg, CUDA dirs, model presence, `gpu` (`{name, vram_mb, cuda_ok, reason}`; `cuda_ok: null` until tried), `keep_awake`, `cookies` |
| `system.probe_gpu` | request | starts the CUDA provider now (loads the detection model, a few s) → `SystemInfo` with `gpu.cuda_ok` known |
| `system.set_cookies` | request | `{cookies: {NID_AUT, NID_SES, …} \| null}` for private/membership VODs → `CookieStatus`. Stored encrypted for the Windows user (DPAPI); survive restarts |
| `system.cookies_status` | request | → `CookieStatus` (`set, names, platforms, saved_at, encrypted, valid, checked_at, account`) — values are never returned |
| `system.check_cookies` | request | asks chzzk whether the cookies are still logged in → `CookieStatus` (`valid`, `account`). SOOP can't be checked (`valid: null`) |
| `system.set_keep_awake` | request | `{enabled}` keep the PC awake while jobs run (persisted; `serve --no-keep-awake` overrides) |
| `project.list` / `get` / `create` / `delete` | request | `create` takes a URL, chzzk number, `soop:<id>` or a local file path (`source.audio_tracks` lists its audio streams). `list` → `ProjectSummary` with `unreviewed`, `unconfirmed_mutes`, `clips_ready`, `has_audio`, `has_detection`, `last_error` |
| `project.update_settings` | request | margins, rendition height, mute fade, encoder, output dir, `audio_track` (local; run `audio.fetch` again after changing it) |
| `song.update` | request | `{song_id, patch, base_revision?}` partial Song (not `id/clip_id/export/confidence`; `mutes` replaces the list) → Project |
| `song.add` | request | `{song, base_revision?}` → `{project, song_id}` |
| `song.remove` | request | `{song_id, base_revision?}` → Project |
| `mute.upsert` | request | `{song_id, mute, base_revision?}` add (id `""`/unknown) or replace → `{project, mute_id}` |
| `mute.remove` | request | `{song_id, mute_id, base_revision?}` → Project |
| `songs.set` | request | full replacement of `songs` (keep ids; new = `""`); validated; `base_revision?` |
| `clips.plan` | request | re-plan clips (normally automatic). `clip.song_ids` lists the songs a clip holds — show "neighbouring songs come along" before fetching. Planned clip ids stay stable across re-planning |
| `clip.extend` | request | new VOD range for a clip; clears its media |
| `clip.drop_media` / `project.drop_media` | request | `{clip_id}` / `{}` delete downloaded video + stems (songs, mutes, edits stay) → `{project, freed_bytes}` |
| `audio.window` | request | `{start, duration, sample_rate?=48000, channels?=2}` VOD seconds → `{path, file, …}` float32 WAV of the analysis audio, sample-exact |
| `detect.default_params` / `detect.rerun` | request | rerun is instant (cached probabilities) |
| `audio.fetch` | job | |
| `detect.run` | job | `{params?, retag?}` |
| `clips.fetch` | job | `{clip_ids?, song_ids?, exact?}` default: clips missing video or stems. `exact` + `song_ids`: plan clips covering only those songs (± margins) so close neighbours aren't fetched along (such clips are `pinned`). Edits while it runs are fine: it re-reads the plan between clips, pins the clip it works on, and skips clips re-planned away |
| `export.run` | job | `ExportOptions`: `song_ids?`, `format?` (mp4/wav/flac/mp3), `video_codec?` ("copy" = fast), `include_unconfirmed?`, `apply_mutes?` (false = original audio for this run only), `stems?` + `also_render?`, `handles?`, `reference_video?`, `audio_overrides?` (song id → audio file aligned to clip start) |
| `songs.match_setlist` | request | `{text, tolerance?=150, apply?, overwrite?}` → `SetlistMatch` |
| `project.export_metadata` | request | `{format: csv/json/chapters/timestamps, include_disabled?, path?}` → `{text, path}` |
| `queue.add` / `list` / `cancel` / `clear` | request | batch of inputs; `through`: `detect` (default) or `clips`; progress via `queue.changed` + the entry's `job_id` |
| `job.list` / `job.cancel` | request | |
| `engine.shutdown` | request | |

Only one job **per kind per project** runs at a time, and `audio.fetch` excludes
`detect.run`, `clips.fetch` and `audio.window` on the same project (`BUSY`
otherwise; the message names the running job). `clip.extend` / `drop_media` are
`BUSY` while clips are fetched or exported. Settings are validated
(`INVALID_PARAMS`: ranges, `video_codec` ∈ `h264_nvenc` / `libx264` / `copy`).
Project ids are plain names (no path separators).

## Notifications

| method | params |
|--------|--------|
| `engine.ready` | `{protocol, min_client_protocol, system}` — a client built for protocol *v* is compatible when `min_client_protocol ≤ v ≤ protocol` |
| `job.started` | `{job_id, kind, project_id, items}` — `items`: clip ids (`clips.fetch`) / song ids (`export.run`) in processing order, else `[]` |
| `job.progress` | `{job_id, kind, project_id, progress}` — ≤10/s per job (notes always pass). `progress`: `stage, done, total, bytes?, item?, item_index?, items_total?, eta_sec?` (current stage), `peaks_until?` (audio.fetch), `note?` `{code, args?}`, `message?` (English, for logs — don't show it) |
| `job.done` | `{job_id, kind, project_id, result}` (see `JobResults`) |
| `job.failed` | `{job_id, kind, project_id, error: {code, message}}` |
| `project.changed` | `{project}` |
| `queue.changed` | `{queue}` (all entries) |

`progress.item` carries the clip id (`clips.fetch`) or song id (`export.run`) the
event is about, so per-song progress can be shown. Progress `stage` values: `audio.fetch`, `detect.tag`, `video.fetch`,
`video.remux`, `separate`, `talk`, `clips.fetch`, `export.run`,
`export.render`, `export.encode` (done/total in ms), `export.stems`.

Note codes: `copy_audio_track`, `cut_recording`, `find_talk`,
`fallback_segments {count}`, `variant_truncated {resolution}`, `remux`,
`render_audio`, `measure_loudness`, `write_stems`, `write_reference_video`,
`decode`, `alignment_weak {corr}`, `silent_segments {count}` (segments
without audio filled with silence, keeping the timeline), `gpu_out_of_memory`
(separation redone on the CPU). Unknown codes may appear later — ignore them.

A job that panics fails with `INTERNAL` like any other error. Closing stdin
or `engine.shutdown` cancels all jobs; the engine's ffmpeg children run in a
kill-on-close Job Object, so they never outlive it.

**GPU memory**: the separator picks its batch size from free VRAM, halves it
on out-of-memory, then frees the detection model and retries, then separates
that clip on the CPU (`gpu_out_of_memory`).

While any job runs the engine keeps Windows from going to sleep (unless
`system.set_keep_awake {enabled: false}`).

A failed job (not a cancelled one) is also stored as `project.last_error`
(`{kind, code, message, at}`) and cleared when a job of that kind succeeds.

### Resuming after a restart

Jobs don't survive the engine. What is kept: `audio.fetch` restarts from
scratch (about a minute for 4 h; it downloads to a `.part` file, so the
previous audio stays valid until the new one is complete, and it drops
`probs.bin` so the next `detect.run` re-tags); `clips.fetch` skips clips that already have
video and stems (a half-downloaded clip starts over); `export.run` redoes the
songs it is asked for (finished ones have `song.export`). So re-issuing the
same requests resumes the work.

## Errors

JSON-RPC `error = {code, message, data: {code}}`; `code` −32602 for invalid
params, −32601 for unknown methods, −32000 otherwise. `data.code` is the engine code:

| code | meaning / suggested UI |
|------|------------------------|
| `INVALID_PARAMS` | bug or bad input |
| `NOT_FOUND` | project/clip id unknown |
| `BUSY` | same job already running |
| `CONFLICT` | `base_revision` is stale — reload and reapply |
| `PRECONDITION` | step order (e.g. export before clips.fetch) — show what to do next |
| `UNRECOGNIZED_INPUT` | not a chzzk/SOOP VOD reference |
| `VOD_NOT_FOUND` | deleted/unknown VOD |
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
← {"jsonrpc":"2.0","method":"job.started","params":{"job_id":"j…","kind":"audio.fetch","project_id":"chzzk-15533152"}}
← {"jsonrpc":"2.0","id":2,"result":{"job_id":"j…"}}
← {"jsonrpc":"2.0","method":"job.progress","params":{…,"progress":{"stage":"audio.fetch","done":120,"total":3634,"bytes":12800000}}}
← {"jsonrpc":"2.0","method":"job.done","params":{…,"result":{"audio":{…}}}}
```

`tools/ipc_e2e.py` is a runnable reference client covering the whole flow.

## Headless use

`cliper process <inputs…> [--clips] [--export --format mp4|wav|flac|mp3]`
runs the same queue without a UI (one JSON line per input on stdout) — for
scripts that watch channels for new VODs.
