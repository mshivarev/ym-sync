package dev.mshiv.ymsync

import android.Manifest
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.systemBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.FilterChip
import androidx.compose.material3.FilterChipDefaults
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.NavigationBarItemDefaults
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Slider
import androidx.compose.material3.SliderDefaults
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TextField
import androidx.compose.material3.TextFieldDefaults
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch

private val Bg = Color(0xFF0B0B0D)
private val Surface1 = Color(0xFF16161A)
private val Surface2 = Color(0xFF1F1F25)
private val Surface3 = Color(0xFF2A2A31)
private val Accent = Color(0xFFFFD52E)
private val AccentInk = Color(0xFF1A1500)
private val Dim = Color(0xFF8E8E99)
private val Good = Color(0xFF4EC97A)
private val Warn = Color(0xFFFFC72C)
private val Bad = Color(0xFFFF5A5F)

private val LikesGradient = Brush.linearGradient(listOf(Color(0xFFFF5A6E), Color(0xFFB3122F)))
private val OfflineGradient = Brush.linearGradient(listOf(Color(0xFF3A7BFF), Color(0xFF2439A8)))
private val AlbumsGradient = Brush.linearGradient(listOf(Color(0xFFFFB800), Color(0xFFD9480F)))

/// How much has to be typed before the search runs by itself. Below this, only
/// the keyboard's search key searches: two letters match half the catalogue, and
/// every keystroke would be a wasted request.
private const val LIVE_SEARCH_FROM = 3

/// How long to wait after the last keystroke before asking. Long enough that
/// typing a word is one request rather than six, short enough not to feel slow.
private const val TYPING_PAUSE_MS = 180L

/// Sources that are a whole collection in themselves, with nothing to type in.
private val SELF_CONTAINED = setOf("wave", "likes", "offline")

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val settings = Settings(this)
        setContent {
            MaterialTheme(
                colorScheme = darkColorScheme(
                    primary = Accent,
                    onPrimary = AccentInk,
                    secondaryContainer = Surface3,
                    background = Bg,
                    surface = Bg,
                    surfaceVariant = Surface2,
                    surfaceContainer = Surface1,
                    onSurfaceVariant = Dim,
                    outline = Surface3,
                ),
            ) {
                Surface(modifier = Modifier.fillMaxSize(), color = Bg) { App(settings) }
            }
        }
    }
}

/// The four pages behind the bottom bar.
private enum class Screen(val label: String, val icon: ImageVector) {
    HOME("Главная", AppIcons.Note),
    QUEUE("Очередь", AppIcons.Queue),
    LIBRARY("Коллекция", AppIcons.Library),
    ROOM("Комната", AppIcons.Devices),
}

/// What every page needs to act on the room, gathered so the pages take one
/// argument instead of a dozen.
private class Room(
    val snapshot: Snapshot?,
    val running: Boolean,
    val likedIds: Set<String>,
    val scope: CoroutineScope,
    /// Runs something in a room, raising one on this phone first if there is none.
    val inRoom: (() -> Unit) -> Unit,
) {
    val canDrive: Boolean get() = running

    fun mark(trackId: String): Mark = markFor(snapshot, trackId)

    fun like(track: TrackInfo) {
        val liked = track.trackId in likedIds
        scope.launch { Commands.like(track, !liked)?.let { SyncHolder.say(it) } }
    }

    /// The row already holds the whole track, so nothing is asked of Yandex here
    /// — see `Commands.queueTracks`.
    fun enqueue(tracks: List<TrackInfo>, replace: Boolean = false) {
        inRoom {
            scope.launch {
                Commands.queueTracks(tracks, replace).onFailure { SyncHolder.say(it.message) }
            }
        }
    }

    fun send(action: String) {
        scope.launch { Commands.send(action)?.let { SyncHolder.say(it) } }
    }

    fun load(kind: String, value: String, replace: Boolean) {
        inRoom {
            scope.launch {
                Commands.queueFrom(kind, value, replace)
                    .onSuccess { count ->
                        SyncHolder.say(
                            when {
                                kind == "wave" && replace -> "волна включена"
                                kind == "wave" -> "волна продолжит очередь"
                                replace -> "играю: ${tracksWord(count)}"
                                else -> "добавлено в очередь: ${tracksWord(count)}"
                            },
                        )
                    }
                    .onFailure { SyncHolder.say(it.message) }
            }
        }
    }
}

@Composable
private fun App(settings: Settings) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()

    val snapshot by SyncHolder.snapshot.collectAsStateWithLifecycle()
    val running by SyncHolder.running.collectAsStateWithLifecycle()
    val message by SyncHolder.message.collectAsStateWithLifecycle()

    var screen by rememberSaveable { mutableStateOf(if (running) Screen.HOME else Screen.ROOM) }
    var playerOpen by rememberSaveable { mutableStateOf(false) }
    // Lifted out of the collection page so the home tiles can open a given view
    // of it: «Мне нравится» is a tile there and a tab here.
    var libraryTab by rememberSaveable { mutableStateOf(LibraryTab.ALL) }
    var library by remember { mutableStateOf<Library?>(null) }
    var likes by remember { mutableStateOf<Likes?>(null) }

    // A newer release on GitHub, asked about once per launch. Null when there is
    // none or GitHub could not be reached — either way nothing is shown.
    var update by remember { mutableStateOf<Release?>(null) }
    LaunchedEffect(Unit) { update = Updater.latest(context) }

    // A finished download changes the disk, and the cache revision is how the
    // core says so — cheaper than re-reading the list on every snapshot.
    LaunchedEffect(running, snapshot?.cacheRevision) {
        library = if (running) Commands.library().getOrNull() else null
    }

    // The same idea for «Мне нравится»: the core re-reads it from Yandex on
    // connecting and bumps this whenever it changes, so the list itself never has
    // to travel in a snapshot — it runs to over a thousand tracks.
    LaunchedEffect(running, snapshot?.likedRevision) {
        likes = if (running) Commands.likes().getOrNull() else null
    }

    // Into the room means music: go where it starts. Out of it means the room page,
    // since nothing else works until you are back in.
    LaunchedEffect(running) {
        if (running && screen == Screen.ROOM) screen = Screen.HOME
        if (!running) {
            screen = Screen.ROOM
            playerOpen = false
        }
    }

    // The player lives in the shade as well as on screen, and a notification needs
    // permission from Android 13 on.
    val askNotifications = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { }
    LaunchedEffect(Unit) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            askNotifications.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
    }

    // What was asked for before there was a room to ask it of. Held until the
    // room is up, then run in the order it was pressed — pressing «Моя волна»
    // outside a room should start the wave, not just a room with nothing playing
    // in it. A list rather than one slot: the search fires by itself after a pause
    // in typing, and a press that lands while the room is still coming up must
    // not wipe out the search, nor the other way round.
    var pending by remember { mutableStateOf<List<() -> Unit>>(emptyList()) }
    var raising by remember { mutableStateOf(false) }

    LaunchedEffect(running) {
        if (running) {
            raising = false
            val due = pending
            pending = emptyList()
            due.forEach { it() }
        }
    }

    // A room that never comes up — a busy port, a refused token — must not leave
    // presses waiting for it forever.
    LaunchedEffect(pending, running) {
        if (pending.isNotEmpty() && !running) {
            delay(30_000)
            if (!running) {
                pending = emptyList()
                raising = false
            }
        }
    }

    // A room comes up with whatever this phone has: a name and a password are
    // filled in when blank, and a Yandex token is not needed at all — without one
    // the room plays what is downloaded here and what the other devices offer.
    val inRoom: (() -> Unit) -> Unit = { action ->
        if (running) {
            action()
        } else {
            pending = pending + action
            if (!raising) {
                raising = true
                settings.prepareForSolo()
                SyncHolder.say("поднимаю комнату на этом телефоне…")
                SyncService.start(context, settings.configJson(host = true), true)
            }
        }
    }

    val room = Room(snapshot, running, likes?.ids ?: emptySet(), scope, inRoom)

    BackHandler(enabled = playerOpen) { playerOpen = false }

    Box(Modifier.fillMaxSize()) {
        Scaffold(
            containerColor = Bg,
            bottomBar = {
                Column(Modifier.background(Bg)) {
                    Toast(message)
                    if (running && snapshot?.track != null) {
                        MiniPlayer(room, onOpen = { playerOpen = true })
                    }
                    BottomNav(screen) { screen = it }
                }
            },
        ) { insets ->
            Box(
                Modifier
                    .fillMaxSize()
                    .padding(insets),
            ) {
                when (screen) {
                    Screen.HOME -> HomeScreen(
                        room = room,
                        library = library,
                        likes = likes,
                        update = update,
                        onRoom = { screen = Screen.ROOM },
                        // A tile opens its view of the collection. Playing is the
                        // round button there, not the tile: the tile is a way in
                        // to the list, the way a playlist card is anywhere else.
                        onCollection = { tab ->
                            libraryTab = tab
                            screen = Screen.LIBRARY
                            // The lists live in the core, so they need a room to
                            // be read at all — raise one while the page opens.
                            room.inRoom {}
                        },
                    )

                    Screen.QUEUE -> QueueScreen(room)
                    Screen.LIBRARY -> LibraryScreen(
                        room = room,
                        library = library,
                        likes = likes,
                        tab = libraryTab,
                        onTab = { libraryTab = it },
                        raising = raising,
                        onChanged = { library = it },
                    )
                    Screen.ROOM -> RoomScreen(
                        settings,
                        snapshot,
                        running,
                        update = update,
                        onUpdate = { update = it },
                    ) { host ->
                        val absent = settings.missing
                        if (absent.isEmpty()) {
                            SyncService.start(context, settings.configJson(host), host)
                        } else {
                            SyncHolder.say("не заполнено: ${absent.joinToString(", ")}")
                        }
                    }
                }
            }
        }

        AnimatedVisibility(
            visible = playerOpen && snapshot?.track != null,
            enter = slideInVertically(tween(320, easing = FastOutSlowInEasing)) { it },
            exit = slideOutVertically(tween(260)) { it },
        ) {
            FullPlayer(
                room = room,
                onClose = { playerOpen = false },
                onQueue = {
                    playerOpen = false
                    screen = Screen.QUEUE
                },
            )
        }
    }
}

