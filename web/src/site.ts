import { RELAY_WS, wireCopyButtons } from './lib/relay';

const line = document.querySelector('#relay-line');
if (line) line.textContent = `relay ${RELAY_WS}`;
wireCopyButtons();
