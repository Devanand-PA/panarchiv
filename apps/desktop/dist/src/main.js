// Panarchiv frontend. Dependency-free: talks to Tauri via the injected bridge.

async function invoke(cmd, args = {}) {
  const bridge = window.__TAURI_INTERNALS__;
  if (!bridge || typeof bridge.invoke !== 'function') {
    throw new Error('Panarchiv must be running inside the Tauri app');
  }
  return bridge.invoke(cmd, args);
}

const $ = (id) => document.getElementById(id);

/* ------------------------------------------------------------------ theme */

// 'system' | 'light' | 'dark' — stored so "system" keeps following OS changes.
const THEME_MODES = ['system', 'light', 'dark'];

function applyTheme(mode) {
  const root = document.documentElement;
  if (mode === 'system') root.removeAttribute('data-theme');
  else root.setAttribute('data-theme', mode);
  localStorage.setItem('panarchiv.theme', mode);
  updateThemeIcon(mode);
}

function currentThemeMode() {
  return localStorage.getItem('panarchiv.theme') || 'system';
}

function updateThemeIcon(mode) {
  const btn = $('theme-toggle');
  if (!btn) return;
  const icons = { system: '🌐', light: '☀️', dark: '🌙' };
  const labels = {
    system: 'Theme: follow system (click for light)',
    light: 'Theme: light (click for dark)',
    dark: 'Theme: dark (click for system)',
  };
  btn.textContent = icons[mode] || icons.system;
  btn.title = labels[mode] || labels.system;
  btn.setAttribute('aria-label', btn.title);
}

function initTheme() {
  applyTheme(currentThemeMode());
  // While in system mode, follow OS changes live.
  window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
    if (currentThemeMode() === 'system') applyTheme('system');
  });
}

/* ------------------------------------------------------------------ utils */

function log(message) {
  const el = $('log');
  el.textContent += `${new Date().toLocaleTimeString()}  ${message}\n`;
  el.scrollTop = el.scrollHeight;
}

function status(message, live = false) {
  const el = $('status');
  el.textContent = message;
  el.classList.toggle('live', live);
}

// Build entities by concatenation so the mapping survives editing.
function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => {
    switch (c) {
      case '&': return '&' + 'amp;';
      case '<': return '&' + 'lt;';
      case '>': return '&' + 'gt;';
      case '"': return '&' + 'quot;';
      case "'": return '&' + '#39;';
      default: return c;
    }
  });
}

function splitTags(value) {
  return value.split(',').map((t) => t.trim()).filter(Boolean);
}

async function payloads(fileList) {
  return Promise.all(Array.from(fileList).map(async (f) => ({
    name: f.webkitRelativePath || f.name,
    bytes: Array.from(new Uint8Array(await f.arrayBuffer())),
  })));
}

function extOf(filename) {
  const i = String(filename).lastIndexOf('.');
  return i > 0 ? String(filename).slice(i + 1).toLowerCase() : '';
}

function humanSize(bytes) {
  if (bytes == null) return '';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let n = Number(bytes);
  let u = 0;
  while (n >= 1024 && u < units.length - 1) { n /= 1024; u += 1; }
  return `${n >= 10 || u === 0 ? Math.round(n) : n.toFixed(1)} ${units[u]}`;
}

function shortDate(iso) {
  try { return new Date(iso).toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' }); }
  catch { return iso; }
}

function fileIcon(filename, kind) {
  const e = extOf(filename);
  if (kind === 'Image') return { ico: '🖼️', label: 'Image' };
  if (e === 'pdf') return { ico: '📕', label: 'PDF' };
  if (['doc', 'docx', 'odt', 'rtf', 'txt', 'md'].includes(e)) return { ico: '📘', label: 'Document' };
  if (['xls', 'xlsx', 'csv', 'ods'].includes(e)) return { ico: '📊', label: 'Spreadsheet' };
  if (['ppt', 'pptx', 'odp'].includes(e)) return { ico: '📈', label: 'Presentation' };
  if (['zip', 'gz', 'tar', '7z', 'rar'].includes(e)) return { ico: '🗜️', label: 'Archive' };
  return { ico: '📎', label: kind || 'File' };
}

/* --------------------------------------------------------------- tag editor */

let tagEditorDocId = null;
let tagEditorTags = [];

function closeTagEditor() {
  const editor = $('tag-editor');
  if (editor) editor.remove();
  document.removeEventListener('click', closeTagEditorOnOutside);
  tagEditorDocId = null;
  tagEditorTags = [];
}

function closeTagEditorOnOutside(e) {
  const editor = $('tag-editor');
  if (editor && !editor.contains(e.target)) closeTagEditor();
}

function openTagEditor(btn, docId, currentTags) {
  closeTagEditor();
  tagEditorDocId = docId;
  tagEditorTags = [...currentTags];

  const rect = btn.getBoundingClientRect();
  const editor = document.createElement('div');
  editor.id = 'tag-editor';
  editor.className = 'tag-editor';
  editor.innerHTML = `
    <div class="tag-editor-header">
      <span>Tags</span>
      <button class="icon-btn" id="tag-editor-close" aria-label="Close">✕</button>
    </div>
    <div class="tag-editor-tags" id="tag-editor-tags"></div>
    <div class="tag-editor-input-row">
      <input id="tag-editor-input" type="text" placeholder="Add a tag (comma separated)" />
      <button class="mini primary" id="tag-editor-add">Add</button>
    </div>
    <div class="tag-editor-actions">
      <button class="danger mini" id="tag-editor-cancel">Cancel</button>
      <button class="primary mini" id="tag-editor-save">Save</button>
    </div>
  `;
  document.body.appendChild(editor);
  editor.classList.add('show');

  const mw = Math.min(editor.offsetWidth, 320);
  let left = Math.max(8, Math.min(rect.right - mw, window.innerWidth - mw - 8));
  let top = rect.bottom + 6;
  if (top + 220 > window.innerHeight) top = Math.max(8, rect.top - 230);
  editor.style.left = `${left}px`;
  editor.style.top = `${top}px`;

  renderTagEditorTags();
  $('tag-editor-input').focus();

  $('tag-editor-close').addEventListener('click', closeTagEditor);
  $('tag-editor-cancel').addEventListener('click', closeTagEditor);
  $('tag-editor-add').addEventListener('click', addTagFromInput);
  $('tag-editor-save').addEventListener('click', saveTags);
  $('tag-editor-input').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') addTagFromInput();
    if (e.key === 'Escape') closeTagEditor();
  });

  setTimeout(() => {
    // Only arm the dismiss handler if this editor is still the open one
    // (it may have been closed by Escape before the timeout fired).
    if ($('tag-editor') === editor) document.addEventListener('click', closeTagEditorOnOutside);
  }, 0);
}