// ---------------------------------------------------------------- navigation

@Composable
private fun BottomNav(current: Screen, onSelect: (Screen) -> Unit) {
    NavigationBar(containerColor = Bg, tonalElevation = 0.dp) {
        for (screen in Screen.entries) {
            NavigationBarItem(
                selected = screen == current,
                onClick = { onSelect(screen) },
                icon = { Icon(screen.icon, contentDescription = null, modifier = Modifier.size(24.dp)) },
                label = { Text(screen.label, fontSize = 11.sp) },
                colors = NavigationBarItemDefaults.colors(
                    selectedIconColor = Accent,
                    selectedTextColor = Color.White,
                    indicatorColor = Color.Transparent,
                    unselectedIconColor = Dim,
                    unselectedTextColor = Dim,
                ),
            )
        }
    }
}

/// What the core and the commands have to say, above the player for a few
/// seconds. On the room page it also stays in the status block.
@Composable
private fun Toast(message: String?) {
    var shown by remember { mutableStateOf<String?>(null) }
    LaunchedEffect(message) {
        // A cleared message clears the toast too: «подключаюсь…» is withdrawn
        // this way once the room answers, and must not hang on until the timer.
        if (message.isNullOrBlank()) {
            shown = null
            return@LaunchedEffect
        }
        shown = message
        delay(6_000)
        shown = null
    }
    AnimatedVisibility(visible = shown != null, enter = fadeIn(), exit = fadeOut()) {
        Text(
            shown ?: "",
            color = Color.White,
            style = MaterialTheme.typography.bodyMedium,
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 12.dp, vertical = 4.dp)
                .clip(RoundedCornerShape(12.dp))
                .background(Surface3)
                .clickable { shown = null }
                .padding(horizontal = 14.dp, vertical = 10.dp),
        )
    }
}

// ---------------------------------------------------------------- home

@Composable
private fun HomeScreen(
    room: Room,
    library: Library?,
    likes: Likes?,
    update: Release?,
    onRoom: () -> Unit,
    onCollection: (LibraryTab) -> Unit,
) {
    var query by rememberSaveable { mutableStateOf("") }
    var results by remember { mutableStateOf(emptyList<TrackInfo>()) }
    var suggest by remember { mutableStateOf<Suggest?>(null) }
    var kind by rememberSaveable { mutableStateOf("album") }
    var source by rememberSaveable { mutableStateOf("") }

    fun search(text: String = query) {
        if (text.isBlank()) return
        suggest = null
        // Searching goes through the core, so it needs a room like everything else.
        room.inRoom {
            room.scope.launch {
                Commands.search(text)
                    .onSuccess {
                        results = it
                        if (it.isEmpty()) SyncHolder.say("ничего не найдено")
                    }
                    .onFailure { error -> SyncHolder.say(error.message) }
            }
        }
    }

    // Restarted on every keystroke, which is what makes the pause a pause: the
    // previous run is cancelled before it asks for anything. Below
    // [LIVE_SEARCH_FROM] characters nothing is asked at all — the search key on
    // the keyboard still works.
    LaunchedEffect(query) {
        val typed = query.trim()
        if (typed.length < LIVE_SEARCH_FROM) {
            suggest = null
            return@LaunchedEffect
        }
        delay(TYPING_PAUSE_MS)
        search(typed)
        // Suggestions are decoration, so unlike the search they do not raise a
        // room of their own: they appear once there is one.
        suggest = Commands.suggest(typed).takeIf { !it.isEmpty }
    }

    LazyColumn(
        modifier = Modifier.fillMaxSize(),
        contentPadding = PaddingValues(horizontal = 16.dp, vertical = 12.dp),
        verticalArrangement = Arrangement.spacedBy(14.dp),
    ) {
        item { Logo(room) }

        update?.let { release -> item(key = "update") { UpdateCard(release) } }

        if (!room.running) {
            item {
                Card {
                    Text("Вы не в комнате", fontWeight = FontWeight.Bold, fontSize = 18.sp)
                    Text(
                        "Музыка играет только в комнате — одновременно на всех её устройствах.",
                        color = Dim,
                        style = MaterialTheme.typography.bodyMedium,
                    )
                    Spacer(Modifier.height(4.dp))
                    PrimaryButton("Войти в комнату") { onRoom() }
                }
            }
        }

        item {
            SearchField(
                value = query,
                onValue = { query = it },
                enabled = true,
                onClear = { query = ""; suggest = null; results = emptyList() },
                onSearch = { search() },
            )
        }

        // The dropdown Yandex draws while you type: its best guess, then the
        // queries to try. Part of the page rather than a floating panel — on a
        // phone there is nothing to float over.
        suggest?.let { found ->
            found.best?.let { best ->
                item(key = "best") {
                    SuggestBest(best) { query = best.query; search(best.query) }
                }
            }
            items(found.suggestions, key = { "s$it" }) { line ->
                SuggestLine(line) { query = line; search(line) }
            }
        }

        if (results.isNotEmpty()) {
            item { SectionTitle("Результаты поиска", "клик добавит трек в конец очереди") }
            itemsIndexed(results, key = { index, track -> "r${track.trackId}$index" }) { _, track ->
                TrackRow(
                    track = track,
                    room = room,
                    onClick = { room.enqueue(listOf(track)) },
                )
            }
        }

        item {
            WaveHero(
                // Not gated on being in a room: pressing it raises one here.
                enabled = true,
                playing = room.snapshot?.playing == true,
                station = room.snapshot?.station,
                feeding = room.snapshot?.feeding == true,
                onPlay = { room.load("wave", "", replace = true) },
                onStop = { room.send("stop_wave") },
            )
        }

        item {
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                Tile(
                    title = "Мне нравится",
                    meta = likes?.tracks?.size?.let { if (it > 0) tracksWord(it) else "пока пусто" } ?: "—",
                    art = LikesGradient,
                    icon = AppIcons.Heart,
                    modifier = Modifier.weight(1f),
                ) { onCollection(LibraryTab.LIKES) }
                Tile(
                    title = "Скачанное",
                    meta = library?.tracks?.size?.let { if (it > 0) tracksWord(it) else "пока пусто" } ?: "—",
                    art = OfflineGradient,
                    icon = AppIcons.Download,
                    modifier = Modifier.weight(1f),
                ) { onCollection(LibraryTab.ALL) }
            }
        }

        item {
            SourceCard(
                kind = kind,
                onKind = { kind = it },
                value = source,
                onValue = { source = it },
                enabled = true,
                onLoad = { replace -> room.load(kind, source, replace) },
            )
        }
    }
}

@Composable
private fun Logo(room: Room) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        Box(
            Modifier
                .size(32.dp)
                .clip(CircleShape)
                .background(Brush.sweepGradient(listOf(Accent, Color(0xFFFF7A2E), Color(0xFFFF3D6E), Accent))),
            contentAlignment = Alignment.Center,
        ) {
            Icon(AppIcons.Note, null, tint = Color.Black, modifier = Modifier.size(18.dp))
        }
        Spacer(Modifier.width(10.dp))
        Text("ym-sync", color = Accent, fontWeight = FontWeight.ExtraBold, fontSize = 22.sp)
        Spacer(Modifier.weight(1f))
        room.snapshot?.let { snapshot ->
            Row(
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier
                    .clip(CircleShape)
                    .background(Surface1)
                    .padding(horizontal = 10.dp, vertical = 6.dp),
            ) {
                Box(
                    Modifier
                        .size(8.dp)
                        .clip(CircleShape)
                        .background(if (snapshot.connected) Good else Bad),
                )
                Spacer(Modifier.width(6.dp))
                Text("${snapshot.peers}", color = Color.White, fontSize = 13.sp)
            }
        }
    }
}

