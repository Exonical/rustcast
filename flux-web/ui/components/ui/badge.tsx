import type { ReactNode } from "react";

type Tone = "green" | "amber" | "red" | "neutral" | "blue";
const tones: Record<Tone, string> = {
  green: "border-emerald-400/20 bg-emerald-400/10 text-emerald-300",
  amber: "border-amber-400/20 bg-amber-400/10 text-amber-200",
  red: "border-red-400/20 bg-red-400/10 text-red-300",
  neutral: "border-white/10 bg-white/[0.04] text-zinc-400",
  blue: "border-blue-400/20 bg-blue-400/10 text-blue-200",
};

export function Badge({ children, tone = "neutral", className = "" }: {
  children: ReactNode;
  tone?: Tone;
  className?: string;
}) {
  return (
    <span className={`inline-flex items-center gap-1.5 rounded-full border px-2.5 py-1 text-[10px] font-bold uppercase tracking-[0.12em] ${tones[tone]} ${className}`}>
      {children}
    </span>
  );
}
