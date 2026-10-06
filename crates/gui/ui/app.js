"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const el = (id) => document.getElementById(id);
const ui = {
  setup: el("setup"),
  tokenLine: el("token-line"),
  token: el("token"),
  saveToken: el("save-token"),
  room: el("room"),
  password: el("password"),
  relay: el("relay"),
  advertise: el("advertise"),
  connect: el("connect"),
  host: el("host"),
  disconnect: el("disconnect"),
  status: el("status"),
  hosting: el("hosting"),
  findRooms: el("find-rooms"),
  rooms: el("rooms"),
  kind: el("kind"),
  source: el("source"),
  load: el("load"),
  play: el("play"),
  wave: el("wave"),
  wavePlay: el("wave-play"),
  likesTile: el("likes-tile"),
  likesTileMeta: el("likes-tile-meta"),
  offlineTile: el("offline-tile"),
  offlineTileMeta: el("offline-tile-meta"),
  query: el("query"),
  clearQuery: el("clear-query"),
  suggest: el("suggest"),
  find: el("find"),
  results: el("results"),
  queue: el("queue"),
  queueCount: el("queue-count"),
  navQueue: el("nav-queue"),
  dlTrack: el("dl-track"),
  dlQueue: el("dl-queue"),
  dlCancel: el("dl-cancel"),
  library: el("library"),
  libCount: el("lib-count"),
  libHint: el("lib-hint"),
  libPlay: el("lib-play"),
  libQueue: el("lib-queue"),
  libRefresh: el("lib-refresh"),
  libImport: el("lib-import"),
  libArt: el("lib-art"),
  libTitle: el("lib-title"),
  tabs: el("tabs"),
  nav: el("nav"),
  update: el("update"),
  updateTitle: el("update-title"),
  updateMeta: el("update-meta"),
  sideRoom: el("side-room"),
  sideDot: el("side-dot"),
  sideRoomName: el("side-room-name"),
  sideRoomMeta: el("side-room-meta"),
  heart: el("heart"),
  cover: el("cover"),
  title: el("title"),
  artist: el("artist"),
  subtitle: el("subtitle"),
  nowCover: el("now-cover"),
  nowBackdrop: el("now-backdrop"),
  nowTitle: el("now-title"),
  nowArtist: el("now-artist"),
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

/// How much has to be typed before the search runs by itself. Below this, only
/// Enter searches: two letters match half the catalogue and every keystroke would
/// be a wasted request.
const LIVE_SEARCH_FROM = 3;

/// How long to wait after the last keystroke before asking. Long enough that
/// typing a word is one request rather than six, short enough not to feel slow.
const TYPING_PAUSE_MS = 180;

/// Sources that are whole collections of their own: nothing to type in.
const SELF_CONTAINED = new Set(["wave", "likes"]);

/// Where the album view puts a download whose album name has not been fetched.
const UNNAMED_ALBUM = "без названия альбома";

/// How the collection page heads each of its three views.
const TAB_HEADS = {
  all: { title: "Скачанное", art: "", icon: "i-download" },
  likes: { title: "Мне нравится", art: "likes", icon: "i-heart" },
  albums: { title: "По альбомам", art: "albums", icon: "i-library" },
};

let connected = false;
let latest = null;
let results = [];
let queueKey = "";
let dragging = false;
let toastTimer = null;
let volumeTimer = null;
let typingTimer = null;
/// The room being raised right now, if any; see [`withRoom`].
let raising = null;
/// Answers that arrive out of order must not overwrite a newer query's: each
/// request carries the number of the keystroke it belongs to.
let searchToken = 0;
let suggestToken = 0;
/// Which line of the dropdown the arrow keys are on; -1 is the field itself.
let suggestIndex = -1;
/// Which source the «по ссылке» card loads from.
let kind = "search";
/// This device's downloads, and where they live. Known before connecting.
let library = [];
let libraryRevision = -1;
let cacheDir = "";
let cacheLimit = 0;
let autoCache = false;
/// This account's «Мне нравится», read off the disk — so it is on screen, and its
/// hearts are drawn, with no internet.
let likes = [];
let likedIds = new Set();
let likesRevision = -1;
/// Which view of the library is on screen: all / likes / albums.
let tab = "all";
/// Ids that cost no internet: on this disk, or on somebody else's in the room.
let cachedIds = new Set();
let lanIds = new Set();
/// What the room was called when we joined it, for the sidebar.
let roomName = "";
/// The cover last drawn in the player, so a snapshot four times a second does not
/// reload the same picture.
let coverKey = null;

function fmt(ms) {
  const total = Math.max(0, Math.floor((ms || 0) / 1000));
  return `${Math.floor(total / 60)}:${String(total % 60).padStart(2, "0")}`;
}

/** Binary units, to match what the file manager beside this window says. */
function fmtSize(bytes) {
  const mib = (bytes || 0) / (1024 * 1024);
  return mib >= 1024 ? `${(mib / 1024).toFixed(1)} ГиБ` : `${mib.toFixed(1)} МиБ`;
}

/** «1 трек», «3 трека», «25 треков». */
function tracksWord(count) {
  const tens = count % 100;
  const ones = count % 10;
  if (tens >= 11 && tens <= 14) return `${count} треков`;
  if (ones === 1) return `${count} трек`;
  if (ones >= 2 && ones <= 4) return `${count} трека`;
  return `${count} треков`;
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

/// For commands that answer with nothing: `null` is their success, so it cannot
/// double as the failure `call` reports with.
async function attempt(command, args) {
  try {
    await invoke(command, args);
    return true;
  } catch (err) {
    toast(String(err));
    return false;
  }
}

// ---------- drawing helpers ----------

const SVG = "http://www.w3.org/2000/svg";

/** One icon from the sprite in index.html. */
function icon(name) {
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("class", "i");
  const use = document.createElementNS(SVG, "use");
  use.setAttribute("href", `#${name}`);
  svg.append(use);
  return svg;
}

/** Swaps the icon inside a button that holds one. */
function setIcon(node, name) {
  const use = node.querySelector("use");
  if (use && use.getAttribute("href") !== `#${name}`) use.setAttribute("href", `#${name}`);
}

/// Yandex's image CDN: the only place a cover is fetched from.
const COVER_HOST = "avatars.yandex.net";

/// Yandex hands out a template, `avatars.yandex.net/…/%%`, and each screen asks
/// for the size it draws. Tracks downloaded before covers were stored have none,
/// and neither does anything with no internet — those get the placeholder.
///
/// Anything not on Yandex's CDN is refused here as well as by the CSP: the
/// template arrives inside tracks that any peer in the room can hand over, and
/// the phone has no CSP to fall back on, so both clients hold the same line.
function coverUrl(track, size) {
  const uri = track?.cover_uri?.trim();
  if (!uri) return null;
  const path = uri.replace(/^(https?:)?\/\//i, "");
  if (!path.startsWith(`${COVER_HOST}/`)) return null;
  return `https://${path.replace("%%", `${size}x${size}`)}`;
}

/// Fills a cover box: the picture when there is one, a note glyph otherwise —
/// and the glyph again if the picture fails, which it will with no internet.
function fillCover(box, track, size) {
  const url = coverUrl(track, size);
  const glyph = icon("i-note");
  box.replaceChildren(glyph);
  if (!url) return;
  const img = new Image();
  img.alt = "";
  img.loading = "lazy";
  img.decoding = "async";
  img.src = url;
  img.addEventListener("error", () => img.remove());
  box.append(img);
}

/** Keeps a slider's filled part in step with its value. */
function paintRange(input) {
  const span = Number(input.max) - Number(input.min) || 1;
  input.style.setProperty("--fill", `${((input.value - input.min) / span) * 100}%`);
}

/// A password for a room nobody asked to name.
///
/// The relay refuses a room without one, and stopping to ask for a password when
/// somebody pressed «Моя волна» would be silly — so one is made up and written to
/// the settings, where the room page shows it to whoever wants to join.
function newPassword() {
  const alphabet = "abcdefghijkmnpqrstuvwxyz23456789";
  const bytes = crypto.getRandomValues(new Uint8Array(12));
  return [...bytes].map((byte) => alphabet[byte % alphabet.length]).join("");
}

/// Runs an action in a room, holding one on this PC if there is none.
///
/// Playing something is the point of the app, so it is not gated behind setting
/// a room up first: the room is what the app needs, not what the listener asked
/// for, and it can be made without asking anything.
async function withRoom(action) {
  if (connected) return action();

  // One raise at a time, shared by everybody who asked while it was under way.
  // The search fires by itself after a pause in typing, so a second request
  // landing mid-connect is the normal case, not a corner: it waits for the same
  // room rather than starting another and being told «уже подключено».
  if (!raising) raising = raiseRoom().finally(() => (raising = null));
  if (!(await raising)) return;
  return action();
}

/// The room-raising half of [`withRoom`]. Answers whether there is a room now.
async function raiseRoom() {
  if (!ui.room.value.trim()) ui.room.value = "home";
  if (!ui.password.value.trim()) ui.password.value = newPassword();

  const snapshot = await call("connect", {
    host: true,
    advertise: ui.advertise.value,
    relay: ui.relay.value,
    room: ui.room.value,
    password: ui.password.value,
  });
  if (!snapshot) {
    // Something is wrong with the settings — the room page is where it is fixed.
    showView("room");
    return false;
  }

  roomName = ui.room.value;
  setConnected(true);
  render(snapshot);
  toast(`комната «${roomName}» поднята на этом ПК`);
  return true;
}

/** Puts tracks in the room's queue, raising the room first if need be. */
function queueTracks(tracks, replace = false) {
  if (!tracks.length) return;
  return withRoom(() => call("queue_tracks", { tracks, start: 0, replace }));
}

/// Plays a list from the track that was pressed, as any music player does: the
/// list becomes the queue, so «next» goes on through it and «previous» goes back.
/// Adding to the queue without interrupting is the row's own «в очередь» button.
function playFrom(tracks, index) {
  if (!tracks.length) return;
  return withRoom(() => call("queue_tracks", { tracks, start: index, replace: true }));
}

// ---------- search ----------

function hideSuggest() {
  ui.suggest.classList.add("hidden");
  ui.suggest.replaceChildren();
  ui.query.setAttribute("aria-expanded", "false");
  suggestIndex = -1;
}

/// The dropdown Yandex draws while you type: its best guess, then the queries.
function renderSuggest(found) {
  const rows = [];

  if (found.best) {
    const best = found.best;
    const row = document.createElement("button");
    row.type = "button";
    row.className = "suggest-best";
    row.dataset.query = best.query;

    const art = document.createElement("span");
    art.className = best.kind === "artist" ? "art round" : "art";
    fillCover(art, best, 100);

    const text = document.createElement("span");
    text.className = "meta";
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = best.name;
    const kind = document.createElement("span");
    kind.className = "kind";
    kind.textContent = best.subtitle
      ? `${KINDS[best.kind] ?? best.kind} · ${best.subtitle}`
      : (KINDS[best.kind] ?? best.kind);
    text.append(name, kind);

    row.append(art, text);
    rows.push(row);
    if (found.suggestions.length) rows.push(document.createElement("hr"));
  }

  for (const line of found.suggestions) {
    const row = document.createElement("button");
    row.type = "button";
    row.className = "suggest-line";
    row.dataset.query = line;
    const text = document.createElement("span");
    text.textContent = line;
    row.append(icon("i-search"), text);
    rows.push(row);
  }

  if (!rows.length) {
    hideSuggest();
    return;
  }
  ui.suggest.replaceChildren(...rows);
  ui.suggest.classList.remove("hidden");
  ui.query.setAttribute("aria-expanded", "true");
  suggestIndex = -1;
}

/// What the row under the name says.
const KINDS = { artist: "исполнитель", album: "альбом", track: "трек" };

/** Runs the search itself. `token` guards against a stale answer landing late. */
/// Searching raises a room first, as on the phone: whatever is found is about
/// to be played, and playing needs one. The suggestions do not — they are only
/// words under the field.
async function runSearch(query, token) {
  const found = await withRoom(() => call("search", { query, limit: 30 }));
  if (token !== searchToken || !found) return;
  results = found;
  renderResults();
  return found;
}

/// Everything that happens on a keystroke: the dropdown, and — once there is
/// enough typed — the search itself.
function onTyping() {
  const query = ui.query.value.trim();
  ui.clearQuery.classList.toggle("hidden", !ui.query.value);
  clearTimeout(typingTimer);

  if (query.length < LIVE_SEARCH_FROM) {
    // Not enough to go on: the dropdown closes and nothing is asked for. Enter
    // still searches for whatever is typed.
    hideSuggest();
    return;
  }

  typingTimer = setTimeout(async () => {
    const mine = ++suggestToken;
    const search = ++searchToken;
    runSearch(query, search);

    const found = await call("suggest", { part: query });
    // Another keystroke has already been sent: that answer is the current one.
    if (mine !== suggestToken || !found) return;
    if (document.activeElement === ui.query) renderSuggest(found);
  }, TYPING_PAUSE_MS);
}

/** Searches for exactly this, from Enter or from a line of the dropdown. */
function searchFor(query) {
  if (!query) return;
  ui.query.value = query;
  ui.clearQuery.classList.remove("hidden");
  clearTimeout(typingTimer);
  hideSuggest();
  runSearch(query, ++searchToken);
}

// ---------- navigation ----------

function showView(name) {
  for (const view of document.querySelectorAll(".view")) {
    view.classList.toggle("hidden", view.dataset.view !== name);
  }
  for (const item of ui.nav.querySelectorAll(".nav-item")) {
    item.classList.toggle("current", item.dataset.view === name);
  }
  document.querySelector(".main").scrollTop = 0;
}

ui.nav.addEventListener("click", (event) => {
  const item = event.target.closest(".nav-item");
  if (item) showView(item.dataset.view);
});
ui.sideRoom.addEventListener("click", () => showView("room"));
ui.cover.addEventListener("click", () => showView("queue"));
// A tile is a way into a list, as a playlist card is anywhere else. Playing is
// the round button on the collection page.
ui.likesTile.addEventListener("click", () => {
  selectTab("likes");
  showView("library");
});
ui.offlineTile.addEventListener("click", () => {
  selectTab("all");
  showView("library");
});

// ---------- state ----------

function setConnected(value) {
  connected = value;
  // The way into a room is only interesting when you are not in one: while
  // connected the room page carries the room's state instead.
  ui.setup.classList.toggle("hidden", value);
  ui.disconnect.classList.toggle("hidden", !value);

  // Protocol 3 has no roles: everyone in the room may drive it.
  // Only what needs something already playing is turned off. Everything that
  // starts music stays live: pressing it raises a room on this PC first.
  for (const node of [ui.prev, ui.toggle, ui.next, ui.seek, ui.dlTrack, ui.dlQueue]) {
    node.disabled = !value;
  }
  updateLibraryButtons();

  if (!value) {
    latest = null;
    queueKey = "";
    coverKey = null;
    libraryRevision = -1;
    likesRevision = -1;
    cachedIds = new Set();
    lanIds = new Set();
    ui.connect.textContent = "Подключиться";
    ui.host.textContent = "Хостить";
    ui.connect.disabled = false;
    ui.host.disabled = false;
    ui.status.className = "status";
    ui.status.textContent = "не подключено";
    ui.sideDot.className = "dot";
    ui.sideRoomName.textContent = "не подключено";
    ui.sideRoomMeta.textContent = "нажмите, чтобы войти в комнату";
    ui.drift.classList.add("hidden");
    ui.wave.classList.add("hidden");
    ui.hosting.classList.add("hidden");
    ui.dlCancel.classList.add("hidden");
    ui.queue.replaceChildren(emptyRow("Очередь пуста — поставьте что-нибудь с главной"));
    ui.queueCount.textContent = "";
    ui.navQueue.textContent = "";
    ui.title.textContent = "—";
    ui.artist.textContent = "";
    ui.subtitle.textContent = "";
    ui.nowTitle.textContent = "Тишина";
    ui.nowArtist.textContent = "поставьте что-нибудь в очередь";
    ui.nowBackdrop.style.backgroundImage = "";
    fillCover(ui.cover, null, 100);
    fillCover(ui.nowCover, null, 400);
    document.body.classList.remove("playing");
    setIcon(ui.toggle, "i-play");
    ui.seek.value = 0;
    paintRange(ui.seek);
    ui.position.textContent = "0:00";
    ui.duration.textContent = "0:00";
    updateHeart();
  }
}

/// Playing the library means queueing it into the room, so it needs both the
/// tracks and a connection.
function updateLibraryButtons() {
  const usable = visibleTracks().length > 0;
  ui.libPlay.disabled = !usable;
  ui.libQueue.disabled = !usable;
  ui.libRefresh.title =
    tab === "likes"
      ? "перечитать «Мне нравится» с Яндекса"
      : "перечитать папку с треками";
}

/// The heart beside the title in the player. The engine says whether the track
/// playing right now is liked, so this agrees with the phone's notification.
function updateHeart() {
  const track = latest?.track;
  const liked = latest?.track_liked ?? false;
  ui.heart.className = liked ? "heart on" : "heart";
  setIcon(ui.heart, liked ? "i-heart" : "i-heart-outline");
  ui.heart.title = liked ? "убрать из «Мне нравится»" : "в «Мне нравится»";
  ui.heart.disabled = !track;
}

function renderQueueAndResults() {
  if (latest) renderQueue(latest);
  renderResults();
}

/// Where a track would come from, when that costs no internet.
function origin(trackId) {
  if (cachedIds.has(trackId)) {
    return { icon: "i-disk", cls: "mark disk", title: "есть на этом устройстве" };
  }
  if (lanIds.has(trackId)) {
    return { icon: "i-lan", cls: "mark lan", title: "есть у кого-то в комнате" };
  }
  return null;
}

function emptyRow(text) {
  const item = document.createElement("li");
  item.className = "empty";
  item.textContent = text;
  return item;
}

/// The playing row's equaliser.
function equaliser() {
  const eq = document.createElement("span");
  eq.className = "eq";
  eq.append(document.createElement("span"), document.createElement("span"), document.createElement("span"));
  return eq;
}

/// Rows are real buttons: keyboard-reachable, and assistive tech (and UI
/// automation) can activate them, which a bare `li` with a click handler cannot.
///
/// Every row carries a heart, because the track you want to keep is as likely to
/// be one you are queueing as the one already playing. It sits outside the row's
/// own button: a button inside a button is not valid, and not clickable either.
function trackItem(track, index, { current, onClick, trailing, extra, queueable = true }) {
  const item = document.createElement("li");
  if (current) item.className = "current";

  const entry = document.createElement("button");
  entry.type = "button";
  entry.className = "entry";

  const number = document.createElement("span");
  number.className = "num";
  number.textContent = String(index + 1);

  const art = document.createElement("span");
  art.className = "art";
  fillCover(art, track, 100);
  const overlay = document.createElement("span");
  overlay.className = "overlay";
  overlay.append(current ? equaliser() : icon("i-play"));
  art.append(overlay);

  const meta = document.createElement("span");
  meta.className = "meta";
  const name = document.createElement("span");
  name.className = "name";
  name.textContent = track.title;
  const by = document.createElement("span");
  by.className = "by";
  by.textContent = track.artist;
  meta.append(name, by);

  const flag = origin(track.track_id);
  const mark = document.createElement("span");
  mark.className = flag ? flag.cls : "mark";
  if (flag) {
    mark.append(icon(flag.icon));
    mark.title = flag.title;
  }

  const time = document.createElement("span");
  time.className = "time";
  time.textContent = trailing ?? fmt(track.duration_ms);

  entry.append(number, art, meta, mark, time);
  entry.addEventListener("click", onClick);
  item.append(entry);
  // Pressing the row plays it; this adds it to the end without interrupting.
  // Not on the queue's own rows, where every track is already queued.
  if (queueable) item.append(queueButton(track));
  item.append(heartButton(track));
  if (extra) item.append(extra);
  return item;
}

/// «В очередь»: shown on hover, like the heart beside it.
function queueButton(track) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "enqueue";
  button.append(icon("i-add"));
  button.title = "добавить в очередь";
  button.addEventListener("click", async () => {
    button.disabled = true;
    // `attempt`, not `call`: the command answers with nothing, so only a
    // true/false tells success from a refusal.
    const done = await withRoom(() =>
      attempt("queue_tracks", { tracks: [track], start: 0, replace: false }),
    );
    button.disabled = false;
    if (done) toast(`в очереди: ${track.artist} — ${track.title}`);
  });
  return button;
}

/// The heart, drawn from the stored «Мне нравится».
///
/// Yandex is asked first and the list is re-read after, so a refusal leaves the
/// heart where it was instead of lighting up and lying until the next refresh.
function heartButton(track) {
  const liked = likedIds.has(track.track_id);
  const button = document.createElement("button");
  button.type = "button";
  button.className = liked ? "heart on" : "heart";
  button.append(icon(liked ? "i-heart" : "i-heart-outline"));
  button.title = liked ? "убрать из «Мне нравится»" : "в «Мне нравится»";
  button.addEventListener("click", async () => {
    button.disabled = true;
    const done = await attempt("like", { track, liked: !liked });
    button.disabled = false;
    if (done) await refreshLikes();
  });
  return button;
}

function renderQueue(snapshot) {
  const count = snapshot.queue.length;
  ui.queueCount.textContent = count ? `· ${tracksWord(count)}` : "";
  ui.navQueue.textContent = count ? String(count) : "";
  if (!count) {
    ui.queue.replaceChildren(emptyRow("Очередь пуста — поставьте что-нибудь с главной"));
    return;
  }
  ui.queue.replaceChildren(
    ...snapshot.queue.map((track, index) =>
      trackItem(track, index, {
        current: index === snapshot.index,
        onClick: () => call("control", { action: "index", value: index }),
        queueable: false,
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
        onClick: () => playFrom(results, index),
      }),
    ),
  );
}

/// The library page, in whichever of its three views is on screen.
///
/// Two of them are this device's disk — flat, and grouped by album — and one is
/// «Мне нравится» from the account. All three are drawn from lists already in
/// memory, so switching tabs asks nothing of anybody.
function renderLibrary() {
  const head = TAB_HEADS[tab];
  ui.libTitle.textContent = head.title;
  ui.libArt.className = `playlist-art ${head.art}`;
  setIcon(ui.libArt, head.icon);

  if (tab === "likes") {
    renderLikes();
  } else if (tab === "albums") {
    renderAlbums();
  } else {
    renderDownloads();
  }
  updateLibraryButtons();
  updateLibraryHint();
  updateTiles();
}

/// The flat list of downloads. Rows carry a delete of their own, because this is
/// the one list where the tracks belong to this device rather than to the room.
function renderDownloads() {
  if (!library.length) {
    ui.library.replaceChildren(emptyRow("Здесь пока пусто — скачивайте треки со страницы очереди"));
    return;
  }
  ui.library.replaceChildren(
    ...library.map((entry, index) =>
      trackItem(entry, index, {
        current: false,
        trailing: fmtSize(entry.bytes),
        onClick: () => playFrom(library, index),
        extra: dropButton(entry),
      }),
    ),
  );
}

/// «Мне нравится» from the account, newest first. Marked rows are the ones that
/// are also on this disk and so cost no internet.
function renderLikes() {
  if (!likes.length) {
    ui.library.replaceChildren(emptyRow("Список пуст или ещё не загружен"));
    return;
  }
  ui.library.replaceChildren(
    ...likes.map((track, index) =>
      trackItem(track, index, {
        current: false,
        onClick: () => playFrom(likes, index),
      }),
    ),
  );
}

/// The downloads grouped by album, with a header per album that queues the whole
/// of it. An album with no name yet is one whose title has not been fetched —
/// that needs an internet connection, and the hint below says so.
function renderAlbums() {
  const nodes = [];
  for (const group of albumGroups()) {
    const header = document.createElement("li");
    header.className = "group";

    const button = document.createElement("button");
    button.type = "button";
    button.className = "group-entry";

    const art = document.createElement("span");
    art.className = "art";
    fillCover(art, group.tracks.find((entry) => entry.cover_uri) ?? null, 100);

    const name = document.createElement("span");
    name.className = "group-name";
    name.textContent = group.name;

    const count = document.createElement("span");
    count.className = "group-count";
    count.textContent = tracksWord(group.tracks.length);

    button.append(art, name, count);
    // The album header plays the album, like an album card anywhere else.
    button.title = "играть альбом";
    button.addEventListener("click", () => playFrom(group.tracks, 0));

    const size = document.createElement("span");
    size.className = "time";
    size.textContent = fmtSize(group.tracks.reduce((sum, entry) => sum + entry.bytes, 0));

    header.append(button, size);
    nodes.push(header);

    group.tracks.forEach((entry, index) => {
      nodes.push(
        trackItem(entry, index, {
          current: false,
          onClick: () => playFrom(group.tracks, index),
          extra: dropButton(entry),
        }),
      );
    });
  }
  ui.library.replaceChildren(...(nodes.length ? nodes : [emptyRow("Здесь пока пусто")]));
}

/// Downloads by album: named albums first in alphabetical order, then everything
/// whose album name is not known yet, in one group at the end.
///
/// One group, not one per album id: without a name they would all be headed
/// «без названия альбома», and fourteen identical headers say less than a single
/// pile does. The name arrives with the next connection and they sort themselves.
function albumGroups() {
  const groups = new Map();
  for (const entry of library) {
    const key = entry.album || UNNAMED_ALBUM;
    if (!groups.has(key)) groups.set(key, { name: key, tracks: [] });
    groups.get(key).tracks.push(entry);
  }
  return [...groups.values()].sort((a, b) => {
    const unnamed = (group) => (group.name === UNNAMED_ALBUM ? 1 : 0);
    return unnamed(a) - unnamed(b) || a.name.localeCompare(b.name, "ru");
  });
}

/// Deleting a download. Only for rows that are on this disk: a like is not a file.
function dropButton(entry) {
  const drop = document.createElement("button");
  drop.type = "button";
  drop.className = "drop";
  drop.append(icon("i-close"));
  drop.title = "удалить с этого устройства";
  drop.addEventListener("click", async () => {
    await call("forget", { id: entry.track_id });
    await refreshLibrary();
  });
  return drop;
}

/// What «Играть» and «В очередь» act on: whatever the open tab is showing.
function visibleTracks() {
  if (tab === "likes") return likes;
  if (tab === "albums") return albumGroups().flatMap((group) => group.tracks);
  return library;
}

function updateLibraryHint() {
  if (tab === "likes") {
    const marked = likes.filter((track) => cachedIds.has(track.track_id)).length;
    ui.libCount.textContent = likes.length ? `${tracksWord(likes.length)} · ${marked} на этом устройстве` : "";
    ui.libHint.textContent = likes.length
      ? "плейлист с Яндекса — отмеченные значком играют без интернета"
      : "нажмите ↻, когда будет интернет";
    return;
  }

  const total = library.reduce((sum, entry) => sum + entry.bytes, 0);
  const size = cacheLimit ? `${fmtSize(total)} из ${fmtSize(cacheLimit)}` : fmtSize(total);
  ui.libCount.textContent = library.length ? `${tracksWord(library.length)} · ${size}` : "";
  const kept = autoCache ? "сохраняется всё, что играет" : "сохраняется только скачанное";
  if (!library.length) {
    ui.libHint.textContent = `кнопки «Трек» и «Всю очередь» на странице очереди оставляют музыку здесь · ${cacheDir}`;
    return;
  }
  // Missing album names are the one thing this view cannot work around on its
  // own: they are fetched when there is a connection and then live on disk.
  const unnamed = library.filter((entry) => !entry.album).length;
  const albums =
    tab === "albums" && unnamed
      ? ` · у ${unnamed} трек(ов) название альбома ещё не загружено, нужен интернет`
      : "";
  ui.libHint.textContent = `${kept}${albums} · ${cacheDir}`;
}

/// The counts on the home page's two tiles.
function updateTiles() {
  ui.likesTileMeta.textContent = likes.length ? tracksWord(likes.length) : "пока пусто";
  ui.offlineTileMeta.textContent = library.length
    ? `${tracksWord(library.length)} · ${fmtSize(library.reduce((sum, entry) => sum + entry.bytes, 0))}`
    : "пока пусто";
}

function selectTab(name) {
  tab = name;
  for (const node of ui.tabs.querySelectorAll(".tab")) {
    node.classList.toggle("current", node.dataset.tab === name);
  }
  renderLibrary();
}

/** Re-reads what is on disk. Works with no connection and no internet. */
async function refreshLibrary() {
  const tracks = await call("library");
  if (!tracks) return;

  library = tracks;
  cachedIds = new Set(tracks.map((entry) => entry.track_id));
  renderLibrary();
}

/** Re-reads the stored «Мне нравится». No network: this is the list on disk. */
async function refreshLikes() {
  const tracks = await call("likes");
  if (!tracks) return;

  likes = tracks;
  likedIds = new Set(tracks.map((track) => track.track_id));
  // Every list draws hearts from this, so all of them are now stale.
  renderQueueAndResults();
  renderLibrary();
  updateHeart();
}

/// The player bar's left end and the queue page's hero: both show the track.
function renderNow(track) {
  ui.title.textContent = track ? track.title : "—";
  ui.artist.textContent = track ? track.artist : "";
  ui.nowTitle.textContent = track ? track.title : "Тишина";
  ui.nowArtist.textContent = track ? track.artist : "поставьте что-нибудь в очередь";

  const key = track ? `${track.track_id}|${track.cover_uri ?? ""}` : "";
  if (key === coverKey) return;
  coverKey = key;
  fillCover(ui.cover, track, 100);
  fillCover(ui.nowCover, track, 400);
  const backdrop = coverUrl(track, 200);
  ui.nowBackdrop.style.backgroundImage = backdrop ? `url("${backdrop}")` : "";
}

function render(snapshot) {
  latest = snapshot;
  // Recomputed before anything is drawn: both lists mark their rows with it.
  cachedIds = new Set(snapshot.cached || []);
  lanIds = new Set(snapshot.on_lan || []);

  renderNow(snapshot.track);
  updateHeart();

  const place = snapshot.queue.length ? `${snapshot.index + 1} из ${snapshot.queue.length}` : "";
  const note = snapshot.loading ? "загрузка…" : (snapshot.notice || "");
  const fetching = snapshot.downloading
    ? snapshot.download_queue > 0
      ? `скачиваю, ещё ${snapshot.download_queue}`
      : "скачиваю"
    : "";
  ui.subtitle.textContent = [place, note, fetching].filter(Boolean).join(" · ");
  ui.dlCancel.classList.toggle("hidden", snapshot.download_queue === 0);

  document.body.classList.toggle("playing", Boolean(snapshot.playing));
  setIcon(ui.toggle, snapshot.playing ? "i-pause" : "i-play");
  ui.position.textContent = snapshot.loading ? "…" : fmt(snapshot.position_ms);
  ui.duration.textContent = fmt(snapshot.duration_ms);
  if (!dragging) {
    ui.seek.value = snapshot.duration_ms
      ? Math.round((snapshot.position_ms / snapshot.duration_ms) * 1000)
      : 0;
    paintRange(ui.seek);
  }

  const rtt = snapshot.rtt_ms === null ? "?" : snapshot.rtt_ms;
  const offset = snapshot.offset_ms === null ? "?" : snapshot.offset_ms;
  const sharing = snapshot.sharing_peers ? ` · раздают ${snapshot.sharing_peers}` : "";
  ui.status.textContent =
    `участников ${snapshot.peers} · rtt ${rtt} мс · смещение ${offset} мс${sharing}`;
  ui.status.className = `status ${snapshot.connected ? "live" : "broken"}`;

  ui.sideDot.className = `dot ${snapshot.connected ? "live" : "broken"}`;
  ui.sideRoomName.textContent = roomName || "комната";
  ui.sideRoomMeta.textContent = snapshot.connected
    ? `участников ${snapshot.peers}${snapshot.hosting ? " · здесь" : ""}`
    : "связь потеряна";

  // The address others must type in, and the reason this app has no `relay` of
  // its own to show while hosting.
  if (snapshot.hosting) {
    ui.hosting.classList.remove("hidden");
    ui.hosting.textContent = `комната здесь · ${snapshot.hosting}`;
  } else {
    ui.hosting.classList.add("hidden");
  }

  // The station is a room-wide setting, but only its feeder can refill it, so
  // say which of the two we are looking at.
  if (snapshot.station) {
    ui.wave.classList.remove("hidden");
    ui.wave.textContent = snapshot.feeding ? "Волна играет · выключить" : "Волну ведёт другое устройство";
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
    ui.drift.title = "насколько это устройство отстаёт от комнаты или опережает её";
  }

  // Rebuilding 85 list items four times a second would fight the scrollbar. The
  // cache revision is in the key because a finished download changes the marks
  // on rows that have not otherwise moved.
  const key = [
    snapshot.queue.length,
    snapshot.index,
    snapshot.queue[0]?.track_id ?? "",
    snapshot.cache_revision,
    lanIds.size,
  ].join("|");
  if (key !== queueKey) {
    queueKey = key;
    renderQueue(snapshot);
    renderResults();
  }

  // A download that finished changed the disk, so the library page is stale.
  if (snapshot.cache_revision !== libraryRevision) {
    libraryRevision = snapshot.cache_revision;
    refreshLibrary();
  }

  // And the same for «Мне нравится»: the engine re-reads it on connecting and
  // bumps this whenever it changes, here or on Yandex.
  if (snapshot.liked_revision !== likesRevision) {
    likesRevision = snapshot.liked_revision;
    refreshLikes();
  }
}

/// Joins a room, or holds it here. The two buttons differ only in `host`.
async function enter(button, host) {
  ui.connect.disabled = true;
  ui.host.disabled = true;
  const label = button.textContent;
  button.textContent = host ? "Поднимаю…" : "Подключаюсь…";

  const snapshot = await call("connect", {
    host,
    advertise: ui.advertise.value,
    relay: ui.relay.value,
    room: ui.room.value,
    password: ui.password.value,
  });

  button.textContent = label;
  if (snapshot) {
    roomName = ui.room.value;
    setConnected(true);
    render(snapshot);
    showView("home");
  } else {
    setConnected(false);
  }
}

ui.connect.addEventListener("click", () => enter(ui.connect, false));
ui.host.addEventListener("click", () => enter(ui.host, true));

/// Stores the Yandex token. The card disappears once there is one.
///
/// A refusal from Yandex leaves the field as typed — the token is probably a
/// mistyped paste, and clearing it would mean starting over.
ui.saveToken.addEventListener("click", async () => {
  if (!ui.token.value.trim()) {
    toast("впишите токен — где его взять, написано в README");
    return;
  }

  ui.saveToken.disabled = true;
  const label = ui.saveToken.textContent;
  ui.saveToken.textContent = "Проверяю…";

  const verdict = await call("save_token", { token: ui.token.value });

  ui.saveToken.textContent = label;
  ui.saveToken.disabled = false;
  if (verdict === null) return;

  ui.token.value = "";
  ui.tokenLine.classList.add("hidden");
  toast(verdict);
});

ui.token.addEventListener("keydown", (event) => {
  if (event.key === "Enter") ui.saveToken.click();
});

ui.disconnect.addEventListener("click", async () => {
  ui.disconnect.disabled = true;
  await call("disconnect");
  ui.disconnect.disabled = false;
  setConnected(false);
});

// Discovery is a question to the network, not to the relay: it works before
// anything is configured, which is the point — the answer is what to configure.
ui.findRooms.addEventListener("click", async () => {
  ui.findRooms.disabled = true;

  const found = await call("find_rooms", { wait: 700 });

  ui.findRooms.disabled = false;
  if (!found) return;

  if (found.length === 0) {
    ui.rooms.classList.add("hidden");
    toast("в сети никто не отвечает — комнату держит ymsync-relay или участник, нажавший «Хостить»");
    return;
  }

  ui.rooms.replaceChildren(
    new Option("комната из сети…", ""),
    ...found.map((room, index) => {
      // An empty name means the relay holds no rooms yet: pick it and the name
      // in the field is what will be created.
      const name = room.room || "комнат пока нет";
      const listeners = room.listeners ? ` · ${room.listeners}` : "";
      const version = room.compatible ? "" : " · другая версия";
      const option = new Option(`${name} · ${room.relay}${listeners}${version}`, index);
      // A relay of another protocol version would refuse this client, so the row
      // is shown and greyed rather than hidden: it explains the situation.
      option.disabled = !room.compatible;
      return option;
    }),
  );
  ui.rooms.classList.remove("hidden");
  // Picking a room fills the fields rather than remembering a choice of its own:
  // what is on screen is then exactly what will be connected to.
  ui.rooms.onchange = () => {
    const room = found[Number(ui.rooms.value)];
    if (!room) return;
    ui.relay.value = room.relay;
    if (room.room) ui.room.value = room.room;
  };
  toast(`нашлось: ${found.length}`);
});

ui.kind.addEventListener("click", (event) => {
  const button = event.target.closest(".seg");
  if (!button) return;
  kind = button.dataset.kind;
  for (const node of ui.kind.querySelectorAll(".seg")) {
    node.classList.toggle("current", node === button);
  }
  ui.source.placeholder = PLACEHOLDERS[kind] ?? "";
  ui.source.focus();
});

/** Loads a source into the room. `replace` starts it instead of queueing. */
async function loadSource(button, source, value, replace) {
  if (!value && !SELF_CONTAINED.has(source)) return;

  button.disabled = true;
  const length = await withRoom(() => call("play_source", { kind: source, value, replace }));
  button.disabled = false;

  if (typeof length !== "number") return;
  if (source === "wave") {
    toast(replace ? "волна включена" : "волна продолжит очередь");
  } else if (replace) {
    toast(`играю: ${tracksWord(length)}`);
  } else {
    toast(`добавлено в очередь: ${tracksWord(length)}`);
  }
}

ui.load.addEventListener("click", () => loadSource(ui.load, kind, ui.source.value.trim(), false));
ui.play.addEventListener("click", () => loadSource(ui.play, kind, ui.source.value.trim(), true));
ui.wavePlay.addEventListener("click", () => loadSource(ui.wavePlay, "wave", "", true));
ui.wave.addEventListener("click", () => call("control", { action: "stop_wave" }));

ui.find.addEventListener("click", async () => {
  const query = ui.query.value.trim();
  if (!query) return;
  ui.find.disabled = true;
  clearTimeout(typingTimer);
  hideSuggest();
  const found = await runSearch(query, ++searchToken);
  ui.find.disabled = false;
  if (found && found.length === 0) toast("ничего не найдено");
});

ui.query.addEventListener("input", onTyping);
ui.query.addEventListener("focus", onTyping);
ui.clearQuery.addEventListener("click", () => {
  ui.query.value = "";
  ui.clearQuery.classList.add("hidden");
  hideSuggest();
  ui.query.focus();
});

// A press picks that line; the field keeps what was chosen, as Yandex does.
ui.suggest.addEventListener("mousedown", (event) => {
  // Before `blur`, or the dropdown would be gone by the time the click lands.
  const row = event.target.closest("[data-query]");
  if (!row) return;
  event.preventDefault();
  searchFor(row.dataset.query);
});

// Leaving the field closes the dropdown, but not before a click on it is seen.
ui.query.addEventListener("blur", () => setTimeout(hideSuggest, 120));

document.addEventListener("click", (event) => {
  if (!event.target.closest(".search")) hideSuggest();
});

ui.source.addEventListener("keydown", (event) => {
  if (event.key === "Enter") ui.load.click();
});
/// The keyboard in the dropdown: arrows walk the lines, Enter takes one, Escape
/// closes it. With nothing highlighted, Enter searches for what was typed — which
/// is the only way to search at all below [`LIVE_SEARCH_FROM`] characters.
ui.query.addEventListener("keydown", (event) => {
  const rows = [...ui.suggest.querySelectorAll("[data-query]")];
  const open = !ui.suggest.classList.contains("hidden") && rows.length > 0;

  if (event.key === "Escape") {
    hideSuggest();
    return;
  }

  if (open && (event.key === "ArrowDown" || event.key === "ArrowUp")) {
    event.preventDefault();
    const step = event.key === "ArrowDown" ? 1 : -1;
    // Positions run 0…N, where 0 is the field itself and k is row k-1: cycling
    // through N+1 places, so Down from the last row lands back in the field.
    const places = rows.length + 1;
    suggestIndex = ((suggestIndex + 1 + step + places) % places) - 1;
    rows.forEach((row, index) => row.classList.toggle("current", index === suggestIndex));
    if (suggestIndex >= 0) rows[suggestIndex].scrollIntoView({ block: "nearest" });
    return;
  }

  if (event.key === "Enter") {
    if (open && suggestIndex >= 0) {
      searchFor(rows[suggestIndex].dataset.query);
      return;
    }
    ui.find.click();
  }
});

ui.toggle.addEventListener("click", () => call("control", { action: "toggle" }));

/// Space pauses and resumes, as in any player — unless the focus is somewhere
/// space means a space: the search field, the room's settings.
function typingInto(target) {
  return (
    target instanceof HTMLInputElement ||
    target instanceof HTMLTextAreaElement ||
    target instanceof HTMLSelectElement ||
    target?.isContentEditable
  );
}

// Caught before it reaches whatever is focused: after a click on a track row the
// row keeps the focus, and space on a button presses it — the track would start
// over instead of pausing. A browser presses buttons on keyup, so both halves of
// the keystroke are taken.
document.addEventListener(
  "keydown",
  (event) => {
    if (event.code !== "Space" || typingInto(event.target) || event.ctrlKey || event.altKey || event.metaKey) {
      return;
    }
    event.preventDefault();
    if (event.repeat || !connected || !latest?.track) return;
    call("control", { action: "toggle" });
  },
  true,
);
document.addEventListener(
  "keyup",
  (event) => {
    if (event.code === "Space" && !typingInto(event.target)) event.preventDefault();
  },
  true,
);
ui.next.addEventListener("click", () => call("control", { action: "next" }));
ui.prev.addEventListener("click", () => call("control", { action: "prev" }));

ui.dlTrack.addEventListener("click", () => {
  const track = latest?.track;
  if (!track) {
    toast("сейчас ничего не играет");
    return;
  }
  call("download", { tracks: [track] });
});
ui.dlQueue.addEventListener("click", () => {
  const tracks = latest?.queue ?? [];
  if (tracks.length === 0) {
    toast("очередь пуста");
    return;
  }
  call("download", { tracks });
});
ui.dlCancel.addEventListener("click", () => call("cancel_downloads"));

// Replacing the queue with the library is how you listen with no internet: every
// one of these plays off the disk. On the «мне нравится» tab it is the playlist
// instead, which is the same act with a different list.
ui.libPlay.addEventListener("click", () => queueTracks(visibleTracks(), true));
ui.libQueue.addEventListener("click", () => queueTracks(visibleTracks()));

// Local files, added to this device's downloads. The picker is the system's own,
// so the paths never pass through this page.
ui.libImport.addEventListener("click", async () => {
  ui.libImport.disabled = true;
  const report = await call("import_tracks");
  ui.libImport.disabled = false;
  if (report) {
    await refreshLibrary();
    toast(report);
  }
});

// Re-reading means two different things: the folder on this disk, or the playlist
// on Yandex. Which one depends on what is on screen.
ui.libRefresh.addEventListener("click", async () => {
  if (tab !== "likes") {
    await refreshLibrary();
    return;
  }
  ui.libRefresh.disabled = true;
  const count = await call("refresh_likes");
  ui.libRefresh.disabled = false;
  if (typeof count === "number") {
    await refreshLikes();
    toast(`«Мне нравится»: ${tracksWord(count)}`);
  }
});

// Three views of the same page. Nothing is fetched on a switch: all three lists
// are already in memory.
ui.tabs.addEventListener("click", (event) => {
  const button = event.target.closest(".tab");
  if (button) selectTab(button.dataset.tab);
});

ui.heart.addEventListener("click", async () => {
  const track = latest?.track;
  if (!track) {
    toast("сейчас ничего не играет");
    return;
  }
  ui.heart.disabled = true;
  const done = await attempt("like", { track, liked: !latest.track_liked });
  ui.heart.disabled = false;
  if (done) await refreshLikes();
});

ui.seek.addEventListener("pointerdown", () => {
  dragging = true;
});
ui.seek.addEventListener("input", () => paintRange(ui.seek));
ui.seek.addEventListener("change", async () => {
  const duration = latest?.duration_ms ?? 0;
  if (duration > 0) {
    await call("control", { action: "seek", value: (ui.seek.value / 1000) * duration });
  }
  dragging = false;
});

ui.volume.addEventListener("input", () => {
  paintRange(ui.volume);
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
    showView("room");
  }
}).catch(() => {});
// Settings that could not be written back: the fields would look remembered and
// come up empty next launch.
listen("notice", (event) => toast(String(event.payload))).catch(() => {});

// ---------- updates ----------

/// Asks GitHub once per launch whether a newer release is out. Silent when it is
/// not, and silent when it cannot tell — no internet, no release yet — since a
/// message about that on every start would only be noise.
async function checkForUpdate() {
  let found = null;
  try {
    found = await invoke("check_update");
  } catch (err) {
    console.info("update check failed", err);
  }
  if (!found) return;
  ui.updateTitle.textContent = `Доступна ${found.version}`;
  ui.updateMeta.textContent = "нажмите, чтобы обновить";
  ui.update.title = found.notes || "скачать, проверить подпись и установить";
  ui.update.classList.remove("hidden");
}

// The download is checked against this project's signing key before anything is
// installed; then the installer runs and the app comes back on the new version.
ui.update.addEventListener("click", async () => {
  ui.update.disabled = true;
  ui.updateMeta.textContent = "скачиваю и проверяю подпись…";
  const done = await attempt("install_update");
  // On success the app restarts and never gets here.
  if (!done) {
    ui.update.disabled = false;
    ui.updateMeta.textContent = "не вышло — нажмите, чтобы повторить";
  }
});

(async function start() {
  checkForUpdate();
  ui.source.placeholder = PLACEHOLDERS[kind];
  setConnected(false);
  // The home page works without a room now — pressing anything there raises one.
  showView("home");
  paintRange(ui.volume);

  const settings = await call("settings");
  if (!settings) return;

  ui.volume.value = Math.round(settings.volume * 100);
  paintRange(ui.volume);
  // The file supplies the defaults; both buttons then act on what is on screen,
  // and whatever connects is written back.
  ui.room.value = settings.room;
  ui.password.value = settings.password;
  ui.relay.value = settings.relay;
  ui.advertise.value = settings.advertise;
  ui.status.textContent = settings.hosting
    ? `${settings.room} · в прошлый раз комнату держал этот ПК`
    : `${settings.room} · ${settings.relay}`;

  cacheDir = settings.cache_dir;
  cacheLimit = settings.cache_limit_bytes;
  autoCache = settings.auto_cache;
  await refreshLibrary();
  // Both lists come off the disk, so the collection is populated — hearts and
  // all — before anything is connected and even with no internet at all.
  await refreshLikes();

  // The Yandex token is the one thing a fresh install has nowhere to come from,
  // so the field appears exactly while it is missing. It stays out of the way
  // afterwards: the file is the place to change a token that already works.
  ui.tokenLine.classList.toggle("hidden", settings.has_yandex_token);
  if (!settings.has_yandex_token) {
    toast(`Впишите токен Яндекса на странице «Комната» — или в ${settings.config_path}`);
  }
})();