/// «Моя волна»: slow coloured blobs, as in the Yandex app. They drift faster
/// while music plays.
@Composable
private fun WaveHero(
    enabled: Boolean,
    playing: Boolean,
    station: String?,
    feeding: Boolean,
    onPlay: () -> Unit,
    onStop: () -> Unit,
) {
    val transition = rememberInfiniteTransition(label = "wave")
    val period = if (playing) 6_000 else 14_000
    val phase by transition.animateFloat(
        initialValue = 0f,
        targetValue = (2 * Math.PI).toFloat(),
        animationSpec = infiniteRepeatable(tween(period, easing = LinearEasing), RepeatMode.Restart),
        label = "phase",
    )

    Box(
        modifier = Modifier
            .fillMaxWidth()
            .height(210.dp)
            .clip(RoundedCornerShape(24.dp))
            .background(Color(0xFF09090B))
            .clickable(enabled = enabled, onClick = onPlay),
        contentAlignment = Alignment.Center,
    ) {
        Canvas(Modifier.fillMaxSize()) {
            val blobs = listOf(
                Triple(Color(0xFFFF3D6E), Offset(0.3f, 0.2f), 0f),
                Triple(Color(0xFFFFB800), Offset(0.72f, 0.45f), 2.1f),
                Triple(Color(0xFF8A3DFF), Offset(0.45f, 0.95f), 4.2f),
            )
            for ((color, anchor, shift) in blobs) {
                val center = Offset(
                    size.width * anchor.x + kotlin.math.cos(phase + shift) * size.width * 0.08f,
                    size.height * anchor.y + kotlin.math.sin(phase + shift) * size.height * 0.12f,
                )
                val radius = size.minDimension * (0.75f + 0.1f * kotlin.math.sin(phase * 2 + shift))
                drawCircle(
                    brush = Brush.radialGradient(
                        listOf(color.copy(alpha = 0.85f), color.copy(alpha = 0f)),
                        center = center,
                        radius = radius,
                    ),
                    radius = radius,
                    center = center,
                )
            }
        }
        Column(horizontalAlignment = Alignment.CenterHorizontally) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(AppIcons.Play, null, tint = Color.White, modifier = Modifier.size(34.dp))
                Spacer(Modifier.width(8.dp))
                Text("Моя волна", color = Color.White, fontSize = 32.sp, fontWeight = FontWeight.ExtraBold)
            }
            Text(
                "бесконечный поток по вашему вкусу",
                color = Color.White.copy(alpha = 0.75f),
                style = MaterialTheme.typography.bodyMedium,
            )
            // The wave is room-wide, but only its feeder can refill it — so only
            // the feeder is offered the off switch.
            if (station != null) {
                Spacer(Modifier.height(12.dp))
                Text(
                    if (feeding) "играет · выключить" else "волну ведёт другое устройство",
                    color = Color.White,
                    fontSize = 13.sp,
                    modifier = Modifier
                        .clip(CircleShape)
                        .background(Color.Black.copy(alpha = 0.4f))
                        .clickable(enabled = feeding, onClick = onStop)
                        .padding(horizontal = 14.dp, vertical = 6.dp),
                )
            }
        }
    }
}

@Composable
private fun Tile(
    title: String,
    meta: String,
    art: Brush,
    icon: ImageVector,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    onClick: () -> Unit,
) {
    // Art above the text rather than beside it: two tiles share a phone's width,
    // and a title squeezed beside a picture is cut to «Мне нрав…».
    Column(
        modifier = modifier
            .clip(RoundedCornerShape(16.dp))
            .background(Surface1)
            .clickable(enabled = enabled, onClick = onClick)
            .padding(12.dp),
    ) {
        Box(
            Modifier
                .size(48.dp)
                .clip(RoundedCornerShape(12.dp))
                .background(art),
            contentAlignment = Alignment.Center,
        ) {
            Icon(icon, null, tint = Color.White, modifier = Modifier.size(24.dp))
        }
        Spacer(Modifier.height(10.dp))
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                title,
                fontWeight = FontWeight.Bold,
                fontSize = 16.sp,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f, fill = false),
            )
            Icon(AppIcons.Chevron, null, tint = Dim, modifier = Modifier.size(18.dp))
        }
        Text(meta, color = Dim, fontSize = 12.sp, maxLines = 1, overflow = TextOverflow.Ellipsis)
    }
}

/// The row at the top of the dropdown: a picture, a name and what it is.
@Composable
private fun SuggestBest(best: BestMatch, onClick: () -> Unit) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(12.dp))
            .clickable(onClick = onClick)
            .padding(8.dp),
    ) {
        CoverArt(
            url = best.coverUrl(100),
            size = 48.dp,
            // An artist is a face, an album is a cover: the shape says which.
            corner = if (best.kind == "artist") 24.dp else 8.dp,
        )
        Spacer(Modifier.width(12.dp))
        Column(Modifier.weight(1f)) {
            Text(best.name, fontWeight = FontWeight.SemiBold, maxLines = 1, overflow = TextOverflow.Ellipsis)
            Text(
                if (best.subtitle.isEmpty()) best.kindLabel else "${best.kindLabel} · ${best.subtitle}",
                color = Dim,
                fontSize = 13.sp,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
    }
}

/// One suggested query.
@Composable
private fun SuggestLine(line: String, onClick: () -> Unit) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(12.dp))
            .clickable(onClick = onClick)
            .padding(horizontal = 10.dp, vertical = 10.dp),
    ) {
        Icon(AppIcons.Search, null, tint = Dim, modifier = Modifier.size(18.dp))
        Spacer(Modifier.width(12.dp))
        Text(line, maxLines = 1, overflow = TextOverflow.Ellipsis)
    }
}

@Composable
private fun SearchField(
    value: String,
    onValue: (String) -> Unit,
    enabled: Boolean,
    onClear: () -> Unit,
    onSearch: () -> Unit,
) {
    TextField(
        value = value,
        onValueChange = onValue,
        enabled = enabled,
        singleLine = true,
        placeholder = { Text("Трек, альбом, исполнитель", color = Dim) },
        leadingIcon = { Icon(AppIcons.Search, null, tint = Dim) },
        trailingIcon = {
            if (value.isNotEmpty()) {
                IconButton(onClick = onClear) {
                    Icon(AppIcons.Close, "очистить", tint = Dim, modifier = Modifier.size(20.dp))
                }
            }
        },
        keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
        keyboardActions = KeyboardActions(onSearch = { onSearch() }),
        shape = CircleShape,
        colors = TextFieldDefaults.colors(
            focusedContainerColor = Surface2,
            unfocusedContainerColor = Surface2,
            disabledContainerColor = Surface1,
            focusedIndicatorColor = Color.Transparent,
            unfocusedIndicatorColor = Color.Transparent,
            disabledIndicatorColor = Color.Transparent,
            cursorColor = Accent,
        ),
        modifier = Modifier.fillMaxWidth(),
    )
}

/// Loading by link or number: an album, a playlist, a track, or a search taken
/// whole. The wave and «Мне нравится» have their own tiles above.
@Composable
private fun SourceCard(
    kind: String,
    onKind: (String) -> Unit,
    value: String,
    onValue: (String) -> Unit,
    enabled: Boolean,
    onLoad: (Boolean) -> Unit,
) {
    val needsValue = kind !in SELF_CONTAINED
    val ready = enabled && (!needsValue || value.isNotBlank())

    Card {
        Text("По ссылке или номеру", fontWeight = FontWeight.Bold, fontSize = 17.sp)
        FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            listOf(
                "album" to "альбом",
                "playlist" to "плейлист",
                "track" to "трек",
                "search" to "поиск",
            ).forEach { (id, label) ->
                Chip(label, selected = kind == id) { onKind(id) }
            }
        }
        OutlinedTextField(
            value = value,
            onValueChange = onValue,
            placeholder = {
                Text(
                    when (kind) {
                        "album" -> "5307396 или ссылка на альбом"
                        "playlist" -> "логин/номер или ссылка"
                        "track" -> "38633712 или ссылка на трек"
                        else -> "кино группа крови"
                    },
                    color = Dim,
                )
            },
            singleLine = true,
            shape = RoundedCornerShape(12.dp),
            colors = fieldColors(),
            modifier = Modifier.fillMaxWidth(),
        )
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            GhostButton("В очередь", AppIcons.Add, enabled = ready) { onLoad(false) }
            PrimaryButton("Играть", AppIcons.Play, enabled = ready) { onLoad(true) }
        }
    }
}

// ---------------------------------------------------------------- queue

