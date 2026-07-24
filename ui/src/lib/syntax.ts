import type { SyntaxTokenSpan } from './types';

export interface SafeSyntaxSegment {
  text: string;
  class?: SyntaxTokenSpan['class'];
}

/**
 * Converts UTF-8 byte spans supplied by Rust into safe JavaScript string
 * segments. Tree-sitter never supplies HTML; Svelte renders every segment as
 * a text node. Invalid/overlapping spans degrade to plain source instead of
 * slicing the middle of a Unicode code point.
 */
export function safeSyntaxSegments(
  text: string,
  sourceStartByte: number | undefined,
  spans: SyntaxTokenSpan[] | undefined
): SafeSyntaxSegment[] {
  if (!text || sourceStartByte === undefined || !spans?.length) return [{ text }];
  const bytesAtIndex: number[] = [0];
  let total = 0;
  for (let index = 0; index < text.length;) {
    const point = text.codePointAt(index);
    const width = point && point > 0xffff ? 2 : 1;
    total += new TextEncoder().encode(text.slice(index, index + width)).length;
    index += width;
    bytesAtIndex[index] = total;
  }
  const byteToIndex = new Map<number, number>();
  bytesAtIndex.forEach((byte, index) => byteToIndex.set(byte, index));
  const sourceEndByte = sourceStartByte + total;
  const relevant = spans
    .filter((span) => span.endByte > sourceStartByte && span.startByte < sourceEndByte && span.startByte < span.endByte)
    .map((span) => {
      const clippedStart = Math.max(span.startByte, sourceStartByte) - sourceStartByte;
      const clippedEnd = Math.min(span.endByte, sourceEndByte) - sourceStartByte;
      return { ...span, start: byteToIndex.get(clippedStart), end: byteToIndex.get(clippedEnd) };
    })
    .filter((span): span is SyntaxTokenSpan & { start: number; end: number } => span.start !== undefined && span.end !== undefined)
    .sort((a, b) => a.start - b.start || a.end - b.end);
  const output: SafeSyntaxSegment[] = [];
  let cursor = 0;
  for (const span of relevant) {
    if (span.start < cursor) continue;
    if (span.start > cursor) output.push({ text: text.slice(cursor, span.start) });
    output.push({ text: text.slice(span.start, span.end), class: span.class });
    cursor = span.end;
  }
  if (cursor < text.length) output.push({ text: text.slice(cursor) });
  return output.length ? output : [{ text }];
}