function renderTagEditorTags() {
  const container = $('tag-editor-tags');
  if (!container) return;
  container.innerHTML = tagEditorTags.map((tag, i) => `
    <span class="tag tag-editor-tag" data-tag-index="${i}">
      ${escapeHtml(tag)}
      <button class="tag-editor-remove" data-tag-index="${i}" aria-label="Remove tag">✕</button>
    </span>
  `).join('') + '<span class="tag tag-editor-new" id="tag-editor-new-tag">+ New tag</span>';
  container.querySelectorAll('.tag-editor-remove').forEach((btn) => {
    btn.addEventListener('click', (e) => {
      e.stopPropagation();
      const idx = Number(btn.dataset.tagIndex);
      tagEditorTags.splice(idx, 1);
      renderTagEditorTags();
    });
  });
  container.querySelectorAll('.tag-editor-tag').forEach((span) => {
    span.addEventListener('click', (e) => {
      if (e.target.closest('.tag-editor-remove')) return;
      const idx = Number(span.dataset.tagIndex);
      const old = tagEditorTags[idx];
      // Inline edit in place of the chip.
      const input = document.createElement('input');
      input.type = 'text';
      input.className = 'tag-edit-input';
      input.value = old;
      span.replaceWith(input);
      input.focus();
      input.select();
      let committed = false;
      const commit = () => {
        if (committed) return;
        committed = true;
        const v = input.value.trim();
        if (v) tagEditorTags[idx] = v;
        renderTagEditorTags();
      };
      input.addEventListener('keydown', (ev) => {
        if (ev.key === 'Enter') { ev.preventDefault(); commit(); }
        if (ev.key === 'Escape') { ev.preventDefault(); ev.stopPropagation(); committed = true; renderTagEditorTags(); }
      });
      input.addEventListener('blur', commit);
      e.stopPropagation();
    });
  });
  $('tag-editor-new-tag').addEventListener('click', () => $('tag-editor-input').focus());
}

function addTagFromInput() {
  const input = $('tag-editor-input');
  const value = input.value.trim();
  if (!value) return;
  const newTags = splitTags(value);
  for (const tag of newTags) {
    if (!tagEditorTags.includes(tag)) tagEditorTags.push(tag);
  }
  input.value = '';
  renderTagEditorTags();
}

async function saveTags() {
  if (!tagEditorDocId) return;
  try {
    status('Saving tags...');
    await guarded(() => invoke('update_document_tags', { id: tagEditorDocId, tags: tagEditorTags }));
    log(`Tags updated for document ${tagEditorDocId}`);
    await refresh();
    status('Ready');
  } catch (e) {
    if (!/cancelled/i.test(String(e))) {
      log(`Failed to update tags: ${e}`);
      status('Failed to update tags');
    }
  }
  closeTagEditor();
}

/* --------------------------------------------------------------- programs */

const PROG_KEY = 'panarchiv.programs';
const LASTPROG_KEY = 'panarchiv.lastprog';

const EXAMPLE_PROGRAMS = [
  { name: 'Image viewer', command: 'gwenview {file}', exts: ['png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp'] },
  { name: 'GIMP', command: 'gimp {file}', exts: [] },
  { name: 'Document viewer', command: 'evince {file}', exts: ['pdf'] },
  { name: 'Text editor (VS Code)', command: 'code {file}', exts: ['txt', 'md', 'json', 'csv'] },
];

let programs = [];
let lastProgByExt = {};

function loadState() {
  try {
    const raw = localStorage.getItem(PROG_KEY);
    const parsed = raw ? JSON.parse(raw) : [];
    programs = Array.isArray(parsed) ? parsed : [];
  } catch { programs = []; }
  try {
    const raw = localStorage.getItem(LASTPROG_KEY);
    lastProgByExt = raw ? JSON.parse(raw) : {};
  } catch { lastProgByExt = {}; }
}

function savePrograms() {
  localStorage.setItem(PROG_KEY, JSON.stringify(programs));
  renderPrograms();
}

function saveLastProg() {
  localStorage.setItem(LASTPROG_KEY, JSON.stringify(lastProgByExt));
}

function normExts(value) {
  return splitTags(value).map((t) => t.replace(/^\./, '').toLowerCase());
}

function programMatches(prog, ext) {
  if (!prog.exts || prog.exts.length === 0) return true;
  return prog.exts.includes(ext);
}

function renderPrograms() {
  const root = $('programs');
  if (!programs.length) {
    root.innerHTML = '<div class="programs-empty">No programs yet. Add one below, or reset to examples.</div>';
    return;
  }
  root.innerHTML = programs.map((p, i) => `
    <div class="program">
      <span class="glyph" style="font-size:15px">🧩</span>
      <div class="p-info">
        <div class="p-name">${escapeHtml(p.name)}</div>
        <div class="p-cmd">${escapeHtml(p.command)}</div>
      </div>
      ${p.exts && p.exts.length ? `<span class="p-exts">${escapeHtml(p.exts.join(' '))}</span>` : ''}
      <button class="mini danger" data-prog-remove="${i}" title="Remove">✕</button>
    </div>`).join('');
  root.querySelectorAll('[data-prog-remove]').forEach((btn) => btn.addEventListener('click', () => {
    programs.splice(Number(btn.dataset.progRemove), 1);
    savePrograms();
    log('Removed program');
  }));
}

/* ------------------------------------------------------------------ state */