@Composable
private fun QueueScreen(room: Room) {
    val snapshot = room.snapshot
    val queue = snapshot?.queue ?: emptyList()
    val track = snapshot?.track
    val cover = rememberCover(track, 400)
    val tint by animateColorAsState(cover?.tint ?: Surface2, tween(600), label = "tint")

    LazyColumn(
        modifier = Modifier.fillMaxSize(),
        contentPadding = PaddingValues(bottom = 12.dp),
    ) {
        item {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .background(Brush.verticalGradient(listOf(tint.copy(alpha = 0.85f), Bg)))
                    .padding(16.dp),
                verticalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                Text("СЕЙЧАС ИГРАЕТ", color = Color.White.copy(alpha = 0.7f), fontSize = 12.sp, fontWeight = FontWeight.Bold)
                Row(verticalAlignment = Alignment.CenterVertically) {
                    CoverBox(track, size = 112.dp, corner = 12.dp, pixels = 400)
                    Spacer(Modifier.width(16.dp))
                    Column(Modifier.weight(1f)) {
                        Text(
                            track?.title ?: "Тишина",
                            fontSize = 22.sp,
                            fontWeight = FontWeight.ExtraBold,
                            maxLines = 2,
                            overflow = TextOverflow.Ellipsis,
                        )
                        Text(
                            track?.artist ?: "поставьте что-нибудь в очередь",
                            color = Color.White.copy(alpha = 0.8f),
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                        queueNote(snapshot)?.let {
                            Text(it, color = Color.White.copy(alpha = 0.6f), fontSize = 12.sp)
                        }
                    }
                }
                // Keeping music here is per-device, so it sits with the queue
                // rather than with the room's controls.
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    GhostButton("Трек", AppIcons.Download, enabled = room.canDrive && track != null) {
                        room.scope.launch { Commands.send("download") { put("queue", false) } }
                    }
                    GhostButton("Всю очередь", AppIcons.Download, enabled = room.canDrive && queue.isNotEmpty()) {
                        room.scope.launch { Commands.send("download") { put("queue", true) } }
                    }
                    if ((snapshot?.downloadQueue ?: 0) > 0) {
                        TextButton(onClick = { room.send("cancel_downloads") }) {
                            Text("отменить", color = Color.White)
                        }
                    }
                }
            }
        }

        item {
            Box(Modifier.padding(horizontal = 16.dp, vertical = 8.dp)) {
                SectionTitle("Далее", if (queue.isNotEmpty()) tracksWord(queue.size) else null)
            }
        }

        if (queue.isEmpty()) {
            item { Empty("Очередь пуста — поставьте что-нибудь с главной") }
        }

        items(queue.size, key = { "q${queue[it].trackId}$it" }) { index ->
            Box(Modifier.padding(horizontal = 8.dp)) {
                TrackRow(
                    track = queue[index],
                    room = room,
                    current = index == (snapshot?.index ?: -1),
                    onClick = { room.scope.launch { Commands.send("index") { put("index", index) } } },
                )
            }
        }
    }
}

private fun queueNote(snapshot: Snapshot?): String? {
    snapshot ?: return null
    return buildList {
        if (snapshot.queue.isNotEmpty()) add("${snapshot.index + 1} из ${snapshot.queue.size}")
        if (snapshot.loading) add("загрузка…")
        snapshot.notice?.let { add(it) }
        if (snapshot.downloading != null) {
            add(if (snapshot.downloadQueue > 0) "скачиваю, ещё ${snapshot.downloadQueue}" else "скачиваю")
        }
    }.joinToString(" · ").ifEmpty { null }
}

// ---------------------------------------------------------------- collection

/// The three views of the collection.
private enum class LibraryTab(val label: String, val title: String) {
    ALL("всё скачанное", "Скачанное"),
    LIKES("мне нравится", "Мне нравится"),
    ALBUMS("по альбомам", "По альбомам"),
}

@Composable
private fun LibraryScreen(
    room: Room,
    library: Library?,
    likes: Likes?,
    tab: LibraryTab,
    onTab: (LibraryTab) -> Unit,
    /// A room is coming up for these lists; they are read from the core.
    raising: Boolean,
    onChanged: (Library?) -> Unit,
) {
    val downloaded = library?.tracks ?: emptyList()
    val likedTracks = likes?.tracks ?: emptyList()
    val shown: List<TrackInfo> = when (tab) {
        LibraryTab.LIKES -> likedTracks
        else -> downloaded.map { it.track }
    }

    LazyColumn(
        modifier = Modifier.fillMaxSize(),
        contentPadding = PaddingValues(horizontal = 8.dp, vertical = 16.dp),
        verticalArrangement = Arrangement.spacedBy(2.dp),
    ) {
        item {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier.padding(horizontal = 8.dp),
            ) {
                Box(
                    Modifier
                        .size(96.dp)
                        .shadow(12.dp, RoundedCornerShape(14.dp))
                        .clip(RoundedCornerShape(14.dp))
                        .background(
                            when (tab) {
                                LibraryTab.ALL -> OfflineGradient
                                LibraryTab.LIKES -> LikesGradient
                                LibraryTab.ALBUMS -> AlbumsGradient
                            },
                        ),
                    contentAlignment = Alignment.Center,
                ) {
                    Icon(
                        when (tab) {
                            LibraryTab.ALL -> AppIcons.Download
                            LibraryTab.LIKES -> AppIcons.Heart
                            LibraryTab.ALBUMS -> AppIcons.Library
                        },
                        null,
                        tint = Color.White,
                        modifier = Modifier.size(44.dp),
                    )
                }
                Spacer(Modifier.width(16.dp))
                Column(Modifier.weight(1f)) {
                    Text("КОЛЛЕКЦИЯ", color = Dim, fontSize = 12.sp, fontWeight = FontWeight.Bold)
                    Text(tab.title, fontSize = 26.sp, fontWeight = FontWeight.ExtraBold)
                    Text(
                        when (tab) {
                            LibraryTab.LIKES -> tracksWord(likedTracks.size)
                            else -> "${tracksWord(downloaded.size)} · ${librarySize(library)}"
                        },
                        color = Dim,
                        fontSize = 13.sp,
                    )
                }
            }
        }

        item {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(10.dp),
                modifier = Modifier.padding(horizontal = 8.dp, vertical = 12.dp),
            ) {
                // Playing the whole collection is how you listen with no internet:
                // every one of the downloads comes off the disk.
                RoundPlay(enabled = room.canDrive && shown.isNotEmpty()) {
                    room.enqueue(shown, replace = true)
                }
                GhostButton("В очередь", AppIcons.Add, enabled = room.canDrive && shown.isNotEmpty()) {
                    room.enqueue(shown)
                }
                ImportButton(room, onChanged)
                // Only «Мне нравится» has anything to re-read: the other two views
                // are this disk, and the core watches that.
                if (tab == LibraryTab.LIKES) {
                    IconButton(onClick = { room.send("refresh_likes") }, enabled = room.canDrive) {
                        Icon(AppIcons.Refresh, "перечитать с Яндекса", tint = Color.White)
                    }
                }
            }
        }

        item {
            // Scrolls rather than wraps: a chip broken over three lines reads as
            // «по / альбома / м».
            Row(
                horizontalArrangement = Arrangement.spacedBy(8.dp),
                modifier = Modifier
                    .horizontalScroll(rememberScrollState())
                    .padding(horizontal = 8.dp, vertical = 4.dp),
            ) {
                for (option in LibraryTab.entries) {
                    Chip(option.label, selected = tab == option) { onTab(option) }
                }
            }
        }

        if (!room.running) {
            item {
                Empty(
                    if (raising) {
                        "поднимаю комнату на этом телефоне…"
                    } else {
                        "Коллекция откроется, когда вы войдёте в комнату"
                    },
                )
            }
            return@LazyColumn
        }

        when (tab) {
            LibraryTab.ALL -> {
                if (downloaded.isEmpty()) {
                    item { Empty("Здесь пока пусто — скачивайте треки со страницы очереди") }
                }
                items(downloaded.size, key = { "c${downloaded[it].track.trackId}" }) { index ->
                    DownloadedRow(downloaded[index], room, onChanged)
                }
            }

            LibraryTab.LIKES -> {
                if (likedTracks.isEmpty()) {
                    item { Empty("Список пуст или ещё не загружен — обновите, когда будет интернет") }
                }
                itemsIndexed(likedTracks, key = { index, track -> "l${track.trackId}$index" }) { _, track ->
                    TrackRow(track = track, room = room, onClick = { room.enqueue(listOf(track)) })
                }
            }

            LibraryTab.ALBUMS -> albums(downloaded, room, onChanged)
        }
    }
}

