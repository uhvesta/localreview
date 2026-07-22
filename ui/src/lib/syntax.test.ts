import { describe, expect, it } from 'vitest';
import { safeSyntaxSegments } from './syntax';

describe('safeSyntaxSegments', () => {
  it('keeps source as text segments instead of accepting HTML', () => {
    const source = '<img src=x onerror=alert(1)> const café = "safe"';
    const bytes = new TextEncoder();
    const start = bytes.encode(source.slice(0, source.indexOf('const'))).length;
    const end = start + bytes.encode('const').length;
    const segments = safeSyntaxSegments(source, 0, [{ startByte: start, endByte: end, class: 'keyword' }]);
    expect(segments.map((segment) => segment.text).join('')).toBe(source);
    expect(segments.find((segment) => segment.class === 'keyword')?.text).toBe('const');
    expect(segments.some((segment) => segment.text.includes('<img'))).toBe(true);
  });

  it('drops spans that split a UTF-8 code point', () => {
    const source = 'café';
    const segments = safeSyntaxSegments(source, 0, [{ startByte: 3, endByte: 4, class: 'string' }]);
    expect(segments).toEqual([{ text: source }]);
  });
});