let allDocs = [];
let archiveInfo = null;
const state = {
  query: '',
  selected: { type: 'all' }, // { type:'all' } | { type:'untagged' } | { type:'path', path:[...] }
  view: 'grid',
  sort: 'date',
  expanded: new Set(),      // expanded document ids
  expandedPaths: new Set(), // expanded folder paths (JSON strings)
};
const thumbCache = new Map();

/* ------------------------------------------------------ directory (tags) */

function buildTree(docs) {
  const root = { name: '', path: [], children: new Map(), docs: new Set() };
  const untagged = [];
  function ensure(segs) {
    let cur = root;
    const acc = [];
    for (const seg of segs) {
      acc.push(seg);
      if (!cur.children.has(seg)) {
        cur.children.set(seg, { name: seg, path: acc.slice(), children: new Map(), docs: new Set() });
      }
      cur = cur.children.get(seg);
    }
    return cur;
  }
  for (const doc of docs) {
    const tags = (doc.tags || []).map((t) => t.trim()).filter(Boolean);
    if (!tags.length) { untagged.push(doc); continue; }
    for (const tag of tags) {
      const segs = tag.split('/').map((s) => s.trim()).filter(Boolean);
      if (!segs.length) { untagged.push(doc); continue; }
      ensure(segs).docs.add(doc.id);
    }
  }
  return { root, untagged };
}

function subtreeCount(node) {
  let n = node.docs.size;
  for (const child of node.children.values()) n += subtreeCount(child);
  return n;
}

function renderTree() {
  const tree = buildTree(allDocs);
  const root = $('tree');
  const parts = [];

  const isAll = state.selected.type === 'all';
  parts.push(`
    <div class="tree-row ${isAll ? 'selected' : ''}" data-tree-select data-type="all" style="padding-left:8px">
      <span class="spacer"></span><span class="glyph">🗂️</span>
      <span class="name">All files</span><span class="count">${allDocs.length}</span>
    </div>`);

  const isUntagged = state.selected.type === 'untagged';
  parts.push(`
    <div class="tree-row ${isUntagged ? 'selected' : ''}" data-tree-select data-type="untagged" style="padding-left:8px">
      <span class="spacer"></span><span class="glyph">📥</span>
      <span class="name">Untagged</span><span class="count">${tree.untagged.length}</span>
    </div>`);

  const sorted = [...tree.root.children.values()].sort((a, b) => a.name.localeCompare(b.name));
  for (const child of sorted) parts.push(treeNode(child, 0));

  root.innerHTML = parts.join('');
}

function treeNode(node, depth) {
  const pathKey = JSON.stringify(node.path);
  const hasKids = node.children.size > 0;
  const open = state.expandedPaths.has(pathKey);
  const isSel = state.selected.type === 'path' && JSON.stringify(state.selected.path) === pathKey;
  const pad = 8 + depth * 14;
  let html = `
    <div class="tree-row ${isSel ? 'selected' : ''}" data-tree-select data-path="${escapeHtml(pathKey)}" style="padding-left:${pad}px">
      ${hasKids
        ? `<span class="chev" data-tree-toggle data-path="${escapeHtml(pathKey)}">${open ? '▾' : '▸'}</span>`
        : '<span class="spacer"></span>'}
      <span class="glyph">${hasKids ? '📂' : '📁'}</span>
      <span class="name">${escapeHtml(node.name)}</span>
      <span class="count">${node.docs.size || subtreeCount(node)}</span>
    </div>`;
  if (hasKids && open) {
    const sorted = [...node.children.values()].sort((a, b) => a.name.localeCompare(b.name));
    html += sorted.map((c) => treeNode(c, depth + 1)).join('');
  }
  return `<div class="tree-branch">${html}</div>`;
}

/* ------------------------------------------------------------- matching */

function docMatches(doc, q) {
  const n = q.toLowerCase();
  if ((doc.title || '').toLowerCase().includes(n)) return true;
  if ((doc.tags || []).some((t) => String(t).toLowerCase().includes(n))) return true;
  return (doc.items || []).some((i) =>
    (i.filename || '').toLowerCase().includes(n) ||
    (i.ocr && i.ocr.text && i.ocr.text.toLowerCase().includes(n)));
}

function currentDocs() {
  let list;
  const q = state.query.trim();
  if (q) {
    list = allDocs.filter((d) => docMatches(d, q));
  } else if (state.selected.type === 'untagged') {
    list = allDocs.filter((d) => !(d.tags || []).length);
  } else if (state.selected.type === 'path') {
    const target = state.selected.path.join('/');
    list = allDocs.filter((d) => (d.tags || []).includes(target));
  } else {
    list = allDocs.slice();
  }

  const sizeOf = (d) => (d.items || []).reduce((s, i) => s + (i.size || 0), 0);
  list.sort((a, b) => {
    if (state.sort === 'name') return (a.title || '').localeCompare(b.title || '');
    if (state.sort === 'files') return (b.items || []).length - (a.items || []).length;
    if (state.sort === 'size') return sizeOf(b) - sizeOf(a);
    return new Date(b.updated_at) - new Date(a.updated_at); // date (default)
  });
  return list;
}

/* ------------------------------------------------------------- rendering */

function renderBreadcrumb() {
  const el = $('breadcrumb');
  const parts = [];
  const segOf = (label, sel, isLast) => {
    const cls = isLast ? 'crumb current' : 'crumb';
    return `<button class="${cls}" data-crumb='${escapeHtml(JSON.stringify(sel))}'>${escapeHtml(label)}</button>`;
  };
  parts.push(segOf('Directories', { type: 'all' }, false));
  if (state.selected.type === 'untagged') {
    parts.push('<span class="crumb-sep">/</span>', segOf('Untagged', { type: 'untagged' }, true));
  } else if (state.selected.type === 'path') {
    state.selected.path.forEach((seg, i) => {
      parts.push('<span class="crumb-sep">/</span>');
      parts.push(segOf(seg, { type: 'path', path: state.selected.path.slice(0, i + 1) }, i === state.selected.path.length - 1));
    });
  } else {
    parts.push('<span class="crumb-sep">/</span>', segOf('All files', { type: 'all' }, true));
  }
  el.innerHTML = parts.join('');
  el.querySelectorAll('[data-crumb]').forEach((b) => b.addEventListener('click', () => {
    state.selected = JSON.parse(b.dataset.crumb);
    revealSelection();
    renderTree(); renderFiles(); renderBreadcrumb();
  }));
}

