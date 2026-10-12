import { createApp } from 'vue';
import App from './App.vue';
import './styles.css';

createApp(App).mount('#root');

// Handy for debugging from the devtools console.
import { engine } from './engine';
import { ui } from './state';
import * as exporter from './exporter';
import { rows } from './rows';
(window as any).__archive = { engine, ui, exporter, rows };