private fun LazyListScope.albums(
    downloaded: List<CachedTrack>,
    room: Room,
    onChanged: (Library?) -> Unit,
) {
    for ((album, entries) in albumGroups(downloaded)) {
        item(key = "a$album") {
            // The header queues the album; that is what makes this view worth
            // having over a flat list.
            Row(
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(top = 14.dp)
                    .clip(RoundedCornerShape(12.dp))
                    .clickable(enabled = room.canDrive) { room.enqueue(entries.map { it.track }) }
                    .padding(8.dp),
            ) {
                CoverBox(entries.firstOrNull { it.track.coverUri != null }?.track, size = 56.dp, corner = 10.dp)
                Spacer(Modifier.width(12.dp))
                Column(Modifier.weight(1f)) {
                    Text(album, fontWeight = FontWeight.Bold, fontSize = 17.sp, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    Text(tracksWord(entries.size), color = Dim, fontSize = 13.sp)
                }
                Icon(AppIcons.Add, "в очередь", tint = Dim)
            }
        }
        items(entries.size, key = { "ac${entries[it].track.trackId}" }) { index ->
            DownloadedRow(entries[index], room, onChanged)
        }
    }
    if (downloaded.any { it.track.album == null }) {
        item { Empty("у части треков название альбома ещё не загружено — нужен интернет") }
    }
}

/// Adds files from the phone to the downloads.
///
/// The system picker hands back content URIs, which only this app may open, so the
/// bytes are read here and passed to the core. It cannot open what the Yandex
/// Music app downloaded: that app keeps its offline tracks encrypted.
@Composable
private fun ImportButton(room: Room, onChanged: (Library?) -> Unit) {
    val context = LocalContext.current
    var working by remember { mutableStateOf(false) }

    val pick = rememberLauncherForActivityResult(
        ActivityResultContracts.OpenMultipleDocuments(),
    ) { uris ->
        if (uris.isEmpty()) return@rememberLauncherForActivityResult
        room.inRoom {
            // Set here, once the import is actually running, and not before the
            // room is up: a room that never comes up drops this action, and a flag
            // raised outside it would then never come down — the button would
            // stay disabled for good.
            working = true
            room.scope.launch {
                var added = 0
                var known = 0
                var failed = 0
                try {
                    for (uri in uris) {
                        val name = displayName(context, uri)
                        val data = withContext(Dispatchers.IO) {
                            runCatching {
                                context.contentResolver.openInputStream(uri)?.use { it.readBytes() }
                            }.getOrNull()
                        }
                        if (data == null) {
                            failed++
                            continue
                        }
                        Commands.importTrack(name, data)
                            .onSuccess { new -> if (new) added++ else known++ }
                            .onFailure { failed++ }
                    }
                    onChanged(Commands.library().getOrNull())
                } finally {
                    working = false
                }
                SyncHolder.say(
                    listOfNotNull(
                        "добавлено: $added",
                        "уже было: $known".takeIf { known > 0 },
                        "не вышло: $failed".takeIf { failed > 0 },
                    ).joinToString(" · "),
                )
            }
        }
    }

    IconButton(
        onClick = { pick.launch(arrayOf("audio/*")) },
        enabled = !working,
    ) {
        Icon(AppIcons.Add, "добавить файлы с телефона", tint = Color.White)
    }
}

/// The file's own name, for when its tags have none.
private fun displayName(context: android.content.Context, uri: android.net.Uri): String {
    val projection = arrayOf(android.provider.OpenableColumns.DISPLAY_NAME)
    context.contentResolver.query(uri, projection, null, null, null)?.use { cursor ->
        if (cursor.moveToFirst() && !cursor.isNull(0)) return cursor.getString(0)
    }
    return uri.lastPathSegment ?: "файл"
}

/// A downloaded track: plays off the disk, can be hearted, and carries the delete
/// that the other lists have no business offering.
@Composable
private fun DownloadedRow(entry: CachedTrack, room: Room, onChanged: (Library?) -> Unit) {
    TrackRow(
        track = entry.track,
        room = room,
        trailing = formatSize(entry.bytes),
        // The metadata was stored beside the audio, so this needs neither Yandex
        // nor a network at all.
        onClick = { room.enqueue(listOf(entry.track)) },
        extra = {
            IconButton(
                onClick = {
                    room.scope.launch {
                        Commands.send("forget") { put("track_id", entry.track.trackId) }
                        onChanged(Commands.library().getOrNull())
                    }
                },
            ) {
                Icon(AppIcons.Close, "удалить с устройства", tint = Dim, modifier = Modifier.size(20.dp))
            }
        },
    )
}

/// Downloads grouped by album: named albums first in alphabetical order, then
/// everything whose album name is not known yet, in one group at the end.
///
/// One group, not one per album id: with no name they would all be headed
/// «без названия альбома», and a dozen identical headers say less than a single
/// pile does. The names arrive with the next connection and they sort themselves.
private fun albumGroups(tracks: List<CachedTrack>): List<Pair<String, List<CachedTrack>>> {
    val groups = LinkedHashMap<String, MutableList<CachedTrack>>()
    for (entry in tracks) {
        groups.getOrPut(entry.track.album ?: UNNAMED_ALBUM) { mutableListOf() }.add(entry)
    }
    return groups.entries
        .map { (album, list) -> album to list.toList() }
        .sortedWith(compareBy({ if (it.first == UNNAMED_ALBUM) 1 else 0 }, { it.first }))
}

private const val UNNAMED_ALBUM = "без названия альбома"

private fun librarySize(library: Library?): String {
    val bytes = formatSize(library?.bytes ?: 0)
    val limit = library?.limitBytes ?: 0
    return if (limit > 0) "$bytes из ${formatSize(limit)}" else bytes
}

// ---------------------------------------------------------------- room

@Composable
private fun RoomScreen(
    settings: Settings,
    snapshot: Snapshot?,
    running: Boolean,
    update: Release?,
    onUpdate: (Release?) -> Unit,
    onEnter: (Boolean) -> Unit,
) {
    val context = LocalContext.current
    val message by SyncHolder.message.collectAsStateWithLifecycle()

    LazyColumn(
        modifier = Modifier.fillMaxSize(),
        contentPadding = PaddingValues(16.dp),
        verticalArrangement = Arrangement.spacedBy(14.dp),
    ) {
        item {
            Column {
                Text("СОВМЕСТНОЕ ПРОСЛУШИВАНИЕ", color = Dim, fontSize = 12.sp, fontWeight = FontWeight.Bold)
                Text("Комната", fontSize = 30.sp, fontWeight = FontWeight.ExtraBold)
                StatusLine(snapshot, message)
            }
        }

        if (running) {
            item {
                Button(
                    onClick = { SyncService.stop(context) },
                    colors = ButtonDefaults.buttonColors(containerColor = Bad.copy(alpha = 0.16f), contentColor = Color(0xFFFF8A8E)),
                ) {
                    Icon(AppIcons.Power, null, modifier = Modifier.size(18.dp))
                    Spacer(Modifier.width(8.dp))
                    Text("Отключиться", fontWeight = FontWeight.Bold)
                }
            }
        } else {
            // Two ways into a room, and the same two fields for both: a room is a
            // name and a password. Which button you press decides who holds it.
            item { RoomCard(settings, onEnter) }
        }

        item { SettingsCard(settings) }

        item { AboutCard(update, onUpdate) }
    }
}

/// The installed version, and a way to ask GitHub for a newer one by hand — the
/// check at launch is silent when it fails, so this is where to look when in doubt.
@Composable
private fun AboutCard(update: Release?, onUpdate: (Release?) -> Unit) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var checking by remember { mutableStateOf(false) }
    var answer by remember { mutableStateOf<String?>(null) }

    Card {
        Text("О приложении", fontWeight = FontWeight.Bold, fontSize = 17.sp)
        Text("ym-sync ${Updater.currentVersion(context)}", color = Dim)
        if (update != null) {
            UpdateCard(update)
        } else {
            GhostButton(if (checking) "Проверяю…" else "Проверить обновления", AppIcons.Refresh, enabled = !checking) {
                checking = true
                scope.launch {
                    val found = Updater.latest(context)
                    onUpdate(found)
                    answer = if (found == null) "это последняя версия — или GitHub сейчас недоступен" else null
                    checking = false
                }
            }
            answer?.let { Hint(it) }
        }
    }
}

/// A newer release: download it, then hand it to the system installer.
///
/// Kept here rather than started on its own: installing always asks the user, and
/// the first time also needs permission to install from this app, so the press
/// that starts it may need repeating after a trip to settings.
@Composable
private fun UpdateCard(release: Release) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var progress by remember { mutableStateOf<Float?>(null) }
    var downloaded by remember { mutableStateOf<java.io.File?>(null) }
    var problem by remember { mutableStateOf<String?>(null) }

    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(18.dp))
            .background(Accent.copy(alpha = 0.12f))
            .padding(14.dp),
    ) {
        Icon(AppIcons.Download, null, tint = Accent, modifier = Modifier.size(28.dp))
        Spacer(Modifier.width(12.dp))
        Column(Modifier.weight(1f)) {
            Text("Доступна ${release.version}", color = Accent, fontWeight = FontWeight.Bold)
            Text(
                problem ?: progress?.let { "скачиваю ${(it * 100).toInt()}%" } ?: "обновление с GitHub",
                color = Accent.copy(alpha = 0.8f),
                fontSize = 13.sp,
            )
        }
        PrimaryButton("Обновить", enabled = progress == null) {
            problem = null
            val ready = downloaded
            if (ready != null) {
                // Already downloaded: this is the press after allowing installs.
                problem = Updater.install(context, ready)
                return@PrimaryButton
            }
            progress = 0f
            scope.launch {
                runCatching { Updater.download(context, release) { progress = it } }
                    .onSuccess { file ->
                        downloaded = file
                        problem = Updater.install(context, file)
                    }
                    .onFailure { problem = "не скачалось: ${it.message}" }
                progress = null
            }
        }
    }
}