function renderFiles() {
  const el = $('files');
  el.className = `files ${state.view}`;
  const docs = currentDocs();

  let heading = '';
  if (state.query.trim()) heading = `${docs.length} result(s) for “${escapeHtml(state.query.trim())}”`;

  if (!docs.length) {
    el.innerHTML = `<div class="empty"><span class="empty-ico">📂</span>${
      state.query.trim() ? 'No files match your search.' : 'This folder is empty. Import files, a folder, or a photo to begin.'
    }</div>`;
    return;
  }
  el.innerHTML = (heading ? `<div class="empty" style="grid-column:1/-1;text-align:left;padding-bottom:0"><b>${heading}</b></div>` : '') +
    docs.map((d) => docCard(d)).join('');
  lazyThumbs();
}

// Tags beyond this many are collapsed into a "+N" chip so cards keep a
// uniform height and the title always has room.
const MAX_VISIBLE_TAGS = 3;

function docCard(doc) {
  const open = state.expanded.has(doc.id);
  const first = (doc.items || [])[0];
  const iconHtml = first && first.kind === 'Image'
    ? `<img data-thumb="${first.blob}" alt="" />`
    : `<span>${first ? fileIcon(first.filename, first.kind).ico : '📄'}</span>`;

  const docTags = doc.tags || [];
  const shownTags = docTags.slice(0, MAX_VISIBLE_TAGS);
  const hiddenTags = docTags.length - shownTags.length;
  const tagChips = shownTags.map((t) =>
    `<button type="button" class="tag clickable" data-tag="${escapeHtml(t)}" title="Open folder ${escapeHtml(t)}">${escapeHtml(t)}</button>`).join('');
  const moreChip = hiddenTags > 0
    ? `<button type="button" class="tag tag-more" data-action="tags" title="Show ${hiddenTags} more tag(s)">+${hiddenTags}</button>`
    : '';
  const tagList = tagChips
    ? tagChips + moreChip
    : '<button type="button" class="tag tag-empty" data-action="tags" title="Add a tag">+ add tag</button>';

  const size = (doc.items || []).reduce((s, i) => s + (i.size || 0), 0);
  const parts = [shortDate(doc.updated_at), `${doc.items.length} file${doc.items.length === 1 ? '' : 's'}`];
  if (size) parts.push(humanSize(size));

  const title = (doc.title || '').trim() || 'Untitled';

  return `
    <article class="doc ${open ? 'expanded' : ''}" data-doc="${doc.id}">
      <div class="doc-main" data-action="toggle">
        <div class="doc-icon">${iconHtml}</div>
        <div class="doc-info">
          <div class="doc-title" title="${escapeHtml(title)}">${escapeHtml(title)}</div>
          <div class="doc-meta">
            <span class="doc-tags">${tagList}</span>
            <span class="doc-sub">${escapeHtml(parts.join(' · '))}</span>
          </div>
        </div>
      </div>
      <div class="doc-actions">
        <button type="button" class="mini" data-action="tags" title="Add or edit tags">Tags</button>
        <button type="button" class="mini" data-action="export" title="Export as bundle">Export</button>
        <button type="button" class="mini danger" data-action="del" title="Delete document">Delete</button>
      </div>
      ${open ? `<div class="doc-items">${(doc.items || []).map(itemsRow).join('') || '<div class="hint">No files.</div>'}</div>` : ''}
    </article>`;
}

function itemsRow(item) {
  const { ico, label } = fileIcon(item.filename, item.kind);
  const thumb = item.kind === 'Image' ? `<img data-thumb="${item.blob}" alt="" />` : `<span>${ico}</span>`;
  return `
    <div class="item" data-doc-item="${item.id}">
      <div class="item-thumb">${thumb}</div>
      <div class="item-info">
        <div class="item-name">${escapeHtml(item.filename)}</div>
        <div class="item-meta">
          <span class="badge">${escapeHtml(label)}</span>
          ${item.width && item.height ? `<span>${item.width}×${item.height}</span>` : ''}
          <span>${humanSize(item.size)}</span>
          ${item.ocr ? '<span class="badge ocr">OCR ✓</span>' : ''}
        </div>
      </div>
      <div class="item-actions">
        <button class="mini primary" data-action="open">Open</button>
        <button class="mini" data-action="openwith">Open with ▾</button>
        <button class="mini" data-action="reveal" title="Show in file manager">Reveal</button>
        ${item.kind === 'Image' ? '<button class="mini" data-action="ocr">OCR</button>' : ''}
      </div>
    </div>`;
}

function lazyThumbs() {
  document.querySelectorAll('img[data-thumb]').forEach((img) => {
    const hash = img.dataset.thumb;
    if (!hash) { img.remove(); return; }
    if (thumbCache.has(hash)) {
      const cached = thumbCache.get(hash);
      if (cached === null) img.outerHTML = fileIconFallback(img);
      else img.src = cached;
      return;
    }
    invoke('read_thumb', { hash })
      .then((data) => {
        if (data) { thumbCache.set(hash, data); img.src = data; }
        else { thumbCache.set(hash, null); img.outerHTML = fileIconFallback(img); }
      })
      .catch(() => { thumbCache.set(hash, null); img.outerHTML = fileIconFallback(img); });
  });
}

function fileIconFallback(img) {
  const big = Boolean(img.closest('.doc-icon'));
  return `<span>${big ? '📄' : '📎'}</span>`;
}

/* ------------------------------------------------------------ open logic */

function findIds(btn) {
  const row = btn.closest('.item');
  const art = btn.closest('.doc');
  return {
    docId: art ? art.getAttribute('data-doc') : null,
    itemId: row ? row.getAttribute('data-doc-item') : null,
  };
}

