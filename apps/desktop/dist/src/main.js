// Thin wrapper around the Tauri IPC bridge injected into the WebView.
// Using the injected bridge directly keeps the frontend dependency-free.
async function invoke(cmd, args = {}) {
  const bridge = window.__TAURI_INTERNALS__;
  if (!bridge || typeof bridge.invoke !== 'function') {
    throw new Error('Panarchiv must be running inside the Tauri app');
  }
  return bridge.invoke(cmd, args);
}

const $ = (id) => document.getElementById(id);

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

async function payloads(fileList) {
  return Promise.all(Array.from(fileList).map(async (f) => ({
    name: f.webkitRelativePath || f.name,
    bytes: Array.from(new Uint8Array(await f.arrayBuffer())),
  })));
}

function splitTags(value) {
  return value.split(',').map((t) => t.trim()).filter(Boolean);
}

async function refresh() {
  const query = $('search').value.trim();
  try {
    const docs = query
      ? await invoke('search_documents', { query })
      : await invoke('get_documents');
    render(docs);
  } catch (e) {
    log(`Load failed: ${e}`);
  }
}

function render(docs) {
  const root = $('documents');
  if (!docs.length) {
    root.innerHTML = '<div class="empty">No documents yet. Import files, a folder, or a photo to begin.</div>';
    return;
  }
  root.innerHTML = docs.map((doc) => {
    const tags = doc.tags.map((t) => `<span class="tag">${escapeHtml(t)}</span>`).join('');
    const items = doc.items.map((item) => `
      <div class="item">
        <div>
          <div class="item-name">${escapeHtml(item.filename)}</div>
          <div class="ocr">${escapeHtml(item.ocr ? item.ocr.text : 'No OCR text')}</div>
        </div>
        <div style="margin-left:auto; display:flex; gap:6px">
          ${item.kind === 'Image' ? `<button class="mini" data-ocr="${doc.id}|${item.id}">OCR</button>` : ''}
          <button class="mini danger" data-delete="${doc.id}">Delete</button>
        </div>
      </div>`).join('');
    return `
      <div class="doc">
        <div class="doc-head">
          <div>
            <div class="doc-title">${escapeHtml(doc.title)}</div>
            <div class="doc-meta">${tags} ${doc.updated_at}</div>
          </div>
          <button class="mini" data-export="${doc.id}">Export bundle</button>
        </div>
        ${items}
      </div>`;
  }).join('');

  root.querySelectorAll('[data-ocr]').forEach((btn) => btn.addEventListener('click', async () => {
    const [doc, item] = btn.dataset.ocr.split('|');
    try {
      const text = await invoke('run_ocr', { doc, item, language: $('ocr-lang').value.trim() || 'eng' });
      log(`OCR: ${text.slice(0, 120)}`);
      await refresh();
    } catch (e) { log(`OCR failed: ${e}`); }
  }));
  root.querySelectorAll('[data-delete]').forEach((btn) => btn.addEventListener('click', async () => {
    try {
      await invoke('remove_document', { id: btn.dataset.delete });
      await refresh();
    } catch (e) { log(`Delete failed: ${e}`); }
  }));
  root.querySelectorAll('[data-export]').forEach((btn) => btn.addEventListener('click', async () => {
    try {
      const result = await invoke('export_bundle', {
        name: $('bundle-name').value.trim() || 'panarchiv-export.parbundle',
        ids: [btn.dataset.export],
        all: false,
      });
      log(result);
    } catch (e) { log(`Export failed: ${e}`); }
  }));
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  }[c]));
}

async function importSelected(input, suggestedTitle) {
  if (!input.files || !input.files.length) return;
  try {
    status('Importing...');
    const files = await payloads(input.files);
    const count = await invoke('import_files', {
      files,
      title: $('import-title').value.trim() || suggestedTitle || null,
      tags: splitTags($('import-tags').value),
      group: $('import-group').checked,
      runOcr: $('import-ocr').checked,
      language: $('ocr-lang').value.trim() || 'eng',
    });
    log(`Imported ${count} document(s)`);
    status(`${count} imported`);
    await refresh();
  } catch (e) {
    log(`Import failed: ${e}`);
    status('Import failed');
  } finally {
    input.value = '';
  }
}

$('btn-files').onclick = () => $('file-input').click();
$('file-input').onchange = (e) => importSelected(e.target);
$('btn-folder').onclick = () => $('folder-input').click();
$('folder-input').onchange = (e) => {
  const first = e.target.files[0];
  const folder = first ? (first.webkitRelativePath.split('/')[0] || 'Folder') : 'Folder';
  importSelected(e.target, folder);
};
$('btn-camera').onclick = () => $('camera-input').click();
$('camera-input').onchange = (e) => importSelected(e.target, 'Camera capture');

let searchTimer;
$('search').oninput = () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(refresh, 250);
};

$('btn-start').onclick = async () => {
  try {
    const result = await invoke('start_server', {
      port: Number($('peer-port').value), name: $('peer-name').value.trim() || 'panarchiv',
    });
    log(result);
    status('Sharing active', true);
    $('btn-start').disabled = true;
    $('btn-stop').disabled = false;
  } catch (e) { log(`Start failed: ${e}`); }
};

$('btn-stop').onclick = async () => {
  try {
    log(await invoke('stop_server'));
    status('Ready');
    $('btn-start').disabled = false;
    $('btn-stop').disabled = true;
  } catch (e) { log(`Stop failed: ${e}`); }
};

$('btn-discover').onclick = async () => {
  try {
    const peers = await invoke('discover_peers', {
      port: Number($('peer-port').value), seconds: 3,
    });
    const root = $('peers');
    root.innerHTML = peers.length
      ? peers.map((p) => `<div class="peer" data-host="${escapeHtml(p.addr.split(':')[0])}" data-port="${escapeHtml(p.addr.split(':').pop())}">${escapeHtml(p.name)} · ${escapeHtml(p.addr)}</div>`).join('')
      : '<span class="hint">No peers found</span>';
    root.querySelectorAll('.peer').forEach((el) => el.onclick = () => {
      $('send-host').value = el.dataset.host;
      $('send-port').value = el.dataset.port;
    });
    log(`Found ${peers.length} peer(s)`);
  } catch (e) { log(`Discovery failed: ${e}`); }
};

$('btn-send').onclick = () => $('send-input').click();
$('send-input').onchange = async (e) => {
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
};

$('btn-export').onclick = async () => {
  try {
    log(await invoke('export_bundle', {
      name: $('bundle-name').value.trim() || 'panarchiv-export.parbundle',
      ids: [], all: $('bundle-all').checked,
    }));
  } catch (e) { log(`Export failed: ${e}`); }
};

$('btn-import-bundle').onclick = () => $('bundle-input').click();
$('bundle-input').onchange = async (e) => {
  const file = e.target.files[0];
  if (!file) return;
  try {
    const bytes = Array.from(new Uint8Array(await file.arrayBuffer()));
    const count = await invoke('import_bundle', { name: file.name, bytes });
    log(`Imported ${count} document(s) from bundle`);
    await refresh();
  } catch (err) { log(`Bundle import failed: ${err}`); }
  finally { e.target.value = ''; }
};

refresh();
