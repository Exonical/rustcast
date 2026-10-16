"use client";

import * as PopoverPrimitive from "@radix-ui/react-popover";
import type { ReactNode } from "react";

export const Popover = PopoverPrimitive.Root;
export const PopoverTrigger = PopoverPrimitive.Trigger;

export function PopoverContent({ children, className = "", align = "end" }: {
  children: ReactNode;
  className?: string;
  align?: "start" | "center" | "end";
}) {
  return (
    <PopoverPrimitive.Portal>
      <PopoverPrimitive.Content
        data-local-control
        align={align}
        sideOffset={10}
        className={`z-[60] w-[min(22rem,calc(100vw-1.5rem))] rounded-2xl border border-white/10 bg-[#111318]/95 p-4 text-zinc-100 shadow-2xl backdrop-blur-2xl outline-none ${className}`}
      >
        {children}
      </PopoverPrimitive.Content>
    </PopoverPrimitive.Portal>
  );
}
