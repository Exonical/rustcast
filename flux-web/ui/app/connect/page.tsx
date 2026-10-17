import { Suspense } from "react";
import { ConnectPageClient } from "@/components/stream/ConnectPageClient";

function Loading() {
  return <main className="grid min-h-screen place-items-center bg-black text-sm text-zinc-500">Preparing your Flux stream…</main>;
}

export default function Page() {
  return <Suspense fallback={<Loading />}><ConnectPageClient /></Suspense>;
}
