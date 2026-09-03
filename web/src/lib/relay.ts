// Whatever host served this page is the relay, so the same build works on
// knoot.dev, a preview box and localhost.
export const WS_SCHEME = location.protocol === 'https:' ? 'wss' : 'ws';
export const RELAY_WS = `${WS_SCHEME}://${location.host}/ws`;
export const RELAY_HOST = location.host;

export const esc = (s: unknown): string =>
  String(s ?? '').replace(/[&<>"']/g, (c) =>
    ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]!);

/** Wire every .copy button on the page to the code element beside it. */
export function wireCopyButtons(root: ParentNode = document): void {
  root.addEventListener('click', (e) => {
    const b = (e.target as HTMLElement)?.closest('.copy') as HTMLButtonElement | null;
    if (!b) return;
    const code = b.parentElement?.querySelector('code')?.textContent ?? '';
    navigator.clipboard?.writeText(code).then(
      () => { b.textContent = 'Copied'; setTimeout(() => { b.textContent = 'Copy'; }, 1400); },
      () => { b.textContent = 'Copy failed'; },
    );
  });
}
