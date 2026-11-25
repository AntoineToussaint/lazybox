/** Only the dock's local panel page needs a scriptable child window. */
export function installDesktopWindowOpen(openLink: (url: string) => Promise<void>): void {
  const openPanel = window.open.bind(window);
  window.open = (url, target, features) => {
    const href = String(url || '');
    if (href.split('?')[0] === '/popout.html') return openPanel(url, target, features);
    if (/^(https?:|mailto:)/i.test(href)) {
      void openLink(href).catch(error => console.error('Could not open link', error));
    }
    return null;
  };
}