function findDocItem(docId, itemId) {
  const d = allDocs.find((x) => x.id === docId);
  if (!d) return null;
  return (d.items || []).find((i) => i.id === itemId) || null;
}

async function runOpen(docId, itemId, program) {
  try {
    await invoke('open_item', { doc: docId, item: itemId, program: program || null });
    status('Opened');
  } catch (e) {
    log(`Open failed: ${e}`);
    status('Open failed');
  }
}

async function openDefault(btn) {
  const { docId, itemId } = findIds(btn);
  const item = findDocItem(docId, itemId);
  if (!item) return;
  const ext = extOf(item.filename);
  const prog = lastProgByExt[ext] || null;
  await runOpen(docId, itemId, prog);
}

function closeOwMenu() {
  const m = $('owmenu');
  if (m) m.remove();
}

function openWithMenu(btn) {
  closeOwMenu();
  const { docId, itemId } = findIds(btn);
  const item = findDocItem(docId, itemId);
  if (!item) return;
  const ext = extOf(item.filename);
  const matching = programs.filter((p) => programMatches(p, ext));

  const menu = document.createElement('div');
  menu.id = 'owmenu';
  menu.className = 'owmenu';
  menu.innerHTML = `
    <div class="ow-title">Open ${escapeHtml(item.filename)}</div>
    ${matching.length ? matching.map((p) => `
      <button data-ow="${escapeHtml(p.command)}">
        <span class="ow-ico">🧩</span>
        <span class="ow-item">
          <span class="ow-label">${escapeHtml(p.name)}</span>
          <span class="ow-desc">${escapeHtml(p.command)}</span>
        </span>
      </button>`).join('') : `<div class="hint" style="padding:6px 9px">No registered programs match “.${escapeHtml(ext)}”. Add one under “Open files with…”.</div>`}
    <div class="ow-sep"></div>
    <button data-ow-system><span class="ow-ico">🖥️</span> Open with system default</button>
    <button data-ow-reveal><span class="ow-ico">📁</span> Reveal in file manager</button>
    <div class="ow-sep"></div>
    <div class="ow-input-row">
      <input type="text" placeholder="or type a command (e.g. gimp {file})" />
      <button data-ow-adhoc>Open</button>
    </div>`;
  document.body.appendChild(menu);

  const rect = btn.getBoundingClientRect();
  const mw = Math.min(menu.offsetWidth, 320);
  let left = Math.max(8, Math.min(rect.right - mw, window.innerWidth - mw - 8));
  let top = rect.bottom + 6;
  if (top + 220 > window.innerHeight) top = Math.max(8, rect.top - 230);
  menu.style.left = `${left}px`;
  menu.style.top = `${top}px`;
  menu.classList.add('show');

  menu.querySelectorAll('[data-ow]').forEach((b) => b.addEventListener('click', async () => {
    const cmd = b.dataset.ow;
    if (ext) lastProgByExt[ext] = cmd;
    saveLastProg();
    closeOwMenu();
    log(`Opening with: ${cmd}`);
    await runOpen(docId, itemId, cmd);
  }));
  menu.querySelector('[data-ow-system]').addEventListener('click', async () => {
    if (ext) delete lastProgByExt[ext];
    saveLastProg();
    closeOwMenu();
    await runOpen(docId, itemId, null);
  });
  menu.querySelector('[data-ow-reveal]').addEventListener('click', async () => {
    closeOwMenu();
    try { await invoke('reveal_item', { doc: docId, item: itemId }); status('Revealed'); }
    catch (e) { log(`Reveal failed: ${e}`); }
  });
  const adhocInput = menu.querySelector('.ow-input-row input');
  const doAdhoc = async () => {
    const cmd = adhocInput.value.trim();
    if (!cmd) return;
    closeOwMenu();
    log(`Opening with: ${cmd}`);
    await runOpen(docId, itemId, cmd);
  };
  menu.querySelector('[data-ow-adhoc]').addEventListener('click', doAdhoc);
  adhocInput.addEventListener('keydown', (e) => { if (e.key === 'Enter') doAdhoc(); });

  setTimeout(() => {
    document.addEventListener('click', closeOwMenuOnOutside, { once: true });
  }, 0);
}

function closeOwMenuOnOutside(e) {
  const m = $('owmenu');
  if (m && !m.contains(e.target)) closeOwMenu();
}

document.addEventListener('keydown', (e) => { if (e.key === 'Escape') { closeOwMenu(); closeTagEditor(); } });

/* ------------------------------------------------- central click routing */

$('files').addEventListener('click', async (e) => {
  const tagBtn = e.target.closest('[data-tag]');
  if (tagBtn) {
    state.selected = { type: 'path', path: tagBtn.dataset.tag.split('/') };
    state.query = '';
    $('search').value = '';
    revealSelection();
    renderTree(); renderFiles(); renderBreadcrumb();
    return;
  }

  const btn = e.target.closest('[data-action]');
  if (!btn) return;
  const action = btn.getAttribute('data-action');
  const { docId, itemId } = findIds(btn);

  if (action === 'toggle') {
    state.expanded.has(docId) ? state.expanded.delete(docId) : state.expanded.add(docId);
    renderFiles();
    return;
  }
  if (action === 'open') { await openDefault(btn); return; }
  if (action === 'openwith') { openWithMenu(btn); return; }
  if (action === 'reveal') {
    try { await invoke('reveal_item', { doc: docId, item: itemId }); status('Revealed'); }
    catch (err) { log(`Reveal failed: ${err}`); }
    return;
  }
  if (action === 'ocr') {
    try {
      status('Running OCR…');
      const text = await invoke('run_ocr', { doc: docId, item: itemId, language: $('ocr-lang').value.trim() || 'eng' });
      log(`OCR: ${text.slice(0, 120)}`);
      await refresh();
      status('Ready');
    } catch (err) { log(`OCR failed: ${err}`); status('Ready'); }
    return;
  }
  if (action === 'del') {
    try { await invoke('remove_document', { id: docId }); state.expanded.delete(docId); await refresh(); log('Document deleted'); }
    catch (err) { log(`Delete failed: ${err}`); }
    return;
  }
  if (action === 'tags') {
    const doc = allDocs.find((d) => d.id === docId);
    if (doc) openTagEditor(btn, doc.id, doc.tags || []);
    return;
  }
  if (action === 'export') {
    try {
      const result = await guarded(() => invoke('export_bundle', {
        name: $('bundle-name').value.trim() || 'panarchiv-export.parbundle',
        ids: [docId], all: false, tag: null,
      }));
      log(result);
    } catch (err) { log(`Export failed: ${err}`); }
  }
});

