// Listen for settings changes, the re-compute theme
import { listen } from '@tauri-apps/api/event';
import type { ModelPayload } from './components/GlobalHooks';
import { getSettings } from './lib/store';

function setFontSizeOnDocument(fontSize: number) {
  document.documentElement.style.fontSize = `${fontSize}px`;
}

listen<ModelPayload>('upserted_model', async (event) => {
  if (event.payload.model.model !== 'settings') return;
  setFontSizeOnDocument(event.payload.model.interfaceFontSize);
}).catch(console.error);

getSettings().then((settings) => setFontSizeOnDocument(settings.interfaceFontSize));
