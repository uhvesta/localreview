import type { SyntaxTokenSpan } from './types';

export interface SafeSyntaxSegment {
  text: string;
  class?: SyntaxTokenSpan['class'];
  /** UTF-16 offsets within the rendered source row. These remain DOM-safe
   * presentation coordinates; native symbol queries receive one-based source
   * line and column separately. */
  start: number;
  end: number;
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
  if (!text || sourceStartByte === undefined || !spans?.length) return [{ text, start: 0, end: text.length }];
  const bytesAtIndex: number[] = [0];
  let total = 0;
  for (let index = 0; index < text.length;) {
    const point = text.codePointAt(index) ?? 0;
    const width = point > 0xffff ? 2 : 1;
    total += point <= 0x7f ? 1 : point <= 0x7ff ? 2 : point <= 0xffff ? 3 : 4;
    index += width;
    bytesAtIndex[index] = total;
  }
  const byteToIndex = new Map<number, number>();
  bytesAtIndex.forEach((byte, index) => byteToIndex.set(byte, index));
  const sourceEndByte = sourceStartByte + total;
  // Native token spans are emitted in source order. Locate the first
  // overlapping span in O(log n), then inspect only this row's tokens instead
  // of filtering the complete file token array once per visible line.
  let low = 0;
  let high = spans.length;
  while (low < high) {
    const middle = Math.floor((low + high) / 2);
    if (spans[middle].endByte <= sourceStartByte) low = middle + 1;
    else high = middle;
  }
  const relevant: Array<SyntaxTokenSpan & { start: number; end: number }> = [];
  for (let index = low; index < spans.length; index += 1) {
    const span = spans[index];
    if (span.startByte >= sourceEndByte) break;
    if (span.startByte >= span.endByte) continue;
    const clippedStart = Math.max(span.startByte, sourceStartByte) - sourceStartByte;
    const clippedEnd = Math.min(span.endByte, sourceEndByte) - sourceStartByte;
    const start = byteToIndex.get(clippedStart);
    const end = byteToIndex.get(clippedEnd);
    if (start !== undefined && end !== undefined) relevant.push({ ...span, start, end });
  }
  const output: SafeSyntaxSegment[] = [];
  let cursor = 0;
  for (const span of relevant) {
    if (span.start < cursor) continue;
    if (span.start > cursor) output.push({ text: text.slice(cursor, span.start), start: cursor, end: span.start });
    output.push({ text: text.slice(span.start, span.end), class: span.class, start: span.start, end: span.end });
    cursor = span.end;
  }
  if (cursor < text.length) output.push({ text: text.slice(cursor), start: cursor, end: text.length });
  return output.length ? output : [{ text, start: 0, end: text.length }];
}
