const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const rows = new Map();
const selected = new Set();
const expanded = new Set(); // torrents whose file list is open
const MAX_FILES_SHOWN = 2000;
let torrents = [];
let anchor = null; // last clicked row, for shift-click ranges

function bytes(n) {
  if (n == null) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1000 && i < units.length - 1) { n /= 1000; i++; }
  return `${n.toFixed(i === 0 || n >= 100 ? 0 : 1)} ${units[i]}`;
}

function eta(s) {
  if (s == null) return "—";
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  const h = Math.floor(s / 3600);
  if (h < 48) return `${h}h ${Math.floor((s % 3600) / 60)}m`;
  return `${Math.round(h / 24)}d`;
}

const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;

function toast(msg) {
  const t = $("toast");
  t.textContent = String(msg);
  t.hidden = false;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => (t.hidden = true), 5000);
}

async function call(cmd, args) {
  try {
    return await invoke(cmd, args);
  } catch (e) {
    toast(e);
    throw e;
  }
}

const visibleTorrents = () => torrents;

/* Rows */

function createRow(id) {
  const item = $("row-tpl").content.firstElementChild.cloneNode(true);
  const el = item.querySelector(".row");
  el.addEventListener("click", (e) => {
    if (e.target.closest(".act, .c-check, .disclosure")) return;
    clickRow(id, e);
  });
  el.addEventListener("dblclick", (e) => {
    if (e.target.closest(".act, .c-check, .disclosure")) return;
    const t = torrents.find((x) => x.id === id);
    call(t?.phase === "completed" && t.has_video ? "play" : "reveal", { id }).catch(() => {});
  });
  el.querySelector(".check").addEventListener("change", (e) => {
    e.target.checked ? selected.add(id) : selected.delete(id);
    anchor = id;
    render();
  });
  el.querySelector(".play").addEventListener("click", () => call("play", { id }).catch(() => {}));
  el.querySelector(".start").addEventListener("click", () => start(id));
  el.querySelector(".pause").addEventListener("click", () => pause(id));
  el.querySelector(".delete").addEventListener("click", () => confirmDelete([id]));
  el.querySelector(".disclosure").addEventListener("click", () => {
    expanded.has(id) ? expanded.delete(id) : expanded.add(id);
    render();
    if (expanded.has(id)) loadFiles(id);
  });
  return item;
}

/* File lists */

async function loadFiles(id) {
  try {
    const files = await invoke("list_files", { id });
    const item = rows.get(id);
    if (item && expanded.has(id)) renderFiles(id, item.querySelector(".files"), files);
  } catch (e) {
    toast(e);
  }
}

function createFileRow(id, index) {
  const el = $("file-tpl").content.firstElementChild.cloneNode(true);
  el.querySelector(".play").addEventListener("click", () => call("open_file", { id, index }).catch(() => {}));
  el.addEventListener("dblclick", (e) => {
    if (e.target.closest(".act")) return;
    const done = el.dataset.done === "1";
    call(done ? "open_file" : "reveal_file", { id, index }).catch(() => {});
  });
  return el;
}

function renderFiles(id, container, files) {
  const shown = files.slice(0, MAX_FILES_SHOWN);
  while (container.children.length > shown.length) container.lastElementChild.remove();
  shown.forEach((f, i) => {
    let el = container.children[i];
    if (!el || el.dataset.index !== String(f.index)) {
      const fresh = createFileRow(id, f.index);
      fresh.dataset.index = String(f.index);
      el ? container.replaceChild(fresh, el) : container.append(fresh);
      el = fresh;
    }
    const slash = f.path.lastIndexOf("/");
    set(el, ".dir", slash >= 0 ? f.path.slice(0, slash + 1) : "");
    set(el, ".base", f.path.slice(slash + 1));
    el.querySelector(".fname").title = f.path;
    set(el, ".c-size", bytes(f.length));
    const progress = f.length ? f.downloaded_bytes / f.length : 1;
    const done = f.downloaded_bytes === f.length;
    el.dataset.done = done ? "1" : "0";
    el.classList.toggle("done", done);
    el.querySelector(".fill").style.width = `${(progress * 100).toFixed(2)}%`;
    set(el, ".pct", `${Math.floor(progress * 100)}%`);
    el.querySelector(".play").hidden = !(done && f.is_video);
  });
  if (files.length > shown.length) {
    const more = document.createElement("div");
    more.className = "files-more";
    more.textContent = `${files.length - shown.length} more files`;
    container.append(more);
  }
}

