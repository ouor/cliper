import { useEffect, useRef } from "react";
import { FastForward, Pause, Play, Repeat, Rewind, Square, Volume2, VolumeX } from "lucide-react";
import { player, usePlayer } from "@/audio/player";
import { fmtTime } from "@/lib/format";

export function Transport({ total, label }: { total: number; label?: string }) {
  const playing = usePlayer((s) => s.playing);
  const rate = usePlayer((s) => s.rate);
  const buffering = usePlayer((s) => s.buffering);
  const loopOn = usePlayer((s) => s.loopOn);
  const loop = usePlayer((s) => s.loop);
  const volume = usePlayer((s) => s.volume);
  const streamError = usePlayer((s) => s.streamError);
  const timeRef = useRef<HTMLSpanElement>(null);

  useEffect(
    () =>
      player.onFrame((pos) => {
        if (timeRef.current) timeRef.current.textContent = fmtTime(pos, 1, total >= 3600);
      }),
    [total],
  );

  return (
    <div className="transport">
      <span className="tp-time mono">
        <span ref={timeRef}>{fmtTime(player.now(), 1, total >= 3600)}</span>
        <span className="faint"> / {fmtTime(total, 0, total >= 3600)}</span>
      </span>
      <div className="tp-buttons">
        <button className="btn icon sm ghost" onClick={() => player.shuttle(-1)} data-tip="뒤로 재생 (J, 반복 시 배속)" data-tip-pos="top">
          <Rewind size={15} />
        </button>
        <button className="btn icon sm ghost" onClick={() => player.pause()} data-tip="정지 (K)" data-tip-pos="top">
          <Square size={13} />
        </button>
        <button className="btn icon tp-play" onClick={() => player.toggle()} data-tip="재생 / 정지 (Space)" data-tip-pos="top">
          {playing ? <Pause size={16} /> : <Play size={16} />}
        </button>
        <button className="btn icon sm ghost" onClick={() => player.shuttle(1)} data-tip="앞으로 재생 (L, 반복 시 배속)" data-tip-pos="top">
          <FastForward size={15} />
        </button>
      </div>
      {playing && Math.abs(rate) !== 1 && <span className="tag info">{rate < 0 ? "뒤로 " : ""}{Math.abs(rate)}x</span>}
      {buffering && (
        <span className="row faint">
          <span className="spinner" /> 소리 불러오는 중
        </span>
      )}
      {streamError && <span className="tag danger" data-tip={streamError} data-tip-pos="top">소리를 불러오지 못했어요</span>}
      <button className={`btn sm ${loopOn ? "active" : "ghost"}`} onClick={() => player.toggleLoop()} disabled={!loop} data-tip={loop ? `${fmtTime(loop[0], 1)} ~ ${fmtTime(loop[1], 1)} 반복` : "반복할 구간이 없어요"} data-tip-pos="top">
        <Repeat size={14} /> 구간 반복 {loopOn ? "켬" : "끔"} <kbd>Ctrl+L</kbd>
      </button>
      {label && <span className="faint tp-label">{label}</span>}
      <span className="spacer" />
      <div className="tp-volume">
        <button className="btn icon xs ghost" onClick={() => player.setVolume(volume > 0 ? 0 : 0.9)} aria-label="소리 끄기">
          {volume > 0 ? <Volume2 size={15} /> : <VolumeX size={15} />}
        </button>
        <input type="range" className="range" min={0} max={1} step={0.01} value={volume} onChange={(e) => player.setVolume(Number(e.target.value))} aria-label="볼륨" />
      </div>
    </div>
  );
}
