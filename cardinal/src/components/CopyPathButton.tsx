import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

export function CopyPathButton({ path }: { path: string }) {
  const { t } = useTranslation();
  const [status, setStatus] = useState<'idle' | 'copied' | 'failed'>('idle');
  const [pending, setPending] = useState(false);
  const epoch = useRef(0);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    epoch.current += 1;
    setStatus('idle');
    setPending(false);
    return () => {
      epoch.current += 1;
      if (timer.current) clearTimeout(timer.current);
    };
  }, [path]);
  const label = t(`copyPath.${status}`);
  return <button type="button" className="copy-path-button" disabled={pending || !path}
    title={`${label}: ${path}`} aria-label={`${label}: ${path}`} draggable={false}
    onMouseDown={(event) => event.stopPropagation()}
    onMouseUp={(event) => event.stopPropagation()}
    onKeyDown={(event) => {
      // Keep keyboard activation on the button from opening Quick Look.
      if (event.key === ' ' || event.key === 'Enter') event.stopPropagation();
    }}
    onKeyUp={(event) => {
      if (event.key === ' ' || event.key === 'Enter') event.stopPropagation();
    }}
    onDoubleClick={(event) => event.stopPropagation()}
    onDragStart={(event) => { event.preventDefault(); event.stopPropagation(); }}
    onClick={async (event) => {
      event.stopPropagation();
      const version = epoch.current;
      setPending(true);
      if (timer.current) clearTimeout(timer.current);
      try {
        await navigator.clipboard.writeText(path);
        if (epoch.current === version) setStatus('copied');
      } catch {
        if (epoch.current === version) setStatus('failed');
      } finally {
        if (epoch.current === version) {
          setPending(false);
          timer.current = setTimeout(() => setStatus('idle'), 2000);
        }
      }
    }}>
    <svg viewBox="0 0 20 20" fill="none" aria-hidden="true">
      {status === 'copied' ? <path d="m4 10 4 4 8-9" stroke="currentColor" strokeWidth="1.5" />
        : status === 'failed' ? <path d="M10 3v9m0 3v2" stroke="currentColor" strokeWidth="2" />
          : <path d="M7 7h10v10H7zM13 7V3H3v10h4" stroke="currentColor" strokeWidth="1.5" />}
    </svg>
    <span className="copy-path-feedback" role="status">{status !== 'idle' ? label : ''}</span>
  </button>;
}
