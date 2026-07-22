export interface VirtualRange {
  start: number;
  end: number;
  offset: number;
  total: number;
}

/** Returns a small, stable window of rows for a fixed-height virtual list. */
export function getVirtualRange(
  count: number,
  scrollTop: number,
  viewportHeight: number,
  rowHeight: number,
  overscan = 10
): VirtualRange {
  // Clamp stale/overscrolled positions too: a file may shrink after an explicit refresh.
  const firstVisible = Math.min(count, Math.max(0, Math.floor(scrollTop / rowHeight)));
  const visible = Math.ceil(viewportHeight / rowHeight);
  const start = Math.max(0, firstVisible - overscan);
  const end = Math.min(count, firstVisible + visible + overscan);
  return { start, end, offset: start * rowHeight, total: count * rowHeight };
}