/* ------------------------------------------------------------- tree nav */

$('tree').addEventListener('click', (e) => {
  const toggle = e.target.closest('[data-tree-toggle]');
  if (toggle) {
    e.stopPropagation();
    const key = toggle.getAttribute('data-path');
    state.expandedPaths.has(key) ? state.expandedPaths.delete(key) : state.expandedPaths.add(key);
    if (state.selected.type === 'path' && JSON.stringify(state.selected.path) === key) revealSelection();
    else renderTree();
    return;
  }
  const sel = e.target.closest('[data-tree-select]');
  if (!sel) return;
  if (sel.dataset.type === 'all') state.selected = { type: 'all' };
  else if (sel.dataset.type === 'untagged') state.selected = { type: 'untagged' };
  else state.selected = { type: 'path', path: JSON.parse(sel.getAttribute('data-path')) };
  state.query = '';
  $('search').value = '';
  revealSelection();
  renderTree();
  renderFiles();
  renderBreadcrumb();
});

$('nav-all').addEventListener('click', () => {
  state.selected = { type: 'all' };
  state.query = ''; $('search').value = '';
  revealSelection(); renderTree(); renderFiles(); renderBreadcrumb();
});

// Make sure ancestors of the selected path are expanded.
function revealSelection() {
  if (state.selected.type !== 'path') return;
  const { path } = state.selected;
  for (let i = 1; i <= path.length; i += 1) {
    state.expandedPaths.add(JSON.stringify(path.slice(0, i)));
  }
}

/* --------------------------------------------------------------- toolbar */

let searchTimer;
$('search').addEventListener('input', () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(() => { state.query = $('search').value; renderFiles(); }, 200);
});

$('sort').addEventListener('change', (e) => { state.sort = e.target.value; renderFiles(); });

function syncViewButtons() {
  $('view-grid').classList.toggle('active', state.view === 'grid');
  $('view-list').classList.toggle('active', state.view === 'list');
}
$('view-grid').addEventListener('click', () => { state.view = 'grid'; syncViewButtons(); renderFiles(); });
$('view-list').addEventListener('click', () => { state.view = 'list'; syncViewButtons(); renderFiles(); });

$('toggle-left').addEventListener('click', () => document.body.classList.toggle('show-left'));
$('toggle-right').addEventListener('click', () => document.body.classList.toggle('show-right'));

$('btn-import-top').addEventListener('click', () => $('file-input').click());

/* ------------------------------------------------------------- accordion */

document.querySelectorAll('.tool-head').forEach((h) => h.addEventListener('click', () => {
  const tool = h.closest('.tool');
  const open = tool.classList.toggle('open');
  const chev = h.querySelector('.chev');
  if (chev) chev.innerHTML = open ? '▾' : '▸';
}));

/* --------------------------------------------------------------- import */

async function importSelected(input, suggestedTitle) {
  if (!input.files || !input.files.length) return;
  try {
    status('Importing...');
    const files = await payloads(input.files);
    const count = await guarded(() => invoke('import_files', {
      files,
      title: $('import-title').value.trim() || suggestedTitle || null,
      tags: splitTags($('import-tags').value),
      group: $('import-group').checked,
      runOcr: $('import-ocr').checked,
      language: $('ocr-lang').value.trim() || 'eng',
    }));
    log(`Imported ${count} document(s)`);
    status(`${count} imported`);
    await refresh();
  } catch (e) {
    if (/cancelled/i.test(String(e))) return;
    log(`Import failed: ${e}`);
    status('Import failed');
  } finally {
    input.value = '';
  }
}

$('btn-files').addEventListener('click', () => $('file-input').click());
$('file-input').addEventListener('change', (e) => importSelected(e.target));
$('btn-folder').addEventListener('click', () => $('folder-input').click());
$('folder-input').addEventListener('change', (e) => {
  const first = e.target.files[0];
  const folder = first ? (first.webkitRelativePath.split('/')[0] || 'Folder') : 'Folder';
  importSelected(e.target, folder);
});
$('btn-camera').addEventListener('click', () => $('camera-input').click());
$('camera-input').addEventListener('change', (e) => importSelected(e.target, 'Camera capture'));

/* --------------------------------------------------------- programs UI */

$('prog-add').addEventListener('click', () => {
  const name = $('prog-name').value.trim();
  const command = $('prog-cmd').value.trim();
  if (!name || !command) { log('Add both a name and a command'); return; }
  programs.push({ name, command, exts: normExts($('prog-exts').value) });
  savePrograms();
  $('prog-name').value = '';
  $('prog-cmd').value = '';
  $('prog-exts').value = '';
  log('Added program');
});

$('programs-reset').addEventListener('click', () => {
  programs = EXAMPLE_PROGRAMS.map((p) => ({ name: p.name, command: p.command, exts: p.exts.slice() }));
  savePrograms();
  log('Reset programs to examples');
});

$('log-clear').addEventListener('click', () => { $('log').textContent = ''; });

/* ------------------------------------------------------------- peer 2 p2p */

$('btn-start').addEventListener('click', async () => {
  try {
    const result = await invoke('start_server', {
      port: Number($('peer-port').value),
      name: $('peer-name').value.trim() || 'panarchiv',
    });
    log(result);
    status('Sharing active', true);
    $('btn-start').disabled = true;
    $('btn-stop').disabled = false;
  } catch (e) { log(`Start failed: ${e}`); }
});

$('btn-stop').addEventListener('click', async () => {
  try {
    log(await invoke('stop_server'));
    status('Ready');
    $('btn-start').disabled = false;
    $('btn-stop').disabled = true;
  } catch (e) { log(`Stop failed: ${e}`); }
});

