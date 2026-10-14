// Unit tests for the download rules (review A-9). Run: npm test (node:test, type stripping).
import { test } from "node:test";
import assert from "node:assert/strict";
import type { VideoState } from "@ipc/generated/VideoState";
import type { RequestStatus } from "@ipc/generated/RequestStatus";
import type { Song } from "@ipc/generated/Song";
import { exportDefaults, partsIn, planTopUp, songHasVideo, songsToFit, songVideo, spanAt, spanFor, unseparated } from "./coverage.ts";

function vs(ranges: [number, number, VideoState["ranges"][number]["state"]][], requests: Partial<RequestStatus>[] = []): VideoState {
  return {
    project_id: "p",
    ranges: ranges.map(([start, end, state]) => ({ start, end, state })),
    requests: requests.map((r) => ({
      id: "r",
      priority: "user",
      reason: "song",
      song_ids: [],
      ranges: [],
      process: false,
      force: false,
      total_sec: 0,
      have_sec: 0,
      fetching_sec: 0,
      failed_sec: 0,
      remaining_sec: 0,
      songs: [],
      created_at: "",
      ...r,
    })) as RequestStatus[],
    paused: false,
    have_sec: 0,
    fetching_sec: 0,
    queued_sec: 0,
    failed_sec: 0,
    eta_sec: null,
    bytes: 0,
    local: false,
    offline: null,
    warnings: [],
  };
}

function song(id: string, start: number, end: number, extra: Partial<Song> = {}): Song {
  return { id, title: "", artist: "", start, end, origin: "auto", enabled: true, reviewed: false, note: "", confidence: null, mutes: [], mute_enabled: true, fade_in: null, fade_out: null, export: null, video_wanted: false, listened: false, ...extra };
}

test("partsIn: missing parts of a range", () => {
  const v = vs([[100, 200, "have"], [250, 300, "queued"]]);
  assert.deepEqual(partsIn(v, 50, 320), [[50, 100], [200, 320]]);
  assert.deepEqual(partsIn(v, 50, 320, ["have"]), [[100, 200]]);
});

test("songHasVideo: own range, not a neighbour's margin (QA-4)", () => {
  // a V fetch [1232.9, 1366.2] touches song 05's margin only
  const v = vs([[1232.9, 1366.2, "have"]]);
  assert.equal(songHasVideo(v, song("s05", 1174.8, 1231.2)), false);
  // more than half of the song's own range
  assert.equal(songHasVideo(v, song("s06", 1240, 1400)), true);
  // under half
  assert.equal(songHasVideo(v, song("s07", 1300, 1700)), false);
});

test("songHasVideo: pending request or video_wanted with some coverage (U-15)", () => {
  const v = vs([], [{ song_ids: ["a"], remaining_sec: 30 }]);
  assert.equal(songHasVideo(v, song("a", 0, 100)), true);
  // wanted but everything was cleaned up: no longer counts
  assert.equal(songHasVideo(vs([]), song("b", 0, 100, { video_wanted: true })), false);
  // wanted, a little left
  assert.equal(songHasVideo(vs([[0, 10, "have"]]), song("c", 0, 100, { video_wanted: true })), true);
});

test("planTopUp: shrinking never fetches (QA-4)", () => {
  const v = vs([[1232.9, 1366.2, "have"]]);
  const p = planTopUp({ song: song("s05", 1175.3, 1231.2), was: [1174.8, 1231.2], margin: 30, vs: v, ownRequests: [] });
  assert.deepEqual(p.fetch, []);
});

test("planTopUp: a song without video is left alone", () => {
  const p = planTopUp({ song: song("x", 100, 400), was: [150, 400], margin: 30, vs: vs([]), ownRequests: [] });
  assert.deepEqual(p, { cancel: [], fetch: [], sec: 0 });
});

test("planTopUp: only the side that grew + its margin", () => {
  const v = vs([[3182.9, 3824.5, "have"]]);
  const p = planTopUp({ song: song("s11", 3537.5, 3850), was: [3537.5, 3793.9], margin: 30, vs: v, ownRequests: [] });
  assert.deepEqual(p.fetch, [[3824.5, 3880]]);
  assert.ok(Math.abs(p.sec - 55.5) < 1e-6);
});

test("planTopUp: shrink back cancels only this song's unstarted top-ups (QA-8, U-14)", () => {
  const v = vs(
    [[3182.9, 3824.5, "have"], [3824.5, 3882.9, "queued"]],
    [
      { id: "mine", reason: "topup", ranges: [[3824.5, 3880]], fetching_sec: 0 },
      { id: "neighbour", reason: "topup", ranges: [[3824.5, 3880]], fetching_sec: 0 },
      { id: "started", reason: "topup", ranges: [[3824.5, 3880]], fetching_sec: 4 },
    ],
  );
  const p = planTopUp({ song: song("s11", 3537.5, 3793.9), was: [3537.5, 3850], margin: 30, vs: v, ownRequests: ["mine", "started"] });
  assert.deepEqual(p.cancel, ["mine"]);
  assert.deepEqual(p.fetch, []);
});

test("songsToFit: songs with video whose own range lacks some", () => {
  const v = vs([[0, 80, "have"], [400, 600, "have"]]);
  const proj = { vod: null, songs: [song("a", 0, 100), song("b", 590, 700), song("c", 1000, 1100)] };
  assert.deepEqual(songsToFit(proj, v).map((x) => x.song.id), ["a"]);
});

test("exportDefaults: reviewed songs with video, not songs only a neighbour touched", () => {
  const v = vs([[0, 200, "have"], [190, 230, "have"]]);
  const songs = [song("a", 10, 190), song("b", 220, 400), song("c", 500, 600)];
  assert.deepEqual(exportDefaults({ songs }, v), ["a"]);
  songs[2].reviewed = true;
  assert.deepEqual(exportDefaults({ songs }, v), []);
  songs[0].reviewed = true;
  assert.deepEqual(exportDefaults({ songs }, v), ["a"]);
});

test("songVideo: labels", () => {
  const v = vs([[20, 100, "have"]]);
  assert.equal(songVideo({ vod: null }, v, song("a", 0, 100)).label, "앞 20초 영상 없음");
  assert.equal(songVideo({ vod: null }, v, song("a", 30, 90)).label, "영상 ✓");
  assert.equal(songVideo({ vod: { status: "gone", checked_at: "", code: null } }, vs([]), song("a", 0, 100)).state, "gone");
});

test("spans: overlapping spans read the later one (protocol 4)", () => {
  const sp = (id: string, start: number, end: number) => ({ id, start, end, frames: 0, sample_rate: 48000, dir: id, model: "", device: "cpu" as const, created_at: "", version: 1 });
  const p = { stems: [sp("a", 0, 1800), sp("b", 1780, 3000)] };
  assert.equal(spanAt(p, 1790)?.id, "b");
  assert.equal(spanAt(p, 100)?.id, "a");
  assert.equal(spanFor(p, 1785, 1795)?.id, "b");
  assert.deepEqual(unseparated(p, vs([[0, 3100, "have"]]), 2900, 3100), [[3000, 3100]]);
});