@Composable
private fun StatusLine(snapshot: Snapshot?, message: String?) {
    Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
        val text = when {
            snapshot == null -> "не подключено"
            else -> buildString {
                append("участников ${snapshot.peers}")
                snapshot.rttMs?.let { append(" · rtt $it мс") }
                if (snapshot.sharingPeers > 0) append(" · раздают ${snapshot.sharingPeers}")
                if (snapshot.downloading != null) {
                    append(
                        if (snapshot.downloadQueue > 0) {
                            " · скачиваю, ещё ${snapshot.downloadQueue}"
                        } else {
                            " · скачиваю"
                        },
                    )
                }
            }
        }
        Text(
            text,
            color = if (snapshot?.connected == true) Good else Dim,
            style = MaterialTheme.typography.bodyMedium,
        )
        // The address the others have to type in. Only known once the socket is
        // bound, which is why it comes from the snapshot and not the settings.
        snapshot?.hosting?.let {
            Text(
                "комната здесь · $it",
                color = Accent,
                style = MaterialTheme.typography.bodyMedium,
                modifier = Modifier
                    .padding(top = 4.dp)
                    .clip(CircleShape)
                    .background(Accent.copy(alpha = 0.12f))
                    .padding(horizontal = 10.dp, vertical = 4.dp),
            )
        }
        // What the engine has to say — a skipped track, a station that stopped
        // answering — would otherwise never reach the screen.
        snapshot?.notice?.let {
            Text(it, color = Dim, style = MaterialTheme.typography.bodySmall)
        }
        message?.let {
            Text(it, color = Bad, style = MaterialTheme.typography.bodySmall)
        }
    }
}

/**
 * Getting into a room: the name and the password, then the two ways in.
 *
 * «Подключиться» dials the address; «Хостить» runs the room's relay on this phone,
 * which is also what listening with no internet looks like — the queue and the
 * playhead need an authority, and with nothing to dial the phone becomes it.
 */
@Composable
private fun RoomCard(settings: Settings, onEnter: (Boolean) -> Unit) {
    var room by remember { mutableStateOf(settings.room) }
    var password by remember { mutableStateOf(settings.password) }
    var relay by remember { mutableStateOf(settings.relay) }
    var searching by remember { mutableStateOf(false) }
    var found by remember { mutableStateOf(emptyList<FoundRoom>()) }
    val scope = rememberCoroutineScope()

    Column(verticalArrangement = Arrangement.spacedBy(14.dp)) {
        Card {
            Text("Комната", fontWeight = FontWeight.Bold, fontSize = 17.sp)
            Field(room, { room = it; settings.room = it }, "название комнаты")
            Field(password, { password = it; settings.password = it }, "пароль комнаты")
            Hint("Название и пароль должны совпадать у всех участников.")
        }

        Card {
            Text("Подключиться", fontWeight = FontWeight.Bold, fontSize = 17.sp)
            Field(relay, { relay = it; settings.relay = it }, "адрес комнаты")

            // Fills the address in from the network, so nobody has to read an IP off
            // another screen and type it in.
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                GhostButton(if (searching) "Ищу…" else "Найти в сети", AppIcons.Search, enabled = !searching) {
                    searching = true
                    scope.launch {
                        Commands.findRooms()
                            .onSuccess { rooms ->
                                found = rooms
                                SyncHolder.say(
                                    if (rooms.isEmpty()) {
                                        "в сети никто не отвечает — комнату держит " +
                                            "ymsync-relay или участник, нажавший «Хостить»"
                                    } else {
                                        "нашлось комнат: ${rooms.size}"
                                    },
                                )
                            }
                            .onFailure { SyncHolder.say(it.message) }
                        searching = false
                    }
                }
                PrimaryButton("Подключиться") { onEnter(false) }
            }

            found.forEach { candidate ->
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    modifier = Modifier
                        .fillMaxWidth()
                        .clip(RoundedCornerShape(10.dp))
                        .background(Surface2)
                        // A relay of another version would refuse us, so it is shown
                        // and disabled rather than hidden: that explains the situation.
                        .clickable(enabled = candidate.compatible) {
                            relay = candidate.relay
                            settings.relay = candidate.relay
                            // An empty name means that relay holds no rooms yet, so the
                            // name in the field is what will be created.
                            if (candidate.room.isNotEmpty()) {
                                room = candidate.room
                                settings.room = candidate.room
                            }
                            SyncHolder.say("вписал ${candidate.relay}")
                        }
                        .padding(12.dp),
                ) {
                    Icon(AppIcons.Devices, null, tint = if (candidate.compatible) Accent else Dim, modifier = Modifier.size(20.dp))
                    Spacer(Modifier.width(10.dp))
                    Column(Modifier.weight(1f)) {
                        Text(candidate.room.ifEmpty { "комнат пока нет" }, fontWeight = FontWeight.SemiBold)
                        Text(
                            buildString {
                                append(candidate.relay)
                                if (candidate.listeners > 0) append(" · ${candidate.listeners}")
                                if (!candidate.compatible) append(" · другая версия")
                            },
                            color = Dim,
                            fontSize = 12.sp,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                }
            }
        }

        Card {
            Text("Держать комнату здесь", fontWeight = FontWeight.Bold, fontSize = 17.sp)
            Hint(
                "Комнату будет держать этот телефон: адрес для остальных появится на этой " +
                    "странице. Так же выглядит и прослушивание скачанного без интернета.",
            )
            GhostButton("Хостить", AppIcons.Devices) { onEnter(true) }
        }
    }
}

@Composable
private fun SettingsCard(settings: Settings) {
    var yandexToken by remember { mutableStateOf(settings.yandexToken) }
    var autoCache by remember { mutableStateOf(settings.autoCache) }
    var cacheLimit by remember { mutableStateOf(settings.cacheLimitGb.toString()) }

    Card {
        Text("Настройки", fontWeight = FontWeight.Bold, fontSize = 17.sp)
        Field(yandexToken, { yandexToken = it; settings.yandexToken = it }, "токен Яндекса (этого устройства)", secret = true)
        Hint("У каждого устройства свой аккаунт с Плюсом. В эмуляторе релей на компьютере доступен как 10.0.2.2")

        HorizontalDivider(color = Surface3)

        SettingSwitch(
            label = "оставлять всё, что играет",
            hint = "иначе на телефоне остаётся только скачанное кнопками «Трек» и «Всю очередь».",
            checked = autoCache,
            onChange = { autoCache = it; settings.autoCache = it },
        )
        Field(
            cacheLimit,
            { typed ->
                cacheLimit = typed.filter { it.isDigit() || it == '.' }
                // 0 means no limit, and an unreadable value must not be silently
                // taken for one.
                settings.cacheLimitGb = cacheLimit.toFloatOrNull() ?: settings.cacheLimitGb
            },
            "лимит на скачанное, ГБ (0 — без лимита)",
        )
        Hint(
            "Эти два применятся при следующем подключении: ядро читает настройки при " +
                "старте. Файлы лежат в ${settings.cacheDir} и удаляются вместе с приложением.",
        )
    }
}

/// A switch with the sentence that explains what it costs, because none of these
/// are obvious from a label alone.
@Composable
private fun SettingSwitch(
    label: String,
    hint: String,
    checked: Boolean,
    onChange: (Boolean) -> Unit,
) {
    Column {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(label, modifier = Modifier.weight(1f))
            Switch(checked = checked, onCheckedChange = onChange)
        }
        Hint(hint)
    }
}

// ---------------------------------------------------------------- player

/// The strip above the bottom bar: what plays, and the two buttons you reach for
/// most. Pressing it opens the full player.
@Composable
private fun MiniPlayer(room: Room, onOpen: () -> Unit) {
    val snapshot = room.snapshot ?: return
    val track = snapshot.track ?: return
    val cover = rememberCover(track, 100)
    val tint by animateColorAsState(cover?.tint ?: Surface2, tween(600), label = "mini")
    val progress = if (snapshot.durationMs > 0) {
        (snapshot.positionMs.toFloat() / snapshot.durationMs).coerceIn(0f, 1f)
    } else {
        0f
    }

    Column(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 8.dp, vertical = 4.dp)
            .clip(RoundedCornerShape(14.dp))
            .background(Brush.horizontalGradient(listOf(tint.copy(alpha = 0.55f), Surface2)))
            .clickable(onClick = onOpen),
    ) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier.padding(start = 8.dp, top = 8.dp, bottom = 6.dp),
        ) {
            CoverBox(track, size = 44.dp, corner = 8.dp)
            Spacer(Modifier.width(10.dp))
            Column(Modifier.weight(1f)) {
                Text(track.title, fontWeight = FontWeight.SemiBold, maxLines = 1, overflow = TextOverflow.Ellipsis)
                Text(
                    if (snapshot.loading) "загрузка…" else track.artist,
                    color = Color.White.copy(alpha = 0.7f),
                    fontSize = 13.sp,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            // Beside the title rather than with the transport: the transport acts
            // on the room, this acts on the track and on this account.
            HeartButton(snapshot.trackLiked) {
                room.scope.launch { Commands.like(track, !snapshot.trackLiked)?.let { SyncHolder.say(it) } }
            }
            IconButton(onClick = { room.send("toggle") }, enabled = room.canDrive) {
                Icon(if (snapshot.playing) AppIcons.Pause else AppIcons.Play, "пауза / продолжить", tint = Color.White)
            }
            IconButton(onClick = { room.send("next") }, enabled = room.canDrive) {
                Icon(AppIcons.Next, "следующий", tint = Color.White)
            }
        }
        Box(
            Modifier
                .fillMaxWidth()
                .padding(horizontal = 10.dp)
                .height(2.dp)
                .clip(CircleShape)
                .background(Color.White.copy(alpha = 0.2f)),
        ) {
            Box(
                Modifier
                    .fillMaxHeight()
                    .fillMaxWidth(progress)
                    .background(Color.White),
            )
        }
        Spacer(Modifier.height(6.dp))
    }
}