function clickRow(id, e) {
  const visible = visibleTorrents();
  if (e.shiftKey && anchor) {
    const a = visible.findIndex((t) => t.id === anchor);
    const b = visible.findIndex((t) => t.id === id);
    if (a >= 0 && b >= 0) {
      for (const t of visible.slice(Math.min(a, b), Math.max(a, b) + 1)) selected.add(t.id);
    }
  } else if (e.metaKey) {
    selected.has(id) ? selected.delete(id) : selected.add(id);
    anchor = id;
  } else {
    selected.clear();
    selected.add(id);
    anchor = id;
  }
  render();
}

function set(el, sel, text) {
  const node = el.querySelector(sel);
  if (node.textContent !== text) node.textContent = text;
}

function updateRow(item, t) {
  const el = item.querySelector(".row");
  const multi = t.file_count > 1;
  const open = multi && expanded.has(t.id);
  const disclosure = el.querySelector(".disclosure");
  disclosure.hidden = !multi;
  disclosure.setAttribute("aria-expanded", String(open));
  disclosure.title = open ? "Hide files" : `Show ${t.file_count} files`;
  item.classList.toggle("expanded", open);
  item.querySelector(".files").hidden = !open;
  const cls = `row ${t.phase}${selected.has(t.id) ? " selected" : ""}`;
  if (el.className !== cls) el.className = cls;
  el.querySelector(".check").checked = selected.has(t.id);
  set(el, ".name", t.name);
  el.querySelector(".name").title = t.name;
  // Only errors get a second line; normal progress is already in the columns.
  set(el, ".sub", t.phase === "error" ? t.error || "Something went wrong" : "");
  set(el, ".c-size", bytes(t.total_bytes));
  set(el, ".pct", t.phase === "fetching_metadata" ? "" : `${Math.floor(t.progress * 100)}%`);
  el.querySelector(".fill").style.width = `${(t.progress * 100).toFixed(2)}%`;

  const live = (t.phase === "downloading" || t.phase === "fetching_metadata") && t.download_rate > 0;
  const speed = el.querySelector(".c-speed");
  speed.textContent = live ? `${bytes(t.download_rate)}/s` : "—";
  speed.classList.toggle("live", live);
  set(el, ".c-eta", t.phase === "completed" ? "Done" : eta(t.eta_secs));

  const seeking = t.phase === "downloading" || t.phase === "fetching_metadata";
  const seeds = el.querySelector(".c-seeds");
  seeds.textContent = seeking ? `${t.seeds} / ${t.swarm_seeds ?? "?"}` : "—";
  seeds.title = seeking
    ? `Connected to ${plural(t.seeds, "seed")}` +
      (t.swarm_seeds != null ? ` · ${plural(t.swarm_seeds, "seed")} and ${plural(t.swarm_leechers ?? 0, "downloader")} in the swarm` : "")
    : "";

  el.querySelector(".play").hidden = !(t.phase === "completed" && t.has_video);

  // Start and Pause share one spot: whichever applies is shown.
  const done = t.phase === "completed";
  const stopped = t.phase === "paused" || t.phase === "error";
  el.querySelector(".start").hidden = done || !stopped;
  el.querySelector(".pause").hidden = done || stopped;
}

function render() {
  // Forget selections for torrents that no longer exist.
  for (const id of selected) if (!torrents.some((t) => t.id === id)) selected.delete(id);
  for (const id of expanded) if (!torrents.some((t) => t.id === id)) expanded.delete(id);

  const visible = visibleTorrents();
  const container = $("list");
  const seen = new Set();
  visible.forEach((t, i) => {
    seen.add(t.id);
    if (!rows.has(t.id)) rows.set(t.id, createRow(t.id));
    const el = rows.get(t.id);
    updateRow(el, t);
    if (container.children[i] !== el) container.insertBefore(el, container.children[i] || null);
  });
  for (const [id, el] of rows) {
    if (!seen.has(id)) {
      el.remove();
      if (!torrents.some((t) => t.id === id)) rows.delete(id);
    }
  }

  const empty = visible.length === 0;
  $("empty").hidden = !empty;
  container.hidden = empty;
  $("columns").hidden = empty;

  // Selection controls
  const visibleSelected = visible.filter((t) => selected.has(t.id)).length;
  const all = $("check-all");
  all.checked = visible.length > 0 && visibleSelected === visible.length;
  all.indeterminate = visibleSelected > 0 && visibleSelected < visible.length;
  // Bulk actions are always visible and greyed out until they apply to the selection.
  // Start and Pause share one spot, like on the rows; Pause wins when both apply.
  const picked = torrents.filter((t) => selected.has(t.id));
  const canStart = picked.some(isStopped);
  const canPause = picked.some((t) => t.phase !== "completed" && !isStopped(t));
  const showStart = canStart && !canPause;
  $("start-selected").hidden = !showStart;
  $("pause-selected").hidden = showStart;
  $("start-selected").disabled = !canStart;
  $("pause-selected").disabled = !canPause;
  $("delete-selected").disabled = picked.length === 0;
}

