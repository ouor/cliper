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
  `detect.rerun` / `detect.run` replace only untouched auto songs (not reviewed,
  no title/note/mutes/clip, still enabled).
- **Candidates**: `detection.candidates` are music-like stretches the detector
  rejected (`too_short`, `no_singing`, `talk`) — draw them as dashed "maybe a
  missed song" bars; adding one = `songs.set` with a new manual song.
- **Mutes**: suggestions arrive with `origin: "auto", confirmed: false`.
  **Export applies only confirmed mutes** (unless `include_unconfirmed`).
  Confirm = `songs.set` with `confirmed: true`. Mutes created by the user
  (`origin: "manual"`) are confirmed automatically. Re-running `clips.fetch`
  refreshes only unconfirmed suggestions.
- **Mute strength**: `gain_db: null` removes the voice; a negative number only
  turns it down (e.g. `-12`). Default for new suggestions: `settings.mute_gain_db`.

Every mutation — from a request or a job — is followed by `project.changed`
with the full new `Project`. The UI can treat that as the single source of truth.

## Time model

- **All times in `Project` are seconds on the VOD timeline** (song `start/end`,
  mute `start/end`, clip `start/end`, `clip.talk`).
- A clip's files (`source.mp4`, `orig.wav`, `vocals.wav`, peaks) have their own
  time base starting at 0. Convert with
  `clip_time = vod_time − clip.video.vod_start − clip.offset`
  (`offset` is measured by cross-correlation, typically a few ms).
- `clip.video.duration` is the fetched span; a song (and its mutes) must lie in
  `[vod_start + offset, vod_start + offset + duration]` to be exportable. If the
  user drags a song edge outside, call `clip.extend` then `clips.fetch`.

## Files (read directly by the UI)

Project directory: `<projects>/<project id>/`

| file | content |
|------|---------|
| `project.json` | state (do not write it from the UI — use the API) |
| `overview.peaks` | whole-VOD waveform, 20 peaks/s (after `detect.run`) |
| `probs.bin` | PANNs curves (CLPROBS1) — for a diagnostic lane |
| `clips/<clip id>/source.mp4` | video for preview (`<video>`) |
| `clips/<clip id>/orig.wav`, `vocals.wav` | 48 kHz stereo float32 WAV stems (paths also in `clip.stems`) |
| `clips/<clip id>/orig.peaks`, `vocals.peaks` | clip waveforms, 100 peaks/s |
| `exports/<name>.<ext>` | results (unless `settings.output_dir`); `<name>` from `settings.filename_template`, never overwritten (`_v2`, `_v3`…) |
| `exports/<name>_stems/` | editor hand-off: `*_original/processed/vocals/accompaniment.wav` (same start & length, 32-bit float), `*_markers.csv`, `<name>.rpp` (Reaper), optional `*_reference.mp4` |

Local recordings: the project's `source.platform` is `"local"` and `source.url`
is the file path; `audio.mka` replaces `audio.aac`; clips are stream-copied
cuts of the recording (`settings.audio_track` selects the audio track of a
multitrack file). A recording without video can only be exported as wav/flac/mp3.

**Accompaniment is not stored**: it is `orig − vocals`. The live preview of the
edit is `orig − (1 − g(t))·vocals` with `g` = the mute envelope: 1 outside
mutes; inside a mute, a raised-cosine `fade_in` from 1 down to the floor, the
floor, then a raised-cosine `fade_out` back to 1 — fades lie *inside*
`[start, end]`. The floor is 0 for `gain_db: null`, else `10^(gain_db/20)`;
where mutes overlap the smaller gain wins. Export additionally applies
`settings.song_fade_in/out` (raised cosine, min 5 ms) at the song edges and
optional loudness normalisation. An AudioWorklet doing the same math previews
the result sample-accurately (loudness aside).

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
| `system.info` | request | versions, ffmpeg, CUDA dirs, model presence |
| `system.set_cookies` | request | `{cookies: {NID_AUT, NID_SES} \| null}` for private/membership VODs |
| `project.list` / `get` / `create` / `delete` | request | `create` takes a URL, chzzk number or `soop:<id>` |
| `project.update_settings` | request | margins, rendition height, mute fade, encoder, output dir |
| `songs.set` | request | full replacement of `songs` (keep ids; new = `""`); validated |
| `clips.plan` | request | re-plan clips (normally automatic) |
| `clip.extend` | request | new VOD range for a clip; clears its media |
| `detect.default_params` / `detect.rerun` | request | rerun is instant (cached probabilities) |
| `audio.fetch` | job | |
| `detect.run` | job | `{params?, retag?}` |
| `clips.fetch` | job | `{clip_ids?, song_ids?}` default: clips missing video or stems |
| `export.run` | job | `ExportOptions`: `song_ids?`, `format?` (mp4/wav/flac/mp3), `video_codec?` ("copy" = fast), `include_unconfirmed?`, `stems?` + `also_render?`, `handles?`, `reference_video?`, `audio_overrides?` (song id → audio file aligned to clip start) |
| `songs.match_setlist` | request | `{text, tolerance?=150, apply?, overwrite?}` → `SetlistMatch` |
| `project.export_metadata` | request | `{format: csv/json/chapters/timestamps, include_disabled?, path?}` → `{text, path}` |
| `queue.add` / `list` / `cancel` / `clear` | request | batch of inputs; `through`: `detect` (default) or `clips`; progress via `queue.changed` + the entry's `job_id` |
| `job.list` / `job.cancel` | request | |
| `engine.shutdown` | request | |

Only one job **per kind per project** runs at a time (`BUSY` otherwise).

## Notifications

| method | params |
|--------|--------|
| `engine.ready` | `{protocol, system}` |
| `job.started` | `{job_id, kind, project_id}` |
| `job.progress` | `{job_id, kind, project_id, progress: {stage, done, total, bytes?, message?}}` — ≤10/s per job; `message` = human note |
| `job.done` | `{job_id, kind, project_id, result}` (see `JobResults`) |
| `job.failed` | `{job_id, kind, project_id, error: {code, message}}` |
| `project.changed` | `{project}` |
| `queue.changed` | `{queue}` (all entries) |

`progress.item` carries the clip id (`clips.fetch`) or song id (`export.run`) the
event is about, so per-song progress can be shown. Progress `stage` values: `audio.fetch`, `detect.tag`, `video.fetch`,
`video.remux`, `separate`, `talk`, `clips.fetch`, `export.run`,
`export.render`, `export.encode` (done/total in ms), `export.stems`.

While any job runs the engine keeps Windows from going to sleep.

## Errors

JSON-RPC `error = {code, message, data: {code}}`; `code` −32602 for invalid
params, −32601 for unknown methods, −32000 otherwise. `data.code` is the engine code:

| code | meaning / suggested UI |
|------|------------------------|
| `INVALID_PARAMS` | bug or bad input |
| `NOT_FOUND` | project/clip id unknown |
| `BUSY` | same job already running |
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
