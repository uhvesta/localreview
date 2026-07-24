import { describe, expect, it } from 'vitest';
import {
  isNavigableSymbol,
  renderedSymbolToken,
  sameSymbolTarget,
  symbolNavigationRequest,
  symbolWindowRequest
} from './symbolNavigation';

describe('symbol navigation coordinates', () => {
  it('accepts cross-language identifiers without accepting source fragments', () => {
    expect(['render', 'snake_case', '$store', '@decorator', 'predicate?', 'Module::Item']
      .every((symbol) => isNavigableSymbol(symbol))).toBe(true);
    expect(['', 'two words', '"quoted"', 'call()', '../path', 'x'.repeat(257)]
      .every((symbol) => !isNavigableSymbol(symbol))).toBe(true);
  });

  it('keeps one-based UTF-16 source columns and captured IDs in the open request', () => {
    const token = renderedSymbolToken('launch', 3)!;
    expect(token).toEqual({ symbol: 'launch', column: 4 });
    expect(symbolNavigationRequest({
      workspaceId: 'workspace-1',
      repositoryId: 'repo-1',
      fileId: 'file-1',
      comparisonId: 'comparison-1',
      filePath: 'src/main.rs'
    }, token, 'new', 42, 'references')).toEqual({
      workspaceId: 'workspace-1',
      repositoryId: 'repo-1',
      fileId: 'file-1',
      comparisonId: 'comparison-1',
      side: 'new',
      line: 42,
      column: 4,
      symbol: 'launch',
      initialQuery: 'references'
    });
  });

  it('validates native symbol-window bootstrap parameters and preserves the initial action', () => {
    const request = symbolWindowRequest('?view=symbol&workspaceId=workspace-1&repositoryId=repo-1&fileId=file-1&comparisonId=round-1&side=old&line=8&column=5&symbol=Widget&initialQuery=references');
    expect(request).toEqual({
      workspaceId: 'workspace-1',
      repositoryId: 'repo-1',
      fileId: 'file-1',
      comparisonId: 'round-1',
      side: 'old',
      line: 8,
      column: 5,
      symbol: 'Widget',
      initialQuery: 'references'
    });
    expect(symbolWindowRequest('?workspaceId=workspace-1&repositoryId=repo-1&fileId=file-1&side=new&line=0&column=5&symbol=Widget')).toBeUndefined();
    expect(symbolWindowRequest('?workspaceId=../../etc&repositoryId=repo-1&fileId=file-1&side=new&line=8&column=5&symbol=Widget')).toBeUndefined();
  });

  it('compares immutable captured targets independently of the requested query kind', () => {
    const target = {
      workspaceId: 'workspace-1', repositoryId: 'repo-1', fileId: 'file-1',
      comparisonId: 'comparison-1', side: 'new' as const, line: 9, column: 3, symbol: 'parse'
    };
    expect(sameSymbolTarget(target, { ...target })).toBe(true);
    expect(sameSymbolTarget(target, { ...target, line: 10 })).toBe(false);
  });
});
