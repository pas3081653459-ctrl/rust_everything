import { useCallback, useEffect, useRef, useState } from 'react';
import {
  checkFullDiskAccessPermission,
  requestFullDiskAccessPermission as requestNativeFullDiskAccessPermission,
} from 'tauri-plugin-macos-permissions-api';

export type FullDiskAccessStatus = 'unknown' | 'granted' | 'denied';

type UseFullDiskAccessPermissionResult = {
  status: FullDiskAccessStatus;
  isChecking: boolean;
  requestPermission: () => Promise<void>;
};

// Centralise macOS Full Disk Access state so App.tsx stays focused on UI concerns.
export function useFullDiskAccessPermission(): UseFullDiskAccessPermissionResult {
  const [status, setStatus] = useState<FullDiskAccessStatus>('unknown');
  const [isChecking, setIsChecking] = useState(true);
  const checkVersionRef = useRef(0);
  const hasLoggedPermissionStatusRef = useRef(false);

  const refreshStatus = useCallback(async () => {
    const version = ++checkVersionRef.current;
    setIsChecking(true);
    let timeout: ReturnType<typeof setTimeout> | undefined;
    try {
      const authorized = await Promise.race([
        checkFullDiskAccessPermission(),
        new Promise<never>((_, reject) => {
          timeout = setTimeout(() => reject(new Error('Permission check timed out')), 8000);
        }),
      ]);
      if (version !== checkVersionRef.current) return;
      if (!hasLoggedPermissionStatusRef.current) {
        console.log('Full Disk Access granted:', authorized);
        hasLoggedPermissionStatusRef.current = true;
      }
      setStatus(authorized ? 'granted' : 'denied');
    } catch (error) {
      console.error('Failed to check full disk access permission', error);
      if (version === checkVersionRef.current) setStatus('denied');
    } finally {
      if (timeout) clearTimeout(timeout);
      if (version === checkVersionRef.current) setIsChecking(false);
    }
  }, []);

  useEffect(() => {
    void refreshStatus();
    const onFocus = () => { void refreshStatus(); };
    window.addEventListener('focus', onFocus);
    return () => {
      checkVersionRef.current += 1;
      window.removeEventListener('focus', onFocus);
    };
  }, [refreshStatus]);

  const requestPermission = useCallback(async () => {
    try {
      await requestNativeFullDiskAccessPermission();
    } catch (error) {
      console.error('Failed to open Full Disk Access settings', error);
    } finally {
      await refreshStatus();
    }
  }, [refreshStatus]);

  return {
    status,
    isChecking,
    requestPermission,
  };
}
