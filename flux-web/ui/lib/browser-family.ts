export type BrowserFamily = "chrome" | "firefox" | "safari" | "edge" | "other";

export function browserFamily(userAgent: string): BrowserFamily {
  const agent = userAgent.toLowerCase();
  if (["edg/", "edge/", "edga/", "edgios/"].some((token) => agent.includes(token))) return "edge";
  if (agent.includes("firefox/") || agent.includes("fxios/")) return "firefox";
  if (agent.includes("chrome/") || agent.includes("chromium/") || agent.includes("crios/")) return "chrome";
  if (agent.includes("safari/")) return "safari";
  return "other";
}
