import './app.css';
import { mount } from 'svelte';

const root = document.getElementById('app');
if (!root) throw new Error('LocalReview root was not found');

async function start() {
  if (new URLSearchParams(window.location.search).get('view') === 'symbol') {
    const { default: SymbolNavigationWindow } = await import('./SymbolNavigationWindow.svelte');
    mount(SymbolNavigationWindow, { target: root! });
    return;
  }
  const { default: App } = await import('./App.svelte');
  mount(App, { target: root! });
}

void start();
