(() => {
  const qs = location.search;
  const live = document.getElementById('live');
  const content = document.getElementById('content');
  // Files the reader opened or closed by hand keep that state across refreshes.
  const toggled = new Map();
  // The tab the reader picked. Behind a proxy the query is invisible, so the
  // path is part of the key. Until a pick, the server's default stands, and it
  // can move (a tour landing in another repo, say).
  const tabKey = 'recto-tab:' + location.pathname + qs;
  let tab = null;
  try { tab = sessionStorage.getItem(tabKey); } catch {}

  // Focus is one span lit up in the full diff: the last quote clicked, or the
  // agent's `recto focus` when it changes. It survives refreshes.
  let current = null;           // "repo:file:start:end"
  const markers = new Map();    // section id -> last focus marker seen
  const back = document.createElement('button');
  back.id = 'back';
  back.type = 'button';
  back.textContent = '↩ back to tour';
  document.body.appendChild(back);
  let backY = null;

  function spanRows(spec) {
    const [ri, fi, s0, e0] = spec.split(':');
    const s = Number(s0), e = Number(e0);
    const table = document.querySelector(`#f${ri}-${fi} table.diff`);
    if (!table) return [];
    const rows = [...table.rows];
    let first = -1, last = -1;
    rows.forEach((tr, i) => {
      const m = tr.id && tr.id.match(/^L\d+-[0-9a-f]+-(\d+)$/);
      if (!m) return;
      const n = Number(m[1]);
      if (n >= s && n <= e) { if (first < 0) first = i; last = i; }
    });
    // Deletions between the first and last line belong to the span too.
    return first < 0 ? [] : rows.slice(first, last + 1);
  }

  function highlight() {
    for (const tr of content.querySelectorAll('tr.focus')) tr.classList.remove('focus');
    if (current) for (const tr of spanRows(current)) tr.classList.add('focus');
  }

  function focusSpan(spec, scroll) {
    current = spec;
    const [ri, fi] = spec.split(':');
    const section = document.getElementById('repo-' + ri);
    if (section && section.parentElement.classList.contains('tabbed') && !section.classList.contains('active')) {
      activate(section.id, true);
    }
    const file = document.getElementById(`f${ri}-${fi}`);
    if (file && !file.open) { file.open = true; toggled.set(file.id, true); }
    highlight();
    const rows = spanRows(spec);
    if (scroll && rows.length) {
      rows[0].scrollIntoView({ block: rows.length > 12 ? 'start' : 'center', behavior: 'smooth' });
      for (const tr of rows) { tr.classList.remove('flash'); void tr.offsetWidth; tr.classList.add('flash'); }
    } else if (scroll && file) {
      file.scrollIntoView({ block: 'start', behavior: 'smooth' });
    }
  }

  function clearFocus() {
    current = null;
    highlight();
    back.classList.remove('shown');
  }

  // Follow the agent: a focus marker that changed since we last looked is a
  // "look here now". On the first look it only lights up, so opening the page
  // doesn't yank you to wherever the agent pointed yesterday.
  function syncMarkers(initial) {
    let moved = false;
    for (const section of content.querySelectorAll('section.repo')) {
      const marker = section.querySelector('.focus-marker');
      const val = marker ? marker.dataset.focus : '';
      const changed = markers.has(section.id) && markers.get(section.id) !== val;
      markers.set(section.id, val);
      if (val && (changed || (initial && !current))) {
        focusSpan(val, changed && !initial);
        moved = moved || (changed && !initial);
      } else if (!val && changed && current && current.startsWith(section.id.slice(5) + ':')) {
        current = null;
      }
    }
    highlight();
    return moved;
  }

  back.addEventListener('click', () => {
    if (backY !== null) window.scrollTo({ top: backY, behavior: 'smooth' });
    back.classList.remove('shown');
  });
  document.addEventListener('keydown', (e) => { if (e.key === 'Escape') clearFocus(); });

  content.addEventListener('toggle', (e) => {
    if (e.target.matches('details.file') && e.target.id) toggled.set(e.target.id, e.target.open);
  }, true);

  function activate(id, remember) {
    if (remember) {
      tab = id;
      try { sessionStorage.setItem(tabKey, id); } catch {}
    }
    for (const b of content.querySelectorAll('nav.tabs button')) b.classList.toggle('active', b.dataset.tab === id);
    for (const s of content.querySelectorAll('.tabbed > section.repo')) s.classList.toggle('active', s.id === id);
  }

  function applyTab() {
    const buttons = [...content.querySelectorAll('nav.tabs button[data-tab]')];
    if (!buttons.length) return;
    const pick = buttons.find((b) => b.dataset.tab === tab)
      || buttons.find((b) => 'default' in b.dataset)
      || buttons[0];
    activate(pick.dataset.tab, false);
  }

  function syncExpandButtons() {
    for (const btn of content.querySelectorAll('.expand-all')) {
      const section = btn.closest('section.repo');
      const files = section ? [...section.querySelectorAll('details.file')] : [];
      btn.textContent = files.length && files.every((d) => d.open) ? 'collapse all' : 'expand all';
    }
  }

  // Refreshes can overlap; only the newest one gets to replace the page.
  let generation = 0;
  async function refresh() {
    const mine = ++generation;
    let res;
    try { res = await fetch('fragment' + qs, { cache: 'no-store' }); } catch { return; }
    if (!res.ok) return;
    const html = await res.text();
    if (mine !== generation) return;
    const y = window.scrollY;
    content.innerHTML = html;
    for (const [id, open] of toggled) {
      const d = document.getElementById(id);
      if (d) d.open = open;
    }
    applyTab();
    syncExpandButtons();
    if (!syncMarkers(false)) window.scrollTo(0, y);
    live.classList.add('pulse');
    setTimeout(() => live.classList.remove('pulse'), 600);
  }

  function reveal(id) {
    const el = document.getElementById(id);
    if (!el) return;
    const section = el.closest('.tabbed > section.repo');
    if (section && !section.classList.contains('active')) activate(section.id, true);
    for (let p = el.parentElement; p; p = p.parentElement) {
      if (p.tagName === 'DETAILS') p.open = true;
    }
    // A line centers so its context shows; a file or heading starts at the
    // top, or a tall one would land somewhere in its middle.
    el.scrollIntoView({ block: el.tagName === 'TR' ? 'center' : 'start', behavior: 'smooth' });
    if (el.tagName === 'TR') {
      el.classList.remove('flash'); void el.offsetWidth; el.classList.add('flash');
    }
  }

  content.addEventListener('click', (e) => {
    const tabBtn = e.target.closest('nav.tabs button[data-tab]');
    if (tabBtn) {
      activate(tabBtn.dataset.tab, true);
      window.scrollTo(0, 0);
      return;
    }
    const all = e.target.closest('.expand-all');
    if (all) {
      const files = [...all.closest('section.repo').querySelectorAll('details.file')];
      const open = !files.every((d) => d.open);
      for (const d of files) { d.open = open; toggled.set(d.id, open); }
      syncExpandButtons();
      return;
    }
    const a = e.target.closest('a[href^="#"]');
    if (!a) return;
    e.preventDefault();
    if (a.dataset.focus) {
      backY = window.scrollY;
      focusSpan(a.dataset.focus, true);
      back.classList.add('shown');
      return;
    }
    reveal(a.getAttribute('href').slice(1));
  });
  content.addEventListener('toggle', syncExpandButtons, true);

  applyTab();
  syncExpandButtons();
  syncMarkers(true);

  // EventSource retries a dropped connection by itself, but gives up for good
  // on an error response, which is what a proxy sends while recto restarts.
  // So a closed stream is reopened here, and the page catches up when it is.
  let dropped = false;
  function listen() {
    const es = new EventSource('events' + qs);
    es.addEventListener('open', () => {
      live.classList.add('on');
      if (dropped) { dropped = false; refresh(); }
    });
    es.addEventListener('error', () => {
      live.classList.remove('on');
      dropped = true;
      if (es.readyState === EventSource.CLOSED) setTimeout(listen, 3000);
    });
    es.addEventListener('changed', refresh);
  }
  listen();

  // A parent frame can point this page at code: postMessage({focus: "L0-3-42"}).
  window.addEventListener('message', (e) => {
    if (e.source !== window.parent) return;
    if (e.data && typeof e.data.focus === 'string') reveal(e.data.focus);
    if (e.data && e.data.refresh) refresh();
  });
})();