$('btn-discover').addEventListener('click', async () => {
  try {
    const peers = await invoke('discover_peers', {
      port: Number($('peer-port').value), seconds: 3,
    });
    const root = $('peers');
    root.innerHTML = peers.length
      ? peers.map((p) => {
          const [host, port] = p.addr.split(':');
          return `<div class="peer" data-host="${escapeHtml(host)}" data-port="${escapeHtml(port)}">${escapeHtml(p.name)} · ${escapeHtml(p.addr)}</div>`;
        }).join('')
      : '<span class="hint">No peers found</span>';
    root.querySelectorAll('.peer').forEach((el) => el.addEventListener('click', () => {
      $('send-host').value = el.dataset.host;
      $('send-port').value = el.dataset.port;
    }));
    log(`Found ${peers.length} peer(s)`);
  } catch (e) { log(`Discovery failed: ${e}`); }
});

$('btn-send').addEventListener('click', () => $('send-input').click());
$('send-input').addEventListener('change', async (e) => {
  if (!e.target.files.length) return;
  try {
    const files = await payloads(e.target.files);
    const result = await invoke('send_to_peer', {
      host: $('send-host').value.trim(),
      port: Number($('send-port').value),
      name: $('peer-name').value.trim() || 'panarchiv',
      files,
    });
    log(result);
  } catch (err) { log(`Send failed: ${err}`); }
  finally { e.target.value = ''; }
});

/* --------------------------------------------------------------- bundles */

$('btn-export').addEventListener('click', async () => {
  try {
    log(await guarded(() => invoke('export_bundle', {
      name: $('bundle-name').value.trim() || 'panarchiv-export.parbundle',
      ids: [], all: $('bundle-all').checked, tag: null,
    })));
  } catch (e) { log(`Export failed: ${e}`); }
});

$('btn-import-bundle').addEventListener('click', () => $('bundle-input').click());
$('bundle-input').addEventListener('change', async (e) => {
  const file = e.target.files[0];
  if (!file) return;
  try {
    const bytes = Array.from(new Uint8Array(await file.arrayBuffer()));
    const count = await guarded(() => invoke('import_bundle', { name: file.name, bytes }));
    log(`Imported ${count} document(s) from bundle`);
    await refresh();
  } catch (err) {
    if (!/cancelled/i.test(String(err))) log(`Bundle import failed: ${err}`);
  }
  finally { e.target.value = ''; }
});

/* --------------------------------------------------------- unlock dialog */

// The archive is encrypted, so a passphrase-protected archive cannot be
// opened until the user supplies the passphrase. Every command that needs
// the archive goes through `guarded`, which opens this dialog when the
// backend reports that a passphrase is required.
let unlockResolver = null;

function isPassphraseError(e) {
  const msg = String(e || '').toLowerCase();
  return msg.includes('passphrase');
}

function showUnlockDialog(message) {
  const modal = $('unlock');
  if (!modal) return Promise.reject(new Error(message || 'Passphrase required'));
  modal.hidden = false;
  const err = $('unlock-error');
  if (err) {
    err.hidden = !message;
    err.textContent = message || '';
  }
  const input = $('unlock-passphrase');
  if (input) { input.value = ''; input.focus(); }
  return new Promise((resolve, reject) => { unlockResolver = { resolve, reject }; });
}

function closeUnlockDialog() {
  const modal = $('unlock');
  if (modal) modal.hidden = true;
}

async function submitUnlock() {
  const input = $('unlock-passphrase');
  const passphrase = input ? input.value : '';
  const err = $('unlock-error');
  try {
    await invoke('unlock_archive', { passphrase });
    closeUnlockDialog();
    log('Archive unlocked');
    await refresh();
    if (unlockResolver) unlockResolver.resolve();
    unlockResolver = null;
  } catch (e) {
    if (err) { err.hidden = false; err.textContent = String(e); }
    log(`Unlock failed: ${e}`);
  }
}

/**
 * Run a backend command, opening the unlock dialog if the archive turns
 * out to be passphrase protected.
 */
async function guarded(fn) {
  try {
    return await fn();
  } catch (e) {
    if (isPassphraseError(e)) {
      await showUnlockDialog(String(e));
      return fn();
    }
    throw e;
  }
}

$('unlock-ok').addEventListener('click', submitUnlock);
$('unlock-cancel').addEventListener('click', () => {
  closeUnlockDialog();
  if (unlockResolver) unlockResolver.reject(new Error('Unlock cancelled'));
  unlockResolver = null;
});
$('unlock-passphrase').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') submitUnlock();
});

/* ------------------------------------------------------------- tag helpers */

// Every tag used in the archive, for the datalists in the sync and share
// panels. Sorted and de-duplicated.
function allTags() {
  const set = new Set();
  for (const doc of allDocs) {
    for (const tag of (doc.tags || [])) {
      const t = String(tag).trim();
      if (t) set.add(t);
    }
  }
  return [...set].sort((a, b) => a.localeCompare(b));
}

function refreshTagLists() {
  const tags = allTags();
  const options = tags.map((t) => `<option value="${escapeHtml(t)}"></option>`).join('');
  ['sync-tag-options', 'share-tag-options'].forEach((id) => {
    const el = $(id);
    if (el) el.innerHTML = options;
  });
}

/* --------------------------------------------------------------- device sync */

function showResult(id, message, kind) {
  const el = $(id);
  if (!el) return;
  el.hidden = false;
  el.className = `result ${kind || ''}`.trim();
  el.textContent = message;
}

