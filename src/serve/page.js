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

  // A repo narrowed to one commit, by index: what the revision list picked.
  // The server renders whatever the fragment request asks for.
  const viewKey = 'recto-views:' + location.pathname + qs;
  let views = new Map();
  try { views = new Map(JSON.parse(sessionStorage.getItem(viewKey) || '[]')); } catch {}
  function saveViews() {
    try { sessionStorage.setItem(viewKey, JSON.stringify([...views])); } catch {}
  }
  function fragmentURL() {
    let url = 'fragment' + qs;
    for (const [i, spec] of views) {
      url += (url.includes('?') ? '&' : '?') + 'view=' + encodeURIComponent(i + ':' + spec);
    }
    return url;
  }
  // A span to light up once the next refresh lands, for a quote or a focus
  // that points outside the commit a repo is narrowed to.
  let pendingFocus = null;
  function widenFor(spec) {
    const ri = spec.split(':')[0];
    if (!views.has(ri) || spanRows(spec).length) return false;
    views.delete(ri);
    saveViews();
    pendingFocus = spec;
    refresh();
    return true;
  }

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
      if (val && changed && !initial && widenFor(val)) {
        moved = true;
      } else if (val && (changed || (initial && !current))) {
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
    if (e.target.matches('details.file, details.revs') && e.target.id) toggled.set(e.target.id, e.target.open);
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
    try { res = await fetch(fragmentURL(), { cache: 'no-store' }); } catch { return; }
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
    const moved = syncMarkers(false);
    if (pendingFocus) {
      const spec = pendingFocus;
      pendingFocus = null;
      focusSpan(spec, true);
    } else if (!moved) {
      window.scrollTo(0, y);
    }
    decorate();
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
    const pick = e.target.closest('button[data-view]');
    if (pick) {
      const [ri, ...rest] = pick.dataset.view.split(':');
      const spec = rest.join(':');
      if (spec) views.set(ri, spec); else views.delete(ri);
      saveViews();
      pick.classList.add('pending');
      refresh();
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
      back.classList.add('shown');
      if (widenFor(a.dataset.focus)) return;
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
  if (views.size) refresh();

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


  // Comments. Select anything on the page, say something about it, and the
  // frame around this page (rig serve's) holds it for the agent, sending a
  // batch as one message. This page only works out what a selection is about;
  // delivery and tracking belong to the frame, which posts back where each
  // comment stands so the page can mark the spot. With no frame there's no
  // one to deliver to, so none of this shows.
  const framed = window.parent !== window;
  let comments = [];
  const pill = document.createElement('button');
  pill.id = 'comment-pill';
  pill.type = 'button';
  pill.textContent = 'comment';
  const composer = document.createElement('form');
  composer.id = 'composer';
  composer.innerHTML = '<div class=c-anchor></div><blockquote class=c-quote></blockquote>'
    + '<textarea rows=3 placeholder="what should the agent know or do?"></textarea>'
    + '<div class=c-actions><span class=c-hint>⌘↩ hold · ⇧⌘↩ send now · esc</span>'
    + '<button type=button class=c-cancel>cancel</button><button type=button class=c-send>send now</button>'
    + '<button type=submit class=c-hold>hold</button></div>';
  if (framed) document.body.append(pill, composer);
  let pending = null; // {anchor, quote} the composer is writing about

  function rowLines(root, range) {
    const rows = [...root.querySelectorAll('tr')].filter((tr) => tr.cells.length === 3 && range.intersectsNode(tr));
    const nums = (i) => rows.map((tr) => parseInt(tr.cells[i].textContent, 10)).filter((n) => n > 0);
    return { added: nums(1), removed: nums(0) };
  }

  function spanText(nums) {
    const lo = Math.min(...nums), hi = Math.max(...nums);
    return lo === hi ? String(lo) : lo + '-' + hi;
  }

  // What a selection is about, as rig records it: a diff span, a section of
  // a tour or the briefing, or plain text somewhere in a repo.
  function anchorFor(range) {
    let node = range.commonAncestorContainer;
    if (node.nodeType !== 1) node = node.parentElement;
    const section = node.closest('section.repo');
    const repo = section ? section.dataset.repo : '';
    const figure = node.closest('figure.quote');
    const file = node.closest('details.file');
    if (figure || file) {
      let r = repo, path;
      if (figure) {
        path = figure.querySelector('figcaption').textContent.trim().replace(/:[\d-]+$/, '');
        if (!r) { const i = path.indexOf('/'); r = path.slice(0, i); path = path.slice(i + 1); }
      } else {
        path = file.dataset.path;
      }
      const { added, removed } = rowLines(figure || file.querySelector('table.diff') || file, range);
      const anchor = { kind: 'diff', repo: r, path, label: (r ? r + '/' : '') + path };
      if (added.length) {
        anchor.start = Math.min(...added);
        anchor.end = Math.max(...added);
        anchor.label += ':' + spanText(added);
      } else if (removed.length) {
        anchor.label += ' (removed lines ' + spanText(removed) + ')';
      }
      return anchor;
    }
    const ask = node.closest('.ask');
    if (ask) return askAnchor(ask);
    const art = node.closest('article.tour, article.brief');
    if (art) {
      let heading = null;
      for (const h of art.querySelectorAll('h1[id], h2[id]')) {
        if (h.contains(range.startContainer) || (h.compareDocumentPosition(range.startContainer) & Node.DOCUMENT_POSITION_FOLLOWING)) heading = h;
      }
      const brief = art.classList.contains('brief');
      const what = brief ? 'Briefing' : repo ? repo + ' tour' : 'Group tour';
      const title = heading ? heading.textContent.trim() : '';
      return { kind: brief ? 'brief' : 'tour', repo, section: title, label: title ? what + ' › ' + title : what };
    }
    return { kind: 'text', repo, label: repo || 'the page' };
  }

  function askAnchor(card) {
    const q = card.querySelector('.ask-q').textContent.trim().split('\n')[0];
    return { kind: 'ask', section: card.dataset.ask, label: 'Ask ' + card.dataset.ask + ': ' + (q.length > 80 ? q.slice(0, 79) + '…' : q) };
  }

  function send(anchor, quote, body, now) {
    window.parent.postMessage({ comment: { anchor, quote, body, send: now } }, location.origin);
  }

  function hidePill() { pill.classList.remove('shown'); }
  function closeComposer() { composer.classList.remove('shown'); pending = null; }

  document.addEventListener('selectionchange', () => {
    if (!framed) return;
    const sel = document.getSelection();
    if (!sel || sel.isCollapsed || !sel.rangeCount || sel.toString().trim().length < 2) return hidePill();
    const range = sel.getRangeAt(0);
    if (!content.contains(range.commonAncestorContainer) || range.commonAncestorContainer.closest?.('.ask')) return hidePill();
    const rect = range.getBoundingClientRect();
    pill.style.top = Math.min(window.innerHeight - 40, rect.bottom + 6) + 'px';
    pill.style.left = Math.max(8, Math.min(window.innerWidth - 100, rect.right - 40)) + 'px';
    pill.classList.add('shown');
  });
  pill.addEventListener('mousedown', (e) => e.preventDefault()); // keep the selection
  pill.addEventListener('click', () => {
    const sel = document.getSelection();
    if (!sel || !sel.rangeCount) return;
    const range = sel.getRangeAt(0);
    pending = { anchor: anchorFor(range), quote: sel.toString().trim().slice(0, 4000) };
    composer.querySelector('.c-anchor').textContent = pending.anchor.label;
    const q = pending.quote;
    composer.querySelector('.c-quote').textContent = q.length > 280 ? q.slice(0, 279) + '…' : q;
    hidePill();
    composer.classList.add('shown');
    composer.querySelector('textarea').focus();
  });
  function submit(now) {
    const ta = composer.querySelector('textarea');
    if (!pending || !ta.value.trim()) return ta.focus();
    send(pending.anchor, pending.quote, ta.value.trim(), now);
    ta.value = '';
    closeComposer();
    document.getSelection()?.removeAllRanges();
  }
  composer.addEventListener('submit', (e) => { e.preventDefault(); submit(false); });
  composer.querySelector('.c-send').addEventListener('click', () => submit(true));
  composer.querySelector('.c-cancel').addEventListener('click', closeComposer);
  composer.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') { e.stopPropagation(); closeComposer(); }
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) { e.preventDefault(); submit(e.shiftKey); }
  });

  // Asks: questions in the briefing, answered in place. An answer is a
  // comment anchored to the ask, so it batches with everything else.
  content.addEventListener('click', (e) => {
    const choice = e.target.closest('.ask-choices button');
    if (choice) {
      for (const b of choice.parentElement.children) b.classList.toggle('picked', b === choice && !b.classList.contains('picked'));
      return;
    }
    const act = e.target.closest('.ask-answer button[data-act]');
    if (act) {
      const card = act.closest('.ask');
      const picked = card.querySelector('.ask-choices .picked');
      const text = card.querySelector('.ask-answer textarea').value.trim();
      if (!picked && !text) return card.querySelector('.ask-answer textarea').focus();
      const body = [picked ? 'Choice: ' + picked.dataset.choice : '', text].filter(Boolean).join('\n');
      send(askAnchor(card), card.querySelector('.ask-q').textContent.trim(), body, act.dataset.act === 'send');
    }
  });

  // Where each comment stands, marked on the page: a dot in the gutter of a
  // diff line, a badge on a section, an ask showing its answer. Redrawn after
  // every refresh, since a refresh replaces the page.
  const stateWords = { held: 'held', failed: 'not delivered', sent: 'sent', start: 'on it', done: 'handled' };
  function decorate() {
    for (const el of content.querySelectorAll('.has-comment')) { el.classList.remove('has-comment'); delete el.dataset.cstate; }
    for (const el of content.querySelectorAll('.c-badge')) el.remove();
    const answered = new Map();
    for (const c of comments) {
      const a = c.anchor;
      if (a.kind === 'ask') { answered.set(a.section, c); continue; }
      if (a.kind === 'diff' && a.start) {
        const section = content.querySelector(`section.repo[data-repo="${CSS.escape(a.repo)}"]`);
        const file = section && [...section.querySelectorAll('details.file')].find((d) => d.dataset.path === a.path);
        const end = a.end || a.start;
        const rows = file ? [...file.querySelectorAll('tr')].filter((tr) => {
          const n = tr.cells.length === 3 && parseInt(tr.cells[1].textContent, 10);
          return n >= a.start && n <= end;
        }) : [];
        for (const row of rows) { row.classList.add('has-comment'); row.dataset.cstate = c.state; row.title = c.body; }
      } else if ((a.kind === 'tour' || a.kind === 'brief') && a.section) {
        const h = [...content.querySelectorAll(`article.${a.kind} h1, article.${a.kind} h2`)].find((x) => x.textContent.trim() === a.section);
        if (h) {
          const b = document.createElement('span');
          b.className = 'c-badge';
          b.dataset.cstate = c.state;
          b.textContent = stateWords[c.state] || c.state;
          b.title = c.body;
          h.append(b);
        }
      }
    }
    for (const card of content.querySelectorAll('.ask')) {
      const c = answered.get(card.dataset.ask);
      const slot = card.querySelector('.ask-answer');
      card.classList.toggle('answered', !!c);
      if (c) {
        slot.innerHTML = '<div class=ask-given></div><span class=c-badge></span>';
        slot.querySelector('.ask-given').textContent = c.body;
        const b = slot.querySelector('.c-badge');
        b.dataset.cstate = c.state;
        b.textContent = stateWords[c.state] || c.state;
      } else if (!slot.querySelector('textarea') && framed) {
        slot.innerHTML = '<textarea rows=2 placeholder="answer…"></textarea>'
          + '<div class=c-actions><button type=button data-act=send>send now</button><button type=button data-act=hold class=c-hold>hold answer</button></div>';
      }
    }
    // The frame counts what's waiting on the person.
    if (framed) {
      const open = [...content.querySelectorAll('.ask')].filter((c) => !answered.has(c.dataset.ask)).length;
      window.parent.postMessage({ asks: open }, location.origin);
    }
  }

  // A parent frame can point this page at code: postMessage({focus: "L0-3-42"}).
  window.addEventListener('message', (e) => {
    if (e.source !== window.parent) return;
    if (e.data && typeof e.data.focus === 'string') reveal(e.data.focus);
    if (e.data && e.data.refresh) refresh();
    if (e.data && Array.isArray(e.data.comments)) { comments = e.data.comments; decorate(); }
  });
  decorate();
})();
