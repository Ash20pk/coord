import { RELAY_WS, wireCopyButtons } from './lib/relay';

const line = document.querySelector('#relay-line');
if (line) line.textContent = `relay ${RELAY_WS}`;

// The enrolment commands name whichever relay served this page.
const init = document.querySelector('#c-init');
const login = document.querySelector('#c-login');
if (init) init.textContent = `knoot init --relay ${RELAY_WS}`;
if (login) login.textContent = `knoot login --relay ${RELAY_WS} --token <token>`;

wireCopyButtons();

// Highlight the section currently on screen in the sidebar.
const links = [...document.querySelectorAll<HTMLAnchorElement>('.toc a')];
const byId = new Map(links.map((a) => [a.getAttribute('href')!.slice(1), a]));
const seen = new Set<string>();
const io = new IntersectionObserver((entries) => {
  for (const e of entries) {
    if (e.isIntersecting) seen.add(e.target.id); else seen.delete(e.target.id);
  }
  const first = [...byId.keys()].find((id) => seen.has(id));
  for (const [id, a] of byId) a.classList.toggle('on', id === first);
}, { rootMargin: '-70px 0px -70% 0px' });
for (const id of byId.keys()) {
  const el = document.getElementById(id);
  if (el) io.observe(el);
}
