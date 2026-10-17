import type { ReactNode } from "react";

export function PageHeader({ title, description, actions }: {
  title: string;
  description: string;
  actions?: ReactNode;
}) {
  return (
    <header className="mb-8 flex flex-wrap items-end justify-between gap-4">
      <div>
        <p className="mb-2 text-[10px] font-bold uppercase tracking-[0.22em] text-blue-300/80">Flux control plane</p>
        <h1 className="text-3xl font-semibold tracking-[-0.04em] text-white sm:text-4xl">{title}</h1>
        <p className="mt-2 text-sm text-zinc-500">{description}</p>
      </div>
      {actions && <div className="flex items-center gap-2">{actions}</div>}
    </header>
  );
}
