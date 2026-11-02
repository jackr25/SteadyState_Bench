import { mount } from 'svelte'
import './app.css'
import App from './App.svelte'
import { psu } from './lib/psu.svelte'
import { applyTheme, theme } from './lib/theme.svelte'

applyTheme(theme.value)
psu.init()

// The webview's browser menu (Back, Reload, Inspect) doesn't belong in a release build;
// inputs keep theirs for copy/paste.
if (!import.meta.env.DEV) {
  document.addEventListener('contextmenu', (e) => {
    if (!(e.target instanceof HTMLInputElement)) e.preventDefault()
  })
}

const app = mount(App, {
  target: document.getElementById('app')!,
})

export default app
