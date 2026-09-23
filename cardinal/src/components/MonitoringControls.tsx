import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { useTranslation } from 'react-i18next';

type MonitoringStatus = {
  enabled: boolean;
  refreshing: boolean;
  needsInitialScan: boolean;
  needsRefresh: boolean;
  error: string | null;
  revision: number;
};

export function MonitoringControls({
  disabled,
  onIndexUpdated,
}: { disabled: boolean; onIndexUpdated?: () => void }) {
  const { t } = useTranslation();
  const [status, setStatus] = useState<MonitoringStatus | null>(null);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const callbackRef = useRef(onIndexUpdated);
  callbackRef.current = onIndexUpdated;

  useEffect(() => {
    let disposed = false;
    const cleanup: Array<() => void> = [];
    const accept = (next: MonitoringStatus) => {
      if (!disposed) setStatus((previous) =>
        !previous || next.revision >= previous.revision ? next : previous);
    };
    const subscribe = async () => {
      const stopStatus = await listen<MonitoringStatus>('monitoring_status', ({ payload }) => accept(payload));
      if (disposed) { stopStatus(); return; }
      cleanup.push(stopStatus);
      const stopUpdated = await listen('index_refreshed', () => {
        if (!disposed) callbackRef.current?.();
      });
      if (disposed) { stopUpdated(); return; }
      cleanup.push(stopUpdated);
      accept(await invoke<MonitoringStatus>('get_monitoring_status'));
    };
    void subscribe().catch((error) => { if (!disposed) setError(String(error)); });
    return () => { disposed = true; cleanup.forEach((stop) => stop()); };
  }, []);

  const request = async (command: string, args?: Record<string, unknown>) => {
    setPending(true);
    setError(null);
    try { await invoke(command, args); }
    catch (error) { setError(String(error)); }
    finally { setPending(false); }
  };
  const message = status?.refreshing ? t('monitoring.refreshing')
    : status?.needsInitialScan ? t('monitoring.needsInitialScan')
      : status?.enabled ? t('monitoring.live') : t('monitoring.cached');
  const failure = error || status?.error;

  return <div className="monitoring-controls">
    <button type="button" disabled={disabled || pending || !status || status.refreshing}
      onClick={() => void request('trigger_rescan')} title={t('monitoring.refreshHint')}>
      {t('monitoring.refresh')}
    </button>
    <label title={t('monitoring.switchHint')}>
      <input type="checkbox" role="switch" checked={status?.enabled ?? false}
        disabled={pending || !status || (disabled && !status.enabled)}
        onChange={(event) => void request('set_event_monitoring', { enabled: event.target.checked })} />
      {t('monitoring.switch')}
    </label>
    {status?.refreshing && <button type="button" disabled={pending}
      onClick={() => void request('set_event_monitoring', { enabled: false })}>
      {t('monitoring.stop')}
    </button>}
    <span className="monitoring-message" role={failure ? 'alert' : 'status'}
      title={failure || t('monitoring.refreshHint')}>
      {failure ? t('monitoring.failed') : message}
    </span>
  </div>;
}
