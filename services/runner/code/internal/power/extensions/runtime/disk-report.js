// Filters apply only to the saved report. Cleanup still uses a fresh terminal scan.
const filters = document.getElementById('target-filters');
if (filters) {
  const query = document.getElementById('target-query');
  const large = document.getElementById('large-only');
  const count = document.getElementById('match-count');
  const rows = Array.from(document.querySelectorAll('#targets > li'));
  function update() {
    const term = query.value.trim().toLowerCase();
    let matches = 0;
    for (const row of rows) {
      const title = row.querySelector('strong').textContent;
      const path = row.querySelector('code').textContent;
      row.hidden = !(title + ' ' + path).toLowerCase().includes(term)
        || (large.checked && Number(row.dataset.bytes) < 1024 ** 3);
      if (!row.hidden) matches++;
    }
    count.textContent = term || large.checked ? matches + ' of ' + rows.length + ' targets shown' : '';
  }
  query.addEventListener('input', update);
  large.addEventListener('change', update);
  filters.hidden = false;
}
