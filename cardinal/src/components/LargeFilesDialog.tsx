import { CopyPathButton } from './CopyPathButton';
import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useTranslation } from 'react-i18next';
import { formatFileSize } from '../utils/format';
import { splitPath } from '../utils/path';
import './LargeFilesDialog.css';

type Progress = {
  phase: 'scanning' | 'sorting' | 'ready' | 'cancelled' | 'error';
  checked: number;
  total: number;
  matched: number;
  skipped: number;
  error: string | null;
};
type Row = { path: string; size: number; icon?: string | null };
const PAGE_SIZE = 100;
let latestRequestId = Date.now();

export function LargeFilesDialog({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation();
  const dialogRef = useRef<HTMLDialogElement>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const cancelledIdRef = useRef<number | null>(null);
  const [refresh, setRefresh] = useState(0);
  const [id, setId] = useState<number | null>(null);
  const [progress, setProgress] = useState<Progress | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [page, setPage] = useState(0);
  const [ascending, setAscending] = useState(false);
  const [rows, setRows] = useState<Row[]>([]);
  const [loadingPage, setLoadingPage] = useState(false);

  useEffect(() => {
    const dialog = dialogRef.current;
    dialog?.showModal();
    return () => dialog?.close();
  }, []);

  useEffect(() => {
    let disposed = false;
    let jobId: number | null = null;
    let timer: ReturnType<typeof setTimeout> | undefined;
    setId(null);
    setProgress(null);
    setRows([]);
    setPage(0);
    setError(null);
    const cancel = (id: number) => {
      void invoke('cancel_large_file_search', { id }).catch(console.error);
    };
    const poll = async () => {
      try {
        const next = await invoke<Progress>('get_large_file_progress', { id: jobId });
        if (disposed || cancelledIdRef.current === jobId) return;
        setProgress(next);
        if (next.phase === 'scanning' || next.phase === 'sorting') {
          timer = setTimeout(() => void poll(), 300);
        }
      } catch (error) {
        if (!disposed) setError(String(error));
      }
    };
    latestRequestId = Math.max(latestRequestId + 1, Date.now());
    void invoke<number>('start_large_file_search', { requestId: latestRequestId }).then((value) => {
      jobId = value;
      if (disposed) { cancel(value); return; }
      setId(value);
      void poll();
    }).catch((error) => {
      if (!disposed) setError(String(error));
    });
    return () => {
      disposed = true;
      clearTimeout(timer);
      if (jobId !== null) cancel(jobId);
    };
  }, [refresh]);

  useEffect(() => {
    if (id === null || progress?.phase !== 'ready') return;
    let disposed = false;
    setLoadingPage(true);
    setRows([]);
    void invoke<Row[]>('get_large_file_page', {
      id, offset: page * PAGE_SIZE, ascending,
    }).then((rows) => {
      if (!disposed) {
        setRows(rows);
        scrollRef.current?.scrollTo(0, 0);
      }
    }).catch((error) => {
      if (!disposed) setError(String(error));
    }).finally(() => {
      if (!disposed) setLoadingPage(false);
    });
    return () => { disposed = true; };
  }, [id, progress?.phase, page, ascending]);

  const running = !error && (!progress || progress.phase === 'scanning' || progress.phase === 'sorting');
  const pages = Math.max(1, Math.ceil((progress?.matched ?? 0) / PAGE_SIZE));
  const fileAction = (command: 'open_path' | 'open_in_finder', path: string) => {
    void invoke(command, { path }).catch((error) => setError(String(error)));
  };

  return (
    <dialog ref={dialogRef} className="large-files-dialog" aria-labelledby="large-files-title"
      onCancel={(event) => { event.preventDefault(); onClose(); }}
      onKeyDown={(event) => event.stopPropagation()}>
      <header>
        <h2 id="large-files-title">{t('largeFiles.title')}</h2>
        <button type="button" onClick={onClose}>{t('largeFiles.close')}</button>
      </header>
      <p className="large-files-description">{t('largeFiles.description')}</p>
      <div className="large-files-toolbar">
        <button type="button" disabled={running} onClick={() => setRefresh((value) => value + 1)}>
          {t('largeFiles.refresh')}
        </button>
        {running && id !== null && <button type="button" onClick={() => {
          cancelledIdRef.current = id;
          setProgress((previous) => ({
            checked: 0, total: 0, matched: 0, skipped: 0, error: null,
            ...previous, phase: 'cancelled',
          }));
          void invoke('cancel_large_file_search', { id }).catch((error) => setError(String(error)));
        }}>{t('largeFiles.cancel')}</button>}
        <span role="status">
          {t(`largeFiles.${progress?.phase ?? 'scanning'}`)}
          {progress && ` · ${t('largeFiles.progress', {
            checked: progress.checked.toLocaleString(), total: progress.total.toLocaleString(),
            matched: progress.matched.toLocaleString(), skipped: progress.skipped.toLocaleString(),
          })}`}
        </span>
      </div>
      {running && <progress max={progress?.total || 1} value={
        progress?.phase === 'scanning' && progress.total > 0 ? progress.checked : undefined
      } aria-label={t('largeFiles.scanning')} />}
      {(error || progress?.error) && <p role="alert">{error || progress?.error}</p>}
      {progress?.phase === 'ready' && <>
        <div className="large-files-table" ref={scrollRef} aria-busy={loadingPage}>
          <table>
            <colgroup>
              <col className="large-files-name-column" />
              <col />
              <col className="large-files-size-column" />
              <col className="large-files-actions-column" />
            </colgroup>
            <thead><tr>
              <th>{t('columns.filename')}</th>
              <th>{t('largeFiles.path')}</th>
              <th aria-sort={ascending ? 'ascending' : 'descending'}>
                <button type="button" onClick={() => { setAscending((value) => !value); setPage(0); }}>
                  {t('largeFiles.size')} {ascending ? '↑' : '↓'}
                </button>
              </th>
              <th>{t('largeFiles.actions')}</th>
            </tr></thead>
            <tbody>{rows.map((row, index) => {
              const { name, directory } = splitPath(row.path);
              return (
                <tr key={`${page}-${index}`} onDoubleClick={() => fileAction('open_path', row.path)}>
                  <td title={name}>
                    <div className="large-files-name">
                      {row.icon ? <img src={row.icon} alt="" className="file-icon" /> : (
                        <svg className="file-icon" viewBox="0 0 20 20" fill="none" aria-hidden="true">
                          <path d="M4 2h7l5 5v11H4zM11 2v5h5" stroke="currentColor" strokeWidth="1.2" />
                        </svg>
                      )}
                      <span>{name}</span>
                    </div>
                  </td>
                  <td className="large-files-directory" title={row.path}>{directory}</td>
                  <td className="large-files-size" title={`${row.size.toLocaleString()} B`}>
                    {formatFileSize(row.size)}
                  </td>
                  <td className="large-files-actions" onDoubleClick={(event) => event.stopPropagation()}>
                    <CopyPathButton path={row.path} />
                    <button type="button" title={t('largeFiles.open')} aria-label={t('largeFiles.open')}
                      onClick={() => fileAction('open_path', row.path)}>
                      <svg viewBox="0 0 20 20" fill="none" aria-hidden="true">
                        <path d="M11 3h6v6M17 3l-9 9M8 4H3v13h13v-5" stroke="currentColor" strokeWidth="1.5" />
                      </svg>
                    </button>
                    <button type="button" title={t('largeFiles.reveal')} aria-label={t('largeFiles.reveal')}
                      onClick={() => fileAction('open_in_finder', row.path)}>
                      <svg viewBox="0 0 20 20" fill="none" aria-hidden="true">
                        <path d="M2 5h6l2 2h8v10H2z" stroke="currentColor" strokeWidth="1.5" />
                      </svg>
                    </button>
                  </td>
                </tr>
              );
            })}</tbody>
          </table>
          {!loadingPage && progress.matched === 0 && <p>{t('largeFiles.empty')}</p>}
        </div>
        <footer>
          <button type="button" disabled={page === 0 || loadingPage} onClick={() => setPage(0)}>{t('largeFiles.first')}</button>
          <button type="button" disabled={page === 0 || loadingPage} onClick={() => setPage((value) => value - 1)}>{t('largeFiles.previous')}</button>
          <span>{t('largeFiles.page', { page: page + 1, pages })}</span>
          <button type="button" disabled={page + 1 >= pages || loadingPage} onClick={() => setPage((value) => value + 1)}>{t('largeFiles.next')}</button>
          <button type="button" disabled={page + 1 >= pages || loadingPage} onClick={() => setPage(pages - 1)}>{t('largeFiles.last')}</button>
        </footer>
      </>}
    </dialog>
  );
}
