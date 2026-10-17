import type { HTMLAttributes } from "react";

export function Card({ className = "", ...props }: HTMLAttributes<HTMLDivElement>) {
  return (
    <div className={`rounded-2xl border border-white/[0.07] bg-[#111318] shadow-[0_18px_60px_rgba(0,0,0,0.18)] ${className}`} {...props} />
  );
}
