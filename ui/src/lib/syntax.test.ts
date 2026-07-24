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
    expect(segments).toEqual([{ text: source, start: 0, end: source.length }]);
  });

  it('clips multi-line tokens to the visible source row', () => {
    const source = '//! second line';
    const segments = safeSyntaxSegments(source, 15, [
      { startByte: 0, endByte: 30, class: 'comment' }
    ]);
    expect(segments).toEqual([{ text: source, class: 'comment', start: 0, end: source.length }]);
  });

  it('finds a visible token without scanning earlier rows and preserves astral UTF-8 boundaries', () => {
    const earlier = Array.from({ length: 10_000 }, (_, index) => ({
      startByte: index * 2,
      endByte: index * 2 + 1,
      class: 'variable' as const
    }));
    const source = '🚀 launch';
    const startByte = 25_000;
    const segments = safeSyntaxSegments(source, startByte, [
      ...earlier,
      { startByte, endByte: startByte + 4, class: 'string' }
    ]);
    expect(segments).toEqual([
      { text: '🚀', class: 'string', start: 0, end: 2 },
      { text: ' launch', start: 2, end: source.length }
    ]);
  });

  it('preserves every native semantic class as escaped renderer data', () => {
    const classes = [
      'attribute', 'boolean', 'comment', 'constant', 'constructor', 'embedded',
      'escape', 'function', 'keyword', 'markup', 'module', 'number', 'operator',
      'property', 'punctuation', 'string', 'tag', 'type', 'variable'
    ] as const;
    const source = 'x'.repeat(classes.length);
    const segments = safeSyntaxSegments(
      source,
      0,
      classes.map((className, index) => ({ startByte: index, endByte: index + 1, class: className }))
    );
    expect(segments.map((segment) => segment.text).join('')).toBe(source);
    expect(segments.map((segment) => segment.class)).toEqual(classes);
  });
});
