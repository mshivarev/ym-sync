"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const el = (id) => document.getElementById(id);
const ui = {
  connect: el("connect"),
  status: el("status"),
  kind: el("kind"),
  source: el("source"),
  load: el("load"),
  play: el("play"),
  wave: el("wave"),
  query: el("query"),
  find: el("find"),
  results: el("results"),
  queue: el("queue"),
  queueCount: el("queue-count"),
  title: el("title"),
  subtitle: el("subtitle"),
  prev: el("prev"),
  toggle: el("toggle"),
  next: el("next"),
  seek: el("seek"),
  position: el("position"),
  duration: el("duration"),
  volume: el("volume"),
  drift: el("drift"),
  toast: el("toast"),
};

const PLACEHOLDERS = {
  search: "кино группа крови",
  album: "5307396 или ссылка на альбом",
  playlist: "логин/номер или ссылка на плейлист",
  track: "38633712 или ссылка на трек",
};

/// Sources that are whole collections of their own: nothing to type in.
const SELF_CONTAINED = new Set(["wave", "likes"]);

let connected = false;
let latest = null;
let results = [];
let queueKey = "";
let dragging = false;
let toastTimer = null;
let volumeTimer = null;

function fmt(ms) {
  const total = Math.max(0, Math.floor((ms || 0) / 1000));
  return `${Math.floor(total / 60)}:${String(total % 60).padStart(2, "0")}`;
}

function toast(message) {
  console.error(message);
  ui.toast.textContent = message;
  ui.toast.classList.remove("hidden");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => ui.toast.classList.add("hidden"), 20000);
}

/** Invokes a command, surfacing failures as a toast instead of throwing. */
async function call(command, args) {
  try {
    return await invoke(command, args);
  } catch (err) {
    toast(String(err));
    return null;
  }
}

function setConnected(value) {
  connected = value;
  ui.connect.textContent = value ? "Отключиться" : "Подключиться";
  ui.connect.classList.toggle("primary", !value);

  // Protocol 3 has no roles: everyone in the room may drive it.
  for (const node of [ui.kind, ui.source, ui.load, ui.play, ui.prev, ui.toggle, ui.next, ui.seek]) {
    node.disabled = !value;
  }

  if (!value) {
    latest = null;
    queueKey = "";
    ui.status.className = "status";
    ui.status.textContent = "не подключено";
    ui.drift.classList.add("hidden");
    ui.wave.classList.add("hidden");
    ui.queue.replaceChildren();
    ui.queueCount.textContent = "";
    ui.title.textContent = "—";
    ui.subtitle.textContent = "";
    ui.toggle.textContent = "\u25B6";
    ui.seek.value = 0;
    ui.position.textContent = "0:00";
    ui.duration.textContent = "0:00";
  }
}

/// Rows are real buttons: keyboard-reachable, and assistive tech (and UI
/// automation) can activate them, which a bare `li` with a click handler cannot.
function trackItem(track, index, { current, onClick }) {
  const item = document.createElement("li");

  const entry = document.createElement("button");
  entry.type = "button";
  entry.className = current ? "entry current" : "entry";

  const number = document.createElement("span");
  number.className = "num";
  number.textContent = String(index + 1);

  const name = document.createElement("span");
  name.className = "name";
  name.textContent = `${track.artist} — ${track.title}`;

  const time = document.createElement("span");
  time.className = "time";
  time.textContent = fmt(track.duration_ms);

  entry.append(number, name, time);
  entry.addEventListener("click", onClick);
  item.append(entry);
  return item;
}

function renderQueue(snapshot) {
  ui.queueCount.textContent = snapshot.queue.length ? `· ${snapshot.queue.length}` : "";
  ui.queue.replaceChildren(
    ...snapshot.queue.map((track, index) =>
      trackItem(track, index, {
        current: index === snapshot.index,
        onClick: () => call("control", { action: "index", value: index }),
      }),
    ),
  );
  const current = ui.queue.children[snapshot.index];
  if (current) current.scrollIntoView({ block: "nearest" });
}

function renderResults() {
  ui.results.replaceChildren(
    ...results.map((track, index) =>
      trackItem(track, index, {
        current: false,
        // One track at a time, appended: clicking must not throw away a queue
        // that is already playing.
        onClick: () => call("queue_tracks", { tracks: [track], start: 0, replace: false }),
      }),
    ),
  );
}

