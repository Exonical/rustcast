export function formatDuration(seconds: number): string {
  const whole = Math.max(0, Math.floor(seconds));
  const hours = Math.floor(whole / 3600);
  const minutes = Math.floor((whole % 3600) / 60);
  const remainder = whole % 60;
  if (hours > 0) return `${hours}h ${String(minutes).padStart(2, "0")}m`;
  return `${minutes}m ${String(remainder).padStart(2, "0")}s`;
}

export function shortUserAgent(userAgent: string): string {
  const browser = userAgent.includes("Edg/") ? "Edge"
    : userAgent.includes("Firefox/") ? "Firefox"
      : userAgent.includes("Chrome/") ? "Chrome"
        : userAgent.includes("Safari/") ? "Safari"
          : "Browser";
  const platform = /Windows/i.test(userAgent) ? "Windows"
    : /Android/i.test(userAgent) ? "Android"
      : /iPhone|iPad|iPod/i.test(userAgent) ? "iOS"
        : /Mac OS|Macintosh/i.test(userAgent) ? "macOS"
          : /Linux/i.test(userAgent) ? "Linux"
            : "Unknown";
  return `${browser} · ${platform}`;
}