$('btn-sync').addEventListener('click', async () => {
  const host = $('sync-host').value.trim();
  if (!host) {
    showResult('sync-result', 'Enter the address of the other device.', 'error');
    $('sync-host').focus();
    return;
  }
  const btn = $('btn-sync');
  btn.disabled = true;
  showResult('sync-result', 'Syncing…', '');
  try {
    status('Syncing…');
    const result = await guarded(() => invoke('sync_with_peer', {
      host,
      port: Number($('sync-port').value) || 4242,
      name: $('peer-name').value.trim() || 'panarchiv',
      preferNewest: $('sync-prefer-newest').checked,
      syncTags: $('sync-tags').checked,
      tag: $('sync-tag').value.trim() || null,
    }));
    const msg = `Synced with ${result.peer}: ${result.received} file(s) received, ${result.sent} sent, ${result.tags_received} with tags`;
    showResult('sync-result', msg, 'ok');
    log(msg);
    status('Sync complete');
    await refresh();
  } catch (e) {
    showResult('sync-result', `Sync failed: ${e}`, 'error');
    log(`Sync failed: ${e}`);
    status('Sync failed');
  } finally {
    btn.disabled = false;
  }
});

$('btn-sync-discover').addEventListener('click', async () => {
  try {
    const peers = await invoke('discover_peers', {
      port: Number($('sync-port').value) || 4242, seconds: 3,
    });
    const root = $('sync-peers');
    root.innerHTML = peers.length
      ? peers.map((p) => {
          const [host, port] = p.addr.split(':');
          return `<div class="peer" data-host="${escapeHtml(host)}" data-port="${escapeHtml(port)}">${escapeHtml(p.name)} · ${escapeHtml(p.addr)}</div>`;
        }).join('')
      : '<span class="hint">No devices found. Make sure the other device has “Sharing” switched on.</span>';
    root.querySelectorAll('.peer').forEach((el) => el.addEventListener('click', () => {
      $('sync-host').value = el.dataset.host;
      $('sync-port').value = el.dataset.port;
      $('share-tag-host').value = el.dataset.host;
      $('share-tag-port').value = el.dataset.port;
    }));
    log(`Found ${peers.length} device(s)`);
  } catch (e) { log(`Discovery failed: ${e}`); }
});

/* ------------------------------------------------------------ share a tag */

$('btn-share-tag-bundle').addEventListener('click', async () => {
  const tag = $('share-tag').value.trim();
  if (!tag) {
    showResult('share-result', 'Choose a tag to share.', 'error');
    $('share-tag').focus();
    return;
  }
  try {
    const name = $('share-tag-bundle-name').value.trim() || 'panarchiv-tag.parbundle';
    const result = await guarded(() => invoke('export_bundle', {
      name, ids: [], all: false, tag,
    }));
    showResult('share-result', result, 'ok');
    log(result);
  } catch (e) {
    showResult('share-result', `Share failed: ${e}`, 'error');
    log(`Share failed: ${e}`);
  }
});

$('btn-share-tag-peer').addEventListener('click', async () => {
  const tag = $('share-tag').value.trim();
  const host = $('share-tag-host').value.trim();
  if (!tag) {
    showResult('share-result', 'Choose a tag to share.', 'error');
    $('share-tag').focus();
    return;
  }
  if (!host) {
    showResult('share-result', 'Enter the address of the other device.', 'error');
    $('share-tag-host').focus();
    return;
  }
  const btn = $('btn-share-tag-peer');
  btn.disabled = true;
  showResult('share-result', `Sharing “${tag}”…`, '');
  try {
    const result = await guarded(() => invoke('share_tag_with_peer', {
      host,
      port: Number($('share-tag-port').value) || 4242,
      name: $('peer-name').value.trim() || 'panarchiv',
      tag,
    }));
    const msg = `Shared “${tag}” with ${result.peer}: ${result.sent} file(s) sent`;
    showResult('share-result', msg, 'ok');
    log(msg);
    status('Tag shared');
  } catch (e) {
    showResult('share-result', `Share failed: ${e}`, 'error');
    log(`Share failed: ${e}`);
    status('Share failed');
  } finally {
    btn.disabled = false;
  }
});

/* ------------------------------------------------- encryption & storage */

$('btn-storage-info').addEventListener('click', async () => {
  try {
    const info = await guarded(() => invoke('archive_info'));
    archiveInfo = info;
    const el = $('archive-info');
    el.hidden = false;
    const tagLine = info.tags.length
      ? info.tags.map((t) => `${escapeHtml(t.tag)} (${t.count})`).join(', ')
      : 'no tags yet';
    el.innerHTML = `
      <div><b>Location</b><br /><code>${escapeHtml(info.root)}</code></div>
      <div><b>Documents</b> ${info.document_count}</div>
      <div><b>Passphrase</b> ${info.passphrase_protected ? 'enabled' : 'not set'}</div>
      <div><b>Tags</b> ${tagLine}</div>`;
    refreshTagLists();
  } catch (e) {
    log(`Could not read the archive info: ${e}`);
  }
});

$('btn-set-passphrase').addEventListener('click', async () => {
  const input = $('archive-passphrase');
  const passphrase = input ? input.value : '';
  if (!passphrase) {
    log('Enter a passphrase first');
    if (input) input.focus();
    return;
  }
  try {
    const result = await guarded(() => invoke('set_passphrase', { passphrase }));
    log(result);
    if (input) input.value = '';
    $('archive-info').hidden = true;
  } catch (e) {
    log(`Could not set the passphrase: ${e}`);
  }
});

$('btn-remove-passphrase').addEventListener('click', async () => {
  try {
    const result = await guarded(() => invoke('set_passphrase', { passphrase: null }));
    log(result);
    $('archive-info').hidden = true;
  } catch (e) {
    log(`Could not remove the passphrase: ${e}`);
  }
});

/* --------------------------------------------------------------- refresh */

async function refresh() {
  try {
    const docs = await guarded(() => invoke('get_documents'));
    allDocs = docs || [];
    renderTree();
    renderFiles();
    renderBreadcrumb();
    refreshTagLists();
  } catch (e) {
    if (!isPassphraseError(e) && !/cancelled/i.test(String(e))) {
      log(`Load failed: ${e}`);
    }
  }
}

/* -------------------------------------------------------------- bootstrap */

loadState();
renderPrograms();
syncViewButtons();
$('sort').value = state.sort;
initTheme();
$('theme-toggle').addEventListener('click', () => {
  const current = currentThemeMode();
  const next = THEME_MODES[(THEME_MODES.indexOf(current) + 1) % THEME_MODES.length];
  applyTheme(next);
});
refresh();