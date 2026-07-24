import type {
  DiffSide,
  SymbolNavigationKind,
  SymbolNavigationOpenRequest,
  SymbolNavigationTarget
} from './types';

export interface SymbolInteractionContext {
  workspaceId: string;
  repositoryId: string;
  fileId: string;
  comparisonId?: string;
  filePath: string;
}

export interface RenderedSymbolToken {
  symbol: string;
  column: number;
}

// This intentionally accepts common identifier conventions across languages
// without treating quoted strings, operators, or whole source fragments as
// symbols. Native language adapters remain authoritative.
const SYMBOL_PATTERN = /^(?:[#@~]?(?:[\p{L}_$][\p{L}\p{N}_$]*)(?:[!?']|::[\p{L}_$][\p{L}\p{N}_$]*)*)$/u;

export function isNavigableSymbol(value: string): boolean {
  return value.length > 0
    && value.length <= 256
    && value.trim() === value
    && SYMBOL_PATTERN.test(value);
}

export function renderedSymbolToken(value: string, zeroBasedColumn: number): RenderedSymbolToken | undefined {
  return isNavigableSymbol(value)
    ? { symbol: value, column: Math.max(0, zeroBasedColumn) + 1 }
    : undefined;
}

export function selectedSymbolToken(selection: Selection | null, code: HTMLElement): RenderedSymbolToken | undefined {
  if (!selection || selection.rangeCount !== 1 || selection.isCollapsed) return undefined;
  const value = selection.toString();
  if (!isNavigableSymbol(value)) return undefined;
  const range = selection.getRangeAt(0);
  if (!code.contains(range.commonAncestorContainer)) return undefined;
  const before = range.cloneRange();
  before.selectNodeContents(code);
  before.setEnd(range.startContainer, range.startOffset);
  return renderedSymbolToken(value, before.toString().length);
}

export function symbolNavigationRequest(
  context: SymbolInteractionContext,
  token: RenderedSymbolToken,
  side: DiffSide,
  line: number,
  initialQuery: Exclude<SymbolNavigationKind, 'all'>
): SymbolNavigationOpenRequest {
  return {
    workspaceId: context.workspaceId,
    repositoryId: context.repositoryId,
    fileId: context.fileId,
    comparisonId: context.comparisonId,
    side,
    line,
    column: token.column,
    symbol: token.symbol,
    initialQuery
  };
}

export function sameSymbolTarget(left: SymbolNavigationTarget, right: SymbolNavigationTarget): boolean {
  return left.workspaceId === right.workspaceId
    && left.fileId === right.fileId
    && left.comparisonId === right.comparisonId
    && left.side === right.side
    && left.line === right.line
    && left.column === right.column
    && left.symbol === right.symbol;
}

const SAFE_ID = /^[a-zA-Z0-9._:-]{1,160}$/;

export function symbolWindowRequest(search: string): SymbolNavigationOpenRequest | undefined {
  const parameters = new URLSearchParams(search);
  const workspaceId = parameters.get('workspaceId') ?? '';
  const repositoryId = parameters.get('repositoryId') ?? '';
  const fileId = parameters.get('fileId') ?? '';
  const comparisonId = parameters.get('comparisonId') || undefined;
  const side = parameters.get('side');
  const symbol = parameters.get('symbol') ?? '';
  const line = Number(parameters.get('line'));
  const column = Number(parameters.get('column'));
  const requestedKind = parameters.get('initialQuery');
  const initialQuery = requestedKind === 'references' ? 'references' : 'definitions';
  if (![workspaceId, repositoryId, fileId].every((value) => SAFE_ID.test(value))
    || (comparisonId !== undefined && !SAFE_ID.test(comparisonId))
    || (side !== 'old' && side !== 'new')
    || !Number.isSafeInteger(line) || line < 1
    || !Number.isSafeInteger(column) || column < 1
    || !isNavigableSymbol(symbol)) {
    return undefined;
  }
  return {
    workspaceId,
    repositoryId,
    fileId,
    comparisonId,
    side,
    line,
    column,
    symbol,
    initialQuery
  };
}