async function refresh() {
  try {
    torrents = await invoke("list_torrents");
    render();
    for (const id of expanded) loadFiles(id);
  } catch (e) {
    toast(e);
  }
}

/* Actions */

const isStopped = (t) => t.phase === "paused" || t.phase === "error";

function start(id) {
  const t = torrents.find((x) => x.id === id);
  if (t && isStopped(t)) call("resume", { id }).then(refresh).catch(() => {});
}

function pause(id) {
  const t = torrents.find((x) => x.id === id);
  if (t && t.phase !== "completed" && !isStopped(t)) call("pause", { id }).then(refresh).catch(() => {});
}

function toggle(id) {
  const t = torrents.find((x) => x.id === id);
  if (t) isStopped(t) ? start(id) : pause(id);
}

function selectAll(on) {
  const visible = visibleTorrents();
  if (on === undefined) on = !visible.every((t) => selected.has(t.id));
  for (const t of visible) on ? selected.add(t.id) : selected.delete(t.id);
  render();
}

function confirmDelete(ids) {
  ids = ids.filter((id) => torrents.some((t) => t.id === id));
  if (ids.length === 0) return;
  const first = torrents.find((t) => t.id === ids[0]);
  $("delete-title").textContent = ids.length === 1 ? `Delete “${first.name}”?` : `Delete ${ids.length} torrents?`;
  $("delete-text").textContent = ids.length === 1
    ? "The downloaded files will be moved to the Trash."
    : "Their downloaded files will be moved to the Trash.";
  const dlg = $("delete-dialog");
  dlg.returnValue = "";
  dlg.onclose = async () => {
    const choice = dlg.returnValue;
    if (choice !== "trash" && choice !== "list") return;
    try {
      await call("delete", { ids, deleteFiles: choice === "trash" });
    } catch {}
    ids.forEach((id) => selected.delete(id));
    refresh();
  };
  dlg.showModal();
}

$("check-all").addEventListener("change", (e) => selectAll(e.target.checked));
$("delete-selected").addEventListener("click", () => confirmDelete([...selected]));
$("start-selected").addEventListener("click", () => selected.forEach(start));
$("pause-selected").addEventListener("click", () => selected.forEach(pause));

$("list").addEventListener("click", (e) => {
  if (e.target === $("list")) {
    selected.clear();
    render();
  }
});

/* Keyboard */

document.addEventListener("keydown", (e) => {
  if (document.querySelector("dialog[open]")) return;
  if (e.metaKey && e.key === "a") {
    e.preventDefault();
    selectAll(true);
  } else if ((e.key === "Backspace" || e.key === "Delete") && selected.size) {
    e.preventDefault();
    confirmDelete([...selected]);
  } else if (e.key === "Escape") {
    selected.clear();
    render();
  } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    const visible = visibleTorrents();
    const idx = visible.findIndex((t) => t.id === anchor);
    const next = visible[Math.max(0, Math.min(visible.length - 1, idx + (e.key === "ArrowDown" ? 1 : -1)))];
    if (next) {
      clickRow(next.id, { shiftKey: e.shiftKey, metaKey: false });
      rows.get(next.id)?.scrollIntoView({ block: "nearest" });
    }
  } else if (e.key === " " && selected.size) {
    e.preventDefault();
    selected.forEach(toggle);
  }
});

// Highlight while a .torrent is dragged over the window; the drop itself is handled in Rust.
listen("tauri://drag-enter", () => document.body.classList.add("dragging"));
listen("tauri://drag-leave", () => document.body.classList.remove("dragging"));
listen("tauri://drag-drop", () => {
  document.body.classList.remove("dragging");
  setTimeout(refresh, 300);
});

refresh();
setInterval(refresh, 1000);