/// The full-screen player, painted in its cover's colour as the Yandex app does.
@Composable
private fun FullPlayer(room: Room, onClose: () -> Unit, onQueue: () -> Unit) {
    val snapshot = room.snapshot ?: return
    val track = snapshot.track
    val cover = rememberCover(track, 800)
    val tint by animateColorAsState(cover?.tint ?: Color(0xFF3A3A44), tween(700), label = "full")

    var dragging by remember { mutableStateOf(false) }
    var dragged by remember { mutableFloatStateOf(0f) }
    val duration = snapshot.durationMs
    val position = snapshot.positionMs
    val progress = when {
        dragging -> dragged
        duration > 0 -> (position.toFloat() / duration.toFloat()).coerceIn(0f, 1f)
        else -> 0f
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(Brush.verticalGradient(listOf(tint, tint.copy(alpha = 0.6f).compositeOn(Bg), Bg)))
            // Swallows taps, so nothing behind the player reacts to them.
            .clickable(enabled = true, onClick = {}, indication = null, interactionSource = null)
            .systemBarsPadding()
            .padding(horizontal = 24.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Row(verticalAlignment = Alignment.CenterVertically, modifier = Modifier.padding(vertical = 8.dp)) {
            IconButton(onClick = onClose) {
                Icon(AppIcons.ChevronDown, "свернуть", tint = Color.White, modifier = Modifier.size(30.dp))
            }
            Column(Modifier.weight(1f), horizontalAlignment = Alignment.CenterHorizontally) {
                Text("Сейчас играет", color = Color.White.copy(alpha = 0.7f), fontSize = 13.sp)
                Text(
                    track?.album ?: (if (snapshot.queue.isNotEmpty()) "${snapshot.index + 1} из ${snapshot.queue.size}" else ""),
                    color = Color.White,
                    fontSize = 14.sp,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            IconButton(onClick = onQueue) {
                Icon(AppIcons.Queue, "очередь", tint = Color.White)
            }
        }

        Spacer(Modifier.weight(1f))

        CoverBox(
            track,
            corner = 16.dp,
            pixels = 800,
            modifier = Modifier
                .fillMaxWidth()
                .widthIn(max = 420.dp)
                .aspectRatio(1f)
                .shadow(24.dp, RoundedCornerShape(16.dp)),
        )

        Spacer(Modifier.weight(1f))

        Row(verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text(
                    track?.title ?: "—",
                    color = Color.White,
                    fontSize = 24.sp,
                    fontWeight = FontWeight.Bold,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    track?.artist ?: "",
                    color = Color.White.copy(alpha = 0.7f),
                    fontSize = 17.sp,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            if (track != null) {
                HeartButton(snapshot.trackLiked, size = 28.dp, idle = Color.White) {
                    room.scope.launch { Commands.like(track, !snapshot.trackLiked)?.let { SyncHolder.say(it) } }
                }
            }
        }

        Spacer(Modifier.height(12.dp))

        Slider(
            value = progress,
            enabled = room.canDrive && duration > 0,
            onValueChange = { dragging = true; dragged = it },
            onValueChangeFinished = {
                dragging = false
                room.scope.launch {
                    Commands.send("seek") { put("to_ms", (dragged * duration).toLong()) }
                }
            },
            colors = SliderDefaults.colors(
                thumbColor = Color.White,
                activeTrackColor = Color.White,
                inactiveTrackColor = Color.White.copy(alpha = 0.25f),
            ),
            modifier = Modifier.fillMaxWidth(),
        )
        Row(Modifier.fillMaxWidth()) {
            Text(
                if (snapshot.loading) "загрузка…" else formatMs(if (dragging) (dragged * duration).toLong() else position),
                color = Color.White.copy(alpha = 0.7f),
                fontSize = 12.sp,
            )
            Spacer(Modifier.weight(1f))
            Text(formatMs(duration), color = Color.White.copy(alpha = 0.7f), fontSize = 12.sp)
        }

        Spacer(Modifier.height(12.dp))

        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.SpaceEvenly,
            modifier = Modifier.fillMaxWidth(),
        ) {
            IconButton(onClick = { room.send("prev") }, enabled = room.canDrive, modifier = Modifier.size(56.dp)) {
                Icon(AppIcons.Prev, "предыдущий", tint = Color.White, modifier = Modifier.size(36.dp))
            }
            Box(
                Modifier
                    .size(76.dp)
                    .clip(CircleShape)
                    .background(Color.White)
                    .clickable(enabled = room.canDrive) { room.send("toggle") },
                contentAlignment = Alignment.Center,
            ) {
                Icon(
                    if (snapshot.playing) AppIcons.Pause else AppIcons.Play,
                    "пауза / продолжить",
                    tint = Color.Black,
                    modifier = Modifier.size(36.dp),
                )
            }
            IconButton(onClick = { room.send("next") }, enabled = room.canDrive, modifier = Modifier.size(56.dp)) {
                Icon(AppIcons.Next, "следующий", tint = Color.White, modifier = Modifier.size(36.dp))
            }
        }

        Spacer(Modifier.height(20.dp))

        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier
                .fillMaxWidth()
                .padding(bottom = 16.dp),
        ) {
            IconButton(
                onClick = { room.scope.launch { Commands.send("download") { put("queue", false) } } },
                enabled = room.canDrive && track != null,
            ) {
                Icon(
                    if (track != null && track.trackId in snapshot.cached) AppIcons.Disk else AppIcons.Download,
                    "скачать трек",
                    tint = if (track != null && track.trackId in snapshot.cached) Good else Color.White.copy(alpha = 0.8f),
                )
            }
            Spacer(Modifier.weight(1f))
            snapshot.driftMs?.let { drift ->
                val magnitude = kotlin.math.abs(drift)
                Text(
                    "рассинхрон ${if (drift > 0) "+" else ""}$drift мс",
                    fontSize = 12.sp,
                    color = when {
                        magnitude < 100 -> Good
                        magnitude < 300 -> Warn
                        else -> Bad
                    },
                    modifier = Modifier
                        .clip(CircleShape)
                        .background(Color.Black.copy(alpha = 0.3f))
                        .padding(horizontal = 10.dp, vertical = 4.dp),
                )
            }
            Spacer(Modifier.weight(1f))
            Text(
                "${snapshot.peers}",
                color = Color.White.copy(alpha = 0.8f),
                fontSize = 13.sp,
            )
            Icon(
                AppIcons.Devices,
                "участников",
                tint = Color.White.copy(alpha = 0.8f),
                modifier = Modifier
                    .padding(start = 6.dp, end = 12.dp)
                    .size(20.dp),
            )
        }
    }
}

/// The gradient's middle stop, flattened onto the background so the fade does
/// not show the page through it.
private fun Color.compositeOn(background: Color): Color {
    val a = alpha
    return Color(
        red = red * a + background.red * (1 - a),
        green = green * a + background.green * (1 - a),
        blue = blue * a + background.blue * (1 - a),
    )
}

// ---------------------------------------------------------------- rows

/// Where a track would come from, when that costs no internet: this device's own
/// disk, or somebody else's in the room.
private enum class Mark { NONE, DISK, LAN }

private fun markFor(snapshot: Snapshot?, trackId: String): Mark = when {
    snapshot == null -> Mark.NONE
    trackId in snapshot.cached -> Mark.DISK
    trackId in snapshot.onLan -> Mark.LAN
    else -> Mark.NONE
}

@Composable
private fun TrackRow(
    track: TrackInfo,
    room: Room,
    current: Boolean = false,
    /// Replaces the duration on the right, where size matters more.
    trailing: String? = null,
    extra: (@Composable () -> Unit)? = null,
    onClick: () -> Unit,
) {
    val mark = room.mark(track.trackId)
    // The heart is its own button beside the row rather than inside it: a row is
    // already a button, and one inside another is neither valid nor tappable.
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(12.dp))
            .background(if (current) Accent.copy(alpha = 0.08f) else Color.Transparent)
            .clickable(enabled = room.canDrive, onClick = onClick)
            .padding(start = 8.dp, top = 6.dp, bottom = 6.dp),
    ) {
        CoverBox(track, size = 48.dp, corner = 8.dp) {
            if (current) {
                Box(
                    Modifier
                        .fillMaxSize()
                        .background(Color.Black.copy(alpha = 0.5f)),
                    contentAlignment = Alignment.Center,
                ) {
                    Equaliser(playing = room.snapshot?.playing == true)
                }
            }
        }
        Spacer(Modifier.width(12.dp))
        Column(Modifier.weight(1f)) {
            Text(
                track.title,
                color = if (current) Accent else Color.White,
                fontWeight = FontWeight.SemiBold,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Row(verticalAlignment = Alignment.CenterVertically) {
                when (mark) {
                    Mark.DISK -> MarkIcon(AppIcons.Disk, Good, "есть на этом устройстве")
                    Mark.LAN -> MarkIcon(AppIcons.Lan, Accent, "есть у кого-то в комнате")
                    Mark.NONE -> {}
                }
                Text(
                    track.artist,
                    color = Dim,
                    fontSize = 13.sp,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
        Text(
            trailing ?: formatMs(track.durationMs),
            color = Dim,
            fontSize = 12.sp,
            modifier = Modifier.padding(start = 8.dp),
        )
        HeartButton(track.trackId in room.likedIds) { room.like(track) }
        extra?.invoke()
    }
}

@Composable
private fun MarkIcon(icon: ImageVector, tint: Color, description: String) {
    Icon(
        icon,
        description,
        tint = tint,
        modifier = Modifier
            .padding(end = 4.dp)
            .size(14.dp),
    )
}

/// The playing row's three bouncing bars. They hold still while paused.
@Composable
private fun Equaliser(playing: Boolean) {
    val transition = rememberInfiniteTransition(label = "eq")
    val bars = (0..2).map { index ->
        transition.animateFloat(
            initialValue = 0.2f,
            targetValue = 1f,
            animationSpec = infiniteRepeatable(
                tween(420 + index * 130, easing = FastOutSlowInEasing),
                RepeatMode.Reverse,
            ),
            label = "bar$index",
        )
    }
    Row(
        horizontalArrangement = Arrangement.spacedBy(2.dp),
        verticalAlignment = Alignment.Bottom,
        modifier = Modifier.height(16.dp),
    ) {
        bars.forEachIndexed { index, bar ->
            val height = if (playing) bar.value else listOf(0.5f, 0.8f, 0.35f)[index]
            Box(
                Modifier
                    .width(3.dp)
                    .fillMaxHeight(height)
                    .background(Accent, RoundedCornerShape(1.dp)),
            )
        }
    }
}

/// «Мне нравится» for one track. Per account, not per room: pressing it changes
/// what Yandex holds for this token and nothing for the other listeners.
@Composable
private fun HeartButton(liked: Boolean, size: Dp = 22.dp, idle: Color = Dim, onClick: () -> Unit) {
    IconButton(onClick = onClick) {
        Icon(
            if (liked) AppIcons.Heart else AppIcons.HeartOutline,
            if (liked) "убрать из «Мне нравится»" else "в «Мне нравится»",
            tint = if (liked) Bad else idle,
            modifier = Modifier.size(size),
        )
    }
}

// ---------------------------------------------------------------- covers

/// Loads a track's cover, keyed by its address so a row reused for another track
/// does not keep the old picture.
@Composable
private fun rememberCover(track: TrackInfo?, pixels: Int): Cover? {
    val url = track?.coverUrl(pixels)
    val cover by produceState(Covers.peek(url), url) {
        value = url?.let { Covers.load(it) }
    }
    return cover
}

@Composable
private fun CoverBox(
    track: TrackInfo?,
    modifier: Modifier = Modifier,
    size: Dp? = null,
    corner: Dp = 8.dp,
    pixels: Int = 200,
    overlay: @Composable BoxScope.() -> Unit = {},
) {
    CoverArt(track?.coverUrl(pixels), modifier, size, corner, overlay)
}

/// A cover box drawn from an address rather than from a track: the search
/// dropdown's best match has one of its own.
@Composable
private fun CoverArt(
    url: String?,
    modifier: Modifier = Modifier,
    size: Dp? = null,
    corner: Dp = 8.dp,
    overlay: @Composable BoxScope.() -> Unit = {},
) {
    val cover by produceState(Covers.peek(url), url) { value = url?.let { Covers.load(it) } }
    val image = cover?.image
    Box(
        modifier = modifier
            .then(if (size != null) Modifier.size(size) else Modifier)
            .clip(RoundedCornerShape(corner))
            .background(Surface3),
        contentAlignment = Alignment.Center,
    ) {
        if (image != null) {
            Image(
                bitmap = image,
                contentDescription = null,
                contentScale = ContentScale.Crop,
                modifier = Modifier.fillMaxSize(),
            )
        } else {
            Icon(
                AppIcons.Note,
                null,
                tint = Color.White.copy(alpha = 0.5f),
                modifier = Modifier.fillMaxSize(0.4f),
            )
        }
        overlay()
    }
}

// ---------------------------------------------------------------- small parts

@Composable
private fun Card(content: @Composable () -> Unit) {
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(18.dp))
            .background(Surface1)
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) { content() }
}

@Composable
private fun SectionTitle(text: String, note: String? = null) {
    Row(verticalAlignment = Alignment.Bottom) {
        Text(text, fontSize = 20.sp, fontWeight = FontWeight.Bold)
        note?.let {
            Spacer(Modifier.width(8.dp))
            Text(it, color = Dim, fontSize = 13.sp, modifier = Modifier.padding(bottom = 2.dp))
        }
    }
}

@Composable
private fun Empty(text: String) {
    Text(
        text,
        color = Dim,
        textAlign = TextAlign.Center,
        style = MaterialTheme.typography.bodyMedium,
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 24.dp, vertical = 28.dp),
    )
}

@Composable
private fun Hint(text: String) {
    Text(text, style = MaterialTheme.typography.bodySmall, color = Dim)
}

@Composable
private fun Chip(label: String, selected: Boolean, onClick: () -> Unit) {
    FilterChip(
        selected = selected,
        onClick = onClick,
        label = { Text(label, fontWeight = FontWeight.SemiBold, maxLines = 1, softWrap = false) },
        shape = CircleShape,
        border = null,
        colors = FilterChipDefaults.filterChipColors(
            containerColor = Surface2,
            labelColor = Dim,
            selectedContainerColor = Color.White,
            selectedLabelColor = Color.Black,
        ),
    )
}

@Composable
private fun Field(value: String, onValue: (String) -> Unit, label: String, secret: Boolean = false) {
    OutlinedTextField(
        value = value,
        onValueChange = onValue,
        label = { Text(label) },
        singleLine = true,
        shape = RoundedCornerShape(12.dp),
        colors = fieldColors(),
        visualTransformation = if (secret) PasswordVisualTransformation() else androidx.compose.ui.text.input.VisualTransformation.None,
        modifier = Modifier.fillMaxWidth(),
    )
}

@Composable
private fun fieldColors() = OutlinedTextFieldDefaults.colors(
    focusedContainerColor = Surface2,
    unfocusedContainerColor = Surface2,
    focusedBorderColor = Accent,
    unfocusedBorderColor = Color.Transparent,
    focusedLabelColor = Accent,
    cursorColor = Accent,
)

@Composable
private fun PrimaryButton(text: String, icon: ImageVector? = null, enabled: Boolean = true, onClick: () -> Unit) {
    Button(
        onClick = onClick,
        enabled = enabled,
        colors = ButtonDefaults.buttonColors(containerColor = Accent, contentColor = AccentInk),
        contentPadding = PaddingValues(horizontal = 18.dp, vertical = 10.dp),
        modifier = Modifier.heightIn(min = 44.dp),
    ) {
        icon?.let {
            Icon(it, null, modifier = Modifier.size(18.dp))
            Spacer(Modifier.width(6.dp))
        }
        Text(text, fontWeight = FontWeight.Bold)
    }
}

@Composable
private fun GhostButton(text: String, icon: ImageVector? = null, enabled: Boolean = true, onClick: () -> Unit) {
    Button(
        onClick = onClick,
        enabled = enabled,
        colors = ButtonDefaults.buttonColors(
            containerColor = Color.White.copy(alpha = 0.1f),
            contentColor = Color.White,
            disabledContainerColor = Color.White.copy(alpha = 0.05f),
            disabledContentColor = Color.White.copy(alpha = 0.35f),
        ),
        contentPadding = PaddingValues(horizontal = 16.dp, vertical = 10.dp),
        modifier = Modifier.heightIn(min = 44.dp),
    ) {
        icon?.let {
            Icon(it, null, modifier = Modifier.size(18.dp))
            Spacer(Modifier.width(6.dp))
        }
        Text(text, fontWeight = FontWeight.SemiBold)
    }
}

@Composable
private fun RoundPlay(enabled: Boolean, onClick: () -> Unit) {
    Box(
        Modifier
            .size(56.dp)
            .clip(CircleShape)
            .background(if (enabled) Accent else Accent.copy(alpha = 0.35f))
            .clickable(enabled = enabled, onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Icon(AppIcons.Play, "играть всё", tint = AccentInk, modifier = Modifier.size(28.dp))
    }
}

/** «1 трек», «3 трека», «25 треков». */
private fun tracksWord(count: Int): String {
    val tens = count % 100
    val ones = count % 10
    return when {
        tens in 11..14 -> "$count треков"
        ones == 1 -> "$count трек"
        ones in 2..4 -> "$count трека"
        else -> "$count треков"
    }
}
