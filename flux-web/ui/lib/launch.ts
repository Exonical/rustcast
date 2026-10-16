export interface Resolution {
  width: number;
  height: number;
}

export interface LaunchOptions {
  width?: number;
  height?: number;
  fps?: number;
  quality?: number;
}

export interface ConnectParams extends LaunchOptions {
  machine: string | null;
}

export function validResolution(width: number, height: number): boolean {
  return Number.isInteger(width) && Number.isInteger(height)
    && width >= 640 && height >= 480
    && width <= 2560 && height <= 1440
    && width % 2 === 0 && height % 2 === 0;
}

function boundedInteger(value: string | null, min: number, max: number): number | undefined {
  if (value === null || value.trim() === "") return undefined;
  const number = Number(value);
  return Number.isInteger(number) && number >= min && number <= max ? number || undefined : undefined;
}

export function parseConnectParams(search: string | URLSearchParams): ConnectParams {
  const params = search instanceof URLSearchParams
    ? search
    : new URLSearchParams(search.startsWith("?") ? search.slice(1) : search);
  const width = Number(params.get("width"));
  const height = Number(params.get("height"));
  const mode = params.has("width") && params.has("height") && validResolution(width, height)
    ? { width, height }
    : {};
  const fps = boundedInteger(params.get("fps"), 0, 144);
  const quality = boundedInteger(params.get("quality"), 0, 10);
  return {
    machine: params.get("machine"),
    ...mode,
    ...(fps === undefined ? {} : { fps }),
    ...(quality === undefined ? {} : { quality }),
  };
}

export function buildConnectUrl(
  machine: string,
  native: Resolution | undefined,
  options: LaunchOptions = {},
): string {
  const params = new URLSearchParams({ machine });
  if (
    options.width !== undefined && options.height !== undefined
    && validResolution(options.width, options.height)
    && (options.width !== native?.width || options.height !== native?.height)
  ) {
    params.set("width", String(options.width));
    params.set("height", String(options.height));
  }
  if (options.fps !== undefined && Number.isInteger(options.fps) && options.fps >= 1 && options.fps <= 144) {
    params.set("fps", String(options.fps));
  }
  if (options.quality !== undefined && Number.isInteger(options.quality) && options.quality >= 1 && options.quality <= 10) {
    params.set("quality", String(options.quality));
  }
  return `/connect/?${params.toString()}`;
}
