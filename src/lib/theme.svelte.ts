import { getCurrentWindow } from '@tauri-apps/api/window'

export type Theme = 'system' | 'light' | 'dark'

const KEY = 'theme'
const ORDER: Theme[] = ['system', 'light', 'dark']

function stored(): Theme {
  try {
    const t = localStorage.getItem(KEY)
    return t === 'light' || t === 'dark' ? t : 'system'
  } catch {
    return 'system'
  }
}

export const theme = $state({ value: stored() })

/** Applies to the page and to the native title bar, which otherwise follows the OS. */
export function applyTheme(t: Theme) {
  theme.value = t
  if (t === 'system') delete document.documentElement.dataset.theme
  else document.documentElement.dataset.theme = t
  getCurrentWindow()
    .setTheme(t === 'system' ? null : t)
    .catch(() => {})
  try {
    localStorage.setItem(KEY, t)
  } catch {
    // Storage unavailable: the choice just won't survive a restart.
  }
}

export function cycleTheme() {
  applyTheme(ORDER[(ORDER.indexOf(theme.value) + 1) % ORDER.length])
}