function render(snapshot) {
  latest = snapshot;

  const track = snapshot.track;
  ui.title.textContent = track ? `${track.artist} — ${track.title}` : "—";

  const place = snapshot.queue.length ? `${snapshot.index + 1} из ${snapshot.queue.length}` : "";
  const note = snapshot.loading ? "загрузка…" : (snapshot.notice || "");
  ui.subtitle.textContent = [place, note].filter(Boolean).join(" · ");

  ui.toggle.textContent = snapshot.playing ? "\u23F8" : "\u25B6";
  ui.position.textContent = fmt(snapshot.position_ms);
  ui.duration.textContent = fmt(snapshot.duration_ms);
  if (!dragging) {
    ui.seek.value = snapshot.duration_ms
      ? Math.round((snapshot.position_ms / snapshot.duration_ms) * 1000)
      : 0;
  }

  const rtt = snapshot.rtt_ms === null ? "?" : snapshot.rtt_ms;
  const offset = snapshot.offset_ms === null ? "?" : snapshot.offset_ms;
  ui.status.textContent =
    `участников ${snapshot.peers} · rtt ${rtt} мс · смещение ${offset} мс`;
  ui.status.className = `status ${snapshot.connected ? "live" : "broken"}`;

  // The station is a room-wide setting, but only its feeder can refill it, so
  // say which of the two we are looking at.
  if (snapshot.station) {
    ui.wave.classList.remove("hidden");
    ui.wave.textContent = snapshot.feeding ? "Волна · выключить" : "Волна (ведёт другой)";
    ui.wave.disabled = !snapshot.feeding;
  } else {
    ui.wave.classList.add("hidden");
  }

  const drift = snapshot.drift_ms;
  if (drift === null || drift === undefined) {
    ui.drift.classList.add("hidden");
  } else {
    const magnitude = Math.abs(drift);
    ui.drift.className = `drift ${magnitude < 100 ? "good" : magnitude < 300 ? "warn" : "bad"}`;
    ui.drift.textContent = `рассинхрон ${drift > 0 ? "+" : ""}${drift} мс`;
  }

  // Rebuilding 85 list items four times a second would fight the scrollbar.
  const key = `${snapshot.queue.length}|${snapshot.index}|${snapshot.queue[0]?.track_id ?? ""}`;
  if (key !== queueKey) {
    queueKey = key;
    renderQueue(snapshot);
  }
}

ui.connect.addEventListener("click", async () => {
  ui.connect.disabled = true;
  try {
    if (connected) {
      await call("disconnect");
      setConnected(false);
      return;
    }
    ui.connect.textContent = "Подключаюсь…";
    const snapshot = await call("connect");
    if (snapshot) {
      setConnected(true);
      render(snapshot);
    } else {
      setConnected(false);
    }
  } finally {
    ui.connect.disabled = false;
  }
});

ui.kind.addEventListener("change", () => {
  const kind = ui.kind.value;
  // A station or «Мне нравится» needs no argument, so the field would only
  // invite one that is ignored.
  ui.source.hidden = SELF_CONTAINED.has(kind);
  ui.source.placeholder = PLACEHOLDERS[kind] ?? "";
});

/** Loads whatever the source row names. `replace` starts it instead of queueing. */
async function loadSource(button, replace) {
  const kind = ui.kind.value;
  const value = ui.source.value.trim();
  if (!value && !SELF_CONTAINED.has(kind)) return;

  button.disabled = true;
  const length = await call("play_source", { kind, value, replace });
  button.disabled = false;

  if (typeof length !== "number") return;
  if (kind === "wave") {
    toast(replace ? "волна включена" : "волна продолжит очередь");
  } else if (replace) {
    toast(`играю: ${length} трек(ов)`);
  } else {
    toast(`добавлено в очередь: ${length}`);
  }
}

ui.load.addEventListener("click", () => loadSource(ui.load, false));
ui.play.addEventListener("click", () => loadSource(ui.play, true));
ui.wave.addEventListener("click", () => call("control", { action: "stop_wave" }));

ui.find.addEventListener("click", async () => {
  const query = ui.query.value.trim();
  if (!query) return;
  ui.find.disabled = true;
  const found = await call("search", { query, limit: 30 });
  ui.find.disabled = false;
  if (found) {
    results = found;
    renderResults();
    if (found.length === 0) toast("ничего не найдено");
  }
});

ui.source.addEventListener("keydown", (event) => {
  if (event.key === "Enter") ui.load.click();
});
ui.query.addEventListener("keydown", (event) => {
  if (event.key === "Enter") ui.find.click();
});

ui.toggle.addEventListener("click", () => call("control", { action: "toggle" }));
ui.next.addEventListener("click", () => call("control", { action: "next" }));
ui.prev.addEventListener("click", () => call("control", { action: "prev" }));

ui.seek.addEventListener("pointerdown", () => {
  dragging = true;
});
ui.seek.addEventListener("change", async () => {
  const duration = latest?.duration_ms ?? 0;
  if (duration > 0) {
    await call("control", { action: "seek", value: (ui.seek.value / 1000) * duration });
  }
  dragging = false;
});

ui.volume.addEventListener("input", () => {
  // The slider fires continuously; only the settled value is worth sending.
  clearTimeout(volumeTimer);
  volumeTimer = setTimeout(
    () => call("control", { action: "volume", value: ui.volume.value / 100 }),
    120,
  );
});

// `listen` is a core-plugin command and therefore permission-gated: report a
// failure loudly instead of leaving a window that never updates.
listen("snapshot", (event) => render(event.payload)).catch((err) =>
  toast(`не удалось подписаться на события движка: ${err}`),
);
listen("closed", () => {
  if (connected) {
    toast("соединение с релеем закрыто");
    setConnected(false);
  }
}).catch(() => {});

(async function start() {
  ui.source.placeholder = PLACEHOLDERS[ui.kind.value];
  setConnected(false);

  const settings = await call("settings");
  if (!settings) return;

  ui.volume.value = Math.round(settings.volume * 100);
  ui.status.textContent = `${settings.room} · ${settings.relay}`;

  const missing = [];
  if (!settings.has_yandex_token) missing.push("yandex_token");
  if (!settings.has_room_token) missing.push("room_token");
  if (missing.length) {
    toast(`Заполните ${missing.join(" и ")} в ${settings.config_path}`);
  }
})();
