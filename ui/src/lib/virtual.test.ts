import { describe, expect, it } from 'vitest';
import { getVirtualRange } from './virtual';

describe('getVirtualRange', () => {
  it('windows rows around a viewport rather than rendering the whole file', () => {
    const range = getVirtualRange(50_000, 12_000, 720, 24, 12);
    expect(range.total).toBe(1_200_000);
    expect(range.start).toBe(488);
    expect(range.end).toBeLessThan(550);
    expect(range.end - range.start).toBeLessThan(70);
  });

  it('clamps a range near either end of a document', () => {
    expect(getVirtualRange(5, 0, 200, 24, 10)).toMatchObject({ start: 0, end: 5, offset: 0 });
    expect(getVirtualRange(100, 10_000, 100, 20, 2)).toMatchObject({ start: 98, end: 100 });
  });
});
