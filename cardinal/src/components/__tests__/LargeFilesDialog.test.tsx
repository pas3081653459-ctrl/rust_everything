import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { invoke } from '@tauri-apps/api/core';
import { LargeFilesDialog } from '../LargeFilesDialog';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
const mockedInvoke = vi.mocked(invoke);

beforeEach(() => {
  vi.clearAllMocks();
  HTMLDialogElement.prototype.showModal = vi.fn(function (this: HTMLDialogElement) {
    this.setAttribute('open', '');
  });
  HTMLDialogElement.prototype.close = vi.fn(function (this: HTMLDialogElement) {
    this.removeAttribute('open');
  });
  HTMLElement.prototype.scrollTo = vi.fn();
});

describe('LargeFilesDialog', () => {
  it('pages all matches beyond the ordinary sorting limit and switches direction', async () => {
    mockedInvoke.mockImplementation(async (command, args) => {
      if (command === 'start_large_file_search') return (args as { requestId: number }).requestId;
      if (command === 'get_large_file_progress') return {
        phase: 'ready', checked: 4000000, total: 4000000, matched: 20001, skipped: 0, error: null,
      };
      if (command === 'get_large_file_page') return [{ path: '/example/large.bin', size: 2000000 }];
      return undefined;
    });
    const { unmount } = render(<LargeFilesDialog onClose={vi.fn()} />);
    await screen.findByText('large.bin');
    expect(screen.getByText('/example')).toBeInTheDocument();
    expect(screen.getByText('1.91 MB')).toBeInTheDocument();
    fireEvent.click(screen.getByText('largeFiles.last'));
    await waitFor(() => expect(mockedInvoke).toHaveBeenCalledWith('get_large_file_page', {
      id: expect.any(Number), offset: 20000, ascending: false,
    }));
    fireEvent.click(screen.getByRole('button', { name: /largeFiles.size/ }));
    await waitFor(() => expect(mockedInvoke).toHaveBeenCalledWith('get_large_file_page', {
      id: expect.any(Number), offset: 0, ascending: true,
    }));
    unmount();
    expect(mockedInvoke).toHaveBeenCalledWith('cancel_large_file_search', { id: expect.any(Number) });
  });

  it('cancels a task whose start response arrives after the view closes', async () => {
    let resolveStart: (id: number) => void = () => {};
    mockedInvoke.mockImplementation((command) => {
      if (command === 'start_large_file_search') return new Promise((resolve) => { resolveStart = resolve; });
      return Promise.resolve(undefined);
    });
    const { unmount } = render(<LargeFilesDialog onClose={vi.fn()} />);
    unmount();
    await act(async () => { resolveStart(123); });
    expect(mockedInvoke).toHaveBeenCalledWith('cancel_large_file_search', { id: 123 });
    expect(mockedInvoke).not.toHaveBeenCalledWith('get_large_file_progress', expect.anything());
  });
});
