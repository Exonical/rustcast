import type { ButtonHTMLAttributes } from "react";

type Variant = "primary" | "secondary" | "ghost" | "danger";

const styles: Record<Variant, string> = {
  primary: "bg-blue-500 text-white hover:bg-blue-400 shadow-lg shadow-blue-950/40",
  secondary: "border border-white/10 bg-white/[0.04] text-zinc-200 hover:bg-white/[0.08]",
  ghost: "text-zinc-400 hover:bg-white/[0.06] hover:text-white",
  danger: "border border-red-400/20 bg-red-500/10 text-red-200 hover:bg-red-500/20",
};

export function Button({
  variant = "secondary",
  className = "",
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant }) {
  return (
    <button
      data-local-control
      className={`inline-flex items-center justify-center gap-2 rounded-xl px-4 py-2.5 text-sm font-semibold transition disabled:pointer-events-none disabled:opacity-40 ${styles[variant]} ${className}`}
      {...props}
    />
  );
}
