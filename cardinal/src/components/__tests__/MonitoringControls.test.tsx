import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { invoke } from '@tauri-apps/api/core';
import { MonitoringControls } from '../MonitoringControls';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(async () => vi.fn()) }));
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(invoke).mockImplementation(async (command) => command === 'get_monitoring_status' ? {
    enabled: false, refreshing: false, needsInitialScan: false,
    needsRefresh: true, error: null, revision: 1,
  } : undefined);
});

describe('MonitoringControls', () => {
  it('loads status without starting a watcher or refreshing the index', async () => {
    render(<MonitoringControls disabled={false} />);
    await waitFor(() => expect(screen.getByRole('switch')).toBeEnabled());
    expect(screen.getByRole('switch')).not.toBeChecked();
    expect(invoke).not.toHaveBeenCalledWith('trigger_rescan');
    expect(invoke).not.toHaveBeenCalledWith('set_event_monitoring', expect.anything());
    fireEvent.click(screen.getByText('monitoring.refresh'));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith('trigger_rescan', undefined));
  });

  it('only enables background monitoring in response to the switch', async () => {
    render(<MonitoringControls disabled={false} />);
    await waitFor(() => expect(screen.getByRole('switch')).toBeEnabled());
    fireEvent.click(screen.getByRole('switch'));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith('set_event_monitoring', { enabled: true }));
  });
});
