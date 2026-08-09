export type VideoGeometry = {
  boxWidth: number;
  boxHeight: number;
  renderWidth: number;
  renderHeight: number;
  offsetX: number;
  offsetY: number;
};

export function getContainedVideoGeometry(
  boxWidth: number,
  boxHeight: number,
  videoWidth: number,
  videoHeight: number,
): VideoGeometry | null {
  if (boxWidth <= 0 || boxHeight <= 0 || videoWidth <= 0 || videoHeight <= 0) {
    return null;
  }
  const videoRatio = videoWidth / videoHeight;
  const boxRatio = boxWidth / boxHeight;
  if (boxRatio > videoRatio) {
    const renderHeight = boxHeight;
    const renderWidth = renderHeight * videoRatio;
    return { boxWidth, boxHeight, renderWidth, renderHeight, offsetX: (boxWidth - renderWidth) / 2, offsetY: 0 };
  }
  const renderWidth = boxWidth;
  const renderHeight = renderWidth / videoRatio;
  return { boxWidth, boxHeight, renderWidth, renderHeight, offsetX: 0, offsetY: (boxHeight - renderHeight) / 2 };
}

export function mapCursorToVideo(
  position: [number, number],
  geometry: VideoGeometry,
  videoWidth: number,
  videoHeight: number,
  bitmapWidth: number,
  bitmapHeight: number,
) {
  return {
    left: geometry.offsetX + position[0] * geometry.renderWidth / videoWidth,
    top: geometry.offsetY + position[1] * geometry.renderHeight / videoHeight,
    width: bitmapWidth * geometry.renderWidth / videoWidth,
    height: bitmapHeight * geometry.renderHeight / videoHeight,
  };
}
