import { act, renderHook, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { invoke } from '@tauri-apps/api/core';
import type { SlabIndex } from '../../types/slab';
import { DIRECTORY_SCOPE_OPEN_STORAGE_KEY, useFileSearch } from '../useFileSearch';
import { SearchStatusCode } from '../../types/ipc';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

const mockedInvoke = vi.mocked(invoke);

const searchResponse = (results: SlabIndex[] = []) => ({
  results,
  highlights: [],
  total: results.length,
  page: 0,
  pageSize: 100,
  version: 1,
  root: null,
  statusCode: SearchStatusCode.OK,
});

const mockSearchSuccess = (results: SlabIndex[] = []) => {
  mockedInvoke.mockImplementation((command: string) => {
    if (command === 'get_app_status') {
      return Promise.resolve('Ready');
    }
    if (command === 'search_first_page') {
      return Promise.resolve(searchResponse(results));
    }
    return Promise.resolve(null);
  });
};

const mockSearchCancelled = () => {
  mockedInvoke.mockImplementation((command: string) => {
    if (command === 'get_app_status') {
      return Promise.resolve('Ready');
    }
    if (command === 'search_first_page') {
      return Promise.resolve({
        results: [],
        highlights: [],
        statusCode: SearchStatusCode.CANCELLED,
      });
    }
    return Promise.resolve(null);
  });
};

const renderReadySearchHook = async () => {
  const rendered = renderHook(() => useFileSearch());
  await waitFor(() => expect(rendered.result.current.state.initialFetchCompleted).toBe(true));
  return rendered;
};

describe('useFileSearch', () => {
  beforeEach(() => {
    window.localStorage.setItem(DIRECTORY_SCOPE_OPEN_STORAGE_KEY, 'false');
  });

  afterEach(() => {
    vi.clearAllMocks();
  });

  it('reuses backend results array without copying', async () => {
    const backendResults = [1, 2, 3] as SlabIndex[];
    mockSearchSuccess(backendResults);
    const { result } = await renderReadySearchHook();

    expect(result.current.state.results).toBe(backendResults);
    expect(result.current.state.resultCount).toBe(backendResults.length);
  });

  it('ignores results when backend returns CANCELLED status', async () => {
    const initialResults = [1, 2, 3] as SlabIndex[];
    mockSearchSuccess(initialResults);
    const { result } = await renderReadySearchHook();
    expect(result.current.state.results).toBe(initialResults);

    mockSearchCancelled();

    act(() => {
      result.current.queueSearch('new query', { immediate: true });
    });

    // Cancelled results should not overwrite state, and loading should settle.
    await waitFor(() => {
      expect(result.current.state.results).toBe(initialResults);
      expect(result.current.state.currentQuery).toBe(''); // Query doesn't update on cancelled search
      expect(result.current.state.showLoadingUI).toBe(false);
      expect(result.current.state.initialFetchCompleted).toBe(true);
    });
  });

  it('does not send directory scope while the scope input is inactive', async () => {
    mockSearchSuccess();
    const { result } = await renderReadySearchHook();
    mockedInvoke.mockClear();

    act(() => {
      result.current.queueDirectorySearch('Projects', { immediate: true });
    });

    await waitFor(() => {
      expect(mockedInvoke).toHaveBeenCalledWith('search_first_page', {
        query: null,
        directoryQuery: null,
        options: {
          caseInsensitive: true,
        },
      });
    });
  });

  it('re-runs search when directory scope is toggled and controls the directory payload', async () => {
    mockSearchSuccess();
    const { result } = await renderReadySearchHook();

    act(() => {
      result.current.queueDirectorySearch('Projects', { immediate: true });
    });
    await waitFor(() => {
      expect(mockedInvoke).toHaveBeenLastCalledWith('search_first_page', {
        query: null,
        directoryQuery: null,
        options: {
          caseInsensitive: true,
        },
      });
    });

    mockedInvoke.mockClear();
    act(() => {
      result.current.queueDirectoryScopeOpen(true);
    });
    await waitFor(() => {
      expect(mockedInvoke).toHaveBeenLastCalledWith('search_first_page', {
        query: null,
        directoryQuery: 'Projects',
        options: {
          caseInsensitive: true,
        },
      });
      expect(result.current.state.currentDirectoryQuery).toBe('Projects');
      expect(window.localStorage.getItem(DIRECTORY_SCOPE_OPEN_STORAGE_KEY)).toBe('true');
    });

    mockedInvoke.mockClear();
    act(() => {
      result.current.queueDirectoryScopeOpen(false);
    });
    await waitFor(() => {
      expect(mockedInvoke).toHaveBeenLastCalledWith('search_first_page', {
        query: null,
        directoryQuery: null,
        options: {
          caseInsensitive: true,
        },
      });
      expect(result.current.state.currentDirectoryQuery).toBe('');
      expect(window.localStorage.getItem(DIRECTORY_SCOPE_OPEN_STORAGE_KEY)).toBe('false');
    });
  });

  it('hydrates persisted directory scope open state', async () => {
    window.localStorage.setItem(DIRECTORY_SCOPE_OPEN_STORAGE_KEY, 'true');
    mockSearchSuccess();
    const { result } = await renderReadySearchHook();

    expect(result.current.searchParams.directoryScopeOpen).toBe(true);

    act(() => {
      result.current.queueDirectorySearch('Projects', { immediate: true });
    });

    await waitFor(() => {
      expect(mockedInvoke).toHaveBeenLastCalledWith('search_first_page', {
        query: null,
        directoryQuery: 'Projects',
        options: {
          caseInsensitive: true,
        },
      });
    });
  });

  it('passes whitespace directory scope through when the scope is active', async () => {
    mockSearchSuccess();
    const { result } = await renderReadySearchHook();

    act(() => {
      result.current.queueDirectorySearch('   ', { immediate: true });
    });
    act(() => {
      result.current.queueDirectoryScopeOpen(true);
    });

    await waitFor(() => {
      expect(mockedInvoke).toHaveBeenLastCalledWith('search_first_page', {
        query: null,
        directoryQuery: '   ',
        options: {
          caseInsensitive: true,
        },
      });
      expect(result.current.state.currentDirectoryQuery).toBe('   ');
    });
  });

  it('passes whitespace query through to search', async () => {
    mockSearchSuccess();
    const { result } = await renderReadySearchHook();
    mockedInvoke.mockClear();

    act(() => {
      result.current.queueSearch('   ', { immediate: true });
    });

    await waitFor(() => {
      expect(mockedInvoke).toHaveBeenLastCalledWith('search_first_page', {
        query: '   ',
        directoryQuery: null,
        options: {
          caseInsensitive: true,
        },
      });
      expect(result.current.state.currentQuery).toBe('   ');
    });
  });
});


describe('paged file search', () => {
  it('does not request a page while the worker is waiting for permission', async () => {
    mockedInvoke.mockClear();
    mockedInvoke.mockImplementation((command: string) => {
      if (command === 'get_app_status') return Promise.resolve('Initializing');
      return Promise.resolve(searchResponse());
    });
    const { result } = renderHook(() => useFileSearch());
    await act(async () => { await Promise.resolve(); });
    act(() => result.current.queueSearch('waiting', { immediate: true }));
    expect(mockedInvoke.mock.calls.some(([command]) => command === 'search_first_page')).toBe(false);
    act(() => result.current.setLifecycleState('Ready'));
    await waitFor(() => expect(mockedInvoke).toHaveBeenCalledWith('search_first_page', {
      query: 'waiting', directoryQuery: null, options: { caseInsensitive: true },
    }));
  });

  it('keeps the total separate from the current page and requests pages by version', async () => {
    mockedInvoke.mockImplementation((command: string) => {
      if (command === 'get_app_status') return Promise.resolve('Ready');
      if (command === 'search_first_page') return Promise.resolve({ ...searchResponse([1] as SlabIndex[]), total: 4000000, version: 42 });
      if (command === 'get_result_page') return Promise.resolve({ ...searchResponse([2] as SlabIndex[]), total: 4000000, version: 42, page: 9 });
      return Promise.resolve(null);
    });
    const { result } = await renderReadySearchHook();
    expect(result.current.state.resultCount).toBe(4000000);
    expect(result.current.state.results).toHaveLength(1);
    act(() => result.current.goToPage(9));
    await waitFor(() => expect(result.current.state.page).toBe(9));
    expect(mockedInvoke).toHaveBeenCalledWith('get_result_page', { version: 42, page: 9 });
    expect(result.current.state.results).toEqual([2]);
  });
});
