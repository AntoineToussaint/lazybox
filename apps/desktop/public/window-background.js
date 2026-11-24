// Parser-blocking so even the empty document uses the last resolved theme.
try {
  const color = localStorage.getItem('lazybox:appearance:background');
  if (color) document.documentElement.style.setProperty('--window-background', color);
} catch { /* Storage may be unavailable in a private window. */ }
