import Link from "next/link";

export function EmptyState({ title, detail, action }: { title: string; detail: string; action?: { label: string; href: string } }) {
  return (
    <div className="flex min-h-64 flex-col items-center justify-center rounded-2xl border border-dashed border-white/10 bg-white/[0.015] px-6 text-center">
      <div className="mb-4 grid h-12 w-12 place-items-center rounded-2xl border border-white/10 bg-white/[0.03] text-xl text-zinc-500">◌</div>
      <h2 className="text-base font-semibold text-zinc-200">{title}</h2>
      <p className="mt-2 max-w-sm text-sm leading-6 text-zinc-500">{detail}</p>
      {action && <Link href={action.href} className="mt-5 rounded-xl bg-blue-500 px-4 py-2.5 text-sm font-semibold text-white hover:bg-blue-400">{action.label}</Link>}
    </div>
  );
}
