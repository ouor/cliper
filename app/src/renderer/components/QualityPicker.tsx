// 화질 (mp4 export size + bitrate): presets, or 직접 설정 with range-checked inputs.
import { useEffect, useState } from "react";
import { Info } from "lucide-react";
import { QUALITY_PRESETS, RANGES, checkRange, describeQuality, qualityId, type ExportQuality } from "@/lib/quality";

type Field = "height" | "videoK" | "audioK";

const FIELD: Record<Field, { label: string; unit: string; empty?: string }> = {
  height: { label: "높이", unit: "p", empty: "원본" },
  videoK: { label: "영상", unit: "kbps", empty: "일정 품질" },
  audioK: { label: "오디오", unit: "kbps" },
};

const text = (q: ExportQuality): Record<Field, string> => ({ height: q.height?.toString() ?? "", videoK: q.videoK?.toString() ?? "", audioK: String(q.audioK) });

export function QualityPicker({ value, onChange, quality, sourceHeight }: { value: ExportQuality; onChange: (q: ExportQuality) => void; quality?: number; sourceHeight?: number | null }) {
  const id = qualityId(value);
  const [customOpen, setCustomOpen] = useState(id === "custom");
  const custom = customOpen || id === "custom";
  const [t, setT] = useState(() => text(value));
  useEffect(() => setT(text(value)), [value.height, value.videoK, value.audioK]);

  const parse = (s: string): number | null => (s.trim() === "" ? null : Number(s.trim()));
  const errs: Record<Field, string | null> = {
    height: checkRange("height", parse(t.height)),
    videoK: checkRange("videoK", parse(t.videoK)),
    audioK: checkRange("audioK", parse(t.audioK)),
  };
  const commit = () => {
    if (errs.height || errs.videoK || errs.audioK) return;
    const q = { height: parse(t.height), videoK: parse(t.videoK), audioK: parse(t.audioK)! };
    if (q.height !== value.height || q.videoK !== value.videoK || q.audioK !== value.audioK) onChange(q);
  };
  const firstErr = (Object.keys(FIELD) as Field[]).find((f) => errs[f]);
  const noUpscale = value.height !== null && !!sourceHeight && sourceHeight < value.height;

  return (
    <div className="col quality-pick">
      <div className="seg">
        {QUALITY_PRESETS.map((x) => (
          <button
            key={x.id}
            className={!custom && id === x.id ? "on" : ""}
            onClick={() => {
              setCustomOpen(false);
              onChange(x.q);
            }}
          >
            {x.label}
          </button>
        ))}
        <button className={custom ? "on" : ""} onClick={() => setCustomOpen(true)}>
          직접 설정
        </button>
      </div>
      {custom && (
        <div className="row wrap quality-fields">
          {(Object.keys(FIELD) as Field[]).map((f) => (
            <label key={f} className="row quality-field">
              <span className="faint">{FIELD[f].label}</span>
              <input
                className={`input sm mono${errs[f] ? " invalid" : ""}`}
                style={{ width: f === "height" ? 64 : 72 }}
                value={t[f]}
                placeholder={FIELD[f].empty}
                inputMode="numeric"
                aria-invalid={!!errs[f]}
                title={`${RANGES[f][0]}~${RANGES[f][1]}${FIELD[f].empty ? ` · 비우면 ${FIELD[f].empty}` : ""}`}
                onChange={(e) => setT({ ...t, [f]: e.target.value })}
                onBlur={commit}
                onKeyDown={(e) => {
                  e.stopPropagation();
                  if (e.key === "Enter") (e.target as HTMLInputElement).blur();
                }}
              />
              <span className="faint">{FIELD[f].unit}</span>
            </label>
          ))}
        </div>
      )}
      <div className={`hint${firstErr ? " err-text" : ""}`}>
        {firstErr
          ? `${FIELD[firstErr].label}: ${errs[firstErr]}${FIELD[firstErr].empty ? ` (비우면 ${FIELD[firstErr].empty})` : ""}`
          : describeQuality(value, quality)}
      </div>
      {noUpscale && (
        <div className="hint">
          <Info size={12} /> 받은 영상이 {sourceHeight}p라서 그 크기 그대로 만들어요 (키우지 않아요).
        </div>
      )}
    </div>
  );
}
