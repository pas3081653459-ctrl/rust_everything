import { invoke } from '@tauri-apps/api/core';
import { register } from '@tauri-apps/plugin-global-shortcut';

export const QUICK_LAUNCH_SHORTCUT = 'Command+Space';
const LEGACY_QUICK_LAUNCH_SHORTCUT = 'Command+Shift+Space';

export async function initializeGlobalShortcuts(): Promise<void> {
  for (const shortcut of [QUICK_LAUNCH_SHORTCUT, LEGACY_QUICK_LAUNCH_SHORTCUT]) {
    try {
      await register(shortcut, (event) => {
        if (event.state === 'Released') {
          void invoke('toggle_main_window');
        }
      });
    } catch (error) {
      console.error(`Failed to register global shortcut ${shortcut}`, error);
    }
  }
}
