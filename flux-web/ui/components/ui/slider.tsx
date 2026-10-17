"use client";

export function Slider({ label, value, min, max, step = 1, onChange, valueLabel }: {
  label: string;
  value: number;
  min: number;
  max: number;
  step?: number;
  onChange: (value: number) => void;
  valueLabel?: string;
}) {
  return (
    <label data-local-control className="block space-y-2">
      <span className="flex items-center justify-between text-xs font-medium text-zinc-300">
        <span>{label}</span><span className="font-mono text-zinc-500">{valueLabel ?? value}</span>
      </span>
      <input
        data-local-control
        type="range"
        min={min}
        max={max}
        step={step}
        value={value}
        onChange={(event) => onChange(Number(event.currentTarget.value))}
        className="w-full accent-blue-400"
      />
    </label>
  );
}
