// Page the Runner's execution history on demand. Each refresh reads just the
// displayed page, even in installations with years of conversation history.
function attentionPages(render, onError) {
  const pager = document.getElementById('history-pages');
  const newer = pager.querySelector('[data-newer]');
  const older = pager.querySelector('[data-older]');
  const label = pager.querySelector('output');
  let offset = 0;
  let loading = false;
  let hasMore = false;

  async function load(nextOffset = offset) {
    if (loading) return;
    loading = true;
    newer.disabled = older.disabled = true;
    try {
      const page = await lazybox.attention.list({ offset: nextOffset });
      offset = nextOffset;
      hasMore = page.hasMore;
      render(page);
      pager.hidden = offset === 0 && !hasMore;
      label.textContent = 'History page ' + (offset / 100 + 1);
    } catch (error) {
      onError(error);
    } finally {
      loading = false;
      newer.disabled = !offset;
      older.disabled = !hasMore;
    }
  }
  newer.onclick = () => load(Math.max(0, offset - 100));
  older.onclick = () => load(offset + 100);
  return () => load(offset);
}
