package dev.mshiv.ymsync

import android.Manifest
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Slider
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import kotlinx.coroutines.launch

private val Accent = Color(0xFFFFC72C)
private val Good = Color(0xFF4EC97A)
private val Warn = Color(0xFFFFC72C)
private val Bad = Color(0xFFFF6B6B)

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
                    onPrimary = Color(0xFF1A1C22),
                    background = Color(0xFF101218),
                    surface = Color(0xFF171A22),
                    surfaceVariant = Color(0xFF1D212B),
                ),
            ) {
                Surface(modifier = Modifier.fillMaxSize()) { App(settings) }
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

    var showSettings by remember { mutableStateOf(settings.missing.isNotEmpty()) }
    var kind by remember { mutableStateOf("search") }
    var source by remember { mutableStateOf("") }
    var query by remember { mutableStateOf("") }
    var results by remember { mutableStateOf(emptyList<TrackInfo>()) }
    var library by remember { mutableStateOf<Library?>(null) }

    // A finished download changes the disk, and the cache revision is how the
    // core says so — cheaper than re-reading the list on every snapshot.
    LaunchedEffect(running, snapshot?.cacheRevision) {
        library = if (running) Commands.library().getOrNull() else null
    }

    // Media3 needs notification permission for its playback notification.
    val askNotifications = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { }
    LaunchedEffect(Unit) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            askNotifications.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
    }

    // Protocol 3 has no roles: anyone connected to the room may drive it.
    val canDrive = running

    Scaffold(
        bottomBar = { PlayerBar(snapshot, canDrive) },
    ) { insets ->
        LazyColumn(
            modifier = Modifier
                .fillMaxSize()
                .padding(insets)
                .padding(horizontal = 12.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            item {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text("ym-sync", color = Accent, style = MaterialTheme.typography.titleMedium)
                    val station = snapshot?.station
                    if (station != null) {
                        Spacer(Modifier.width(12.dp))
                        // The wave is room-wide, but only its feeder can refill
                        // it — so only the feeder is offered the off switch.
                        FilterChip(
                            selected = true,
                            onClick = {
                                scope.launch { Commands.send("stop_wave") }
                            },
                            enabled = snapshot?.feeding == true,
                            label = {
                                Text(if (snapshot?.feeding == true) "волна · выключить" else "волна")
                            },
                        )
                    }
                }
            }

            item {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    if (running) {
                        Button(onClick = { SyncService.stop(context) }) { Text("Отключиться") }
                    }
                    TextButton(onClick = { showSettings = !showSettings }) { Text("Настройки") }
                }
            }

            item { StatusLine(snapshot, message) }

            // Two ways into a room, and the same two fields for both: a room is a
            // name and a password. Which button you press decides who holds it.
            if (!running) {
                item {
                    RoomCard(settings) { host ->
                        val absent = settings.missing
                        if (absent.isEmpty()) {
                            SyncService.start(context, settings.configJson(host), host)
                        } else {
                            SyncHolder.say("не заполнено: ${absent.joinToString(", ")}")
                            if (settings.yandexToken.isBlank()) showSettings = true
                        }
                    }
                }
            }

            if (showSettings) {
                item { SettingsCard(settings) }
            }

            if (running) {
                item {
                    SourceRow(
                        kind = kind,
                        onKind = { kind = it },
                        value = source,
                        onValue = { source = it },
                        enabled = canDrive,
                        onLoad = { replace ->
                            scope.launch {
                                Commands.queueFrom(kind, source, replace)
                                    .onSuccess { count ->
                                        SyncHolder.say(
                                            when {
                                                kind == "wave" && replace -> "волна включена"
                                                kind == "wave" -> "волна продолжит очередь"
                                                replace -> "играю: $count трек(ов)"
                                                else -> "добавлено в очередь: $count"
                                            },
                                        )
                                    }
                                    .onFailure { SyncHolder.say(it.message) }
                            }
                        },
                    )
                }

                item {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        OutlinedTextField(
                            value = query,
                            onValueChange = { query = it },
                            label = { Text("найти треки") },
                            singleLine = true,
                            modifier = Modifier.weight(1f),
                        )
                        Spacer(Modifier.width(8.dp))
                        OutlinedButton(
                            enabled = canDrive,
                            onClick = {
                                scope.launch {
                                    Commands.search(query)
                                        .onSuccess {
                                            results = it
                                            if (it.isEmpty()) SyncHolder.say("ничего не найдено")
                                        }
                                        .onFailure { error -> SyncHolder.say(error.message) }
                                }
                            },
                        ) { Text("Найти") }
                    }
                }

                if (results.isNotEmpty()) {
                    item { SectionTitle("Результаты — клик добавит трек в конец очереди") }
                    itemsIndexed(results, key = { index, track -> "r${track.trackId}$index" }) { index, track ->
                        TrackRow(
                            position = index + 1,
                            track = track,
                            current = false,
                            enabled = canDrive,
                            mark = markFor(snapshot, track.trackId),
                        ) {
                            // The row already holds the whole track, so nothing is
                            // asked of Yandex here — see `Commands.queueTracks`.
                            scope.launch {
                                Commands.queueTracks(listOf(track))
                                    .onFailure { error -> SyncHolder.say(error.message) }
                            }
                        }
                    }
                }

                val queue = snapshot?.queue ?: emptyList()
                if (queue.isNotEmpty()) {
                    item { SectionTitle("Очередь · ${queue.size}") }
                    item {
                        // Keeping music here is per-device, so it sits with the
                        // queue rather than with the room's controls.
                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            OutlinedButton(
                                enabled = canDrive && snapshot?.track != null,
                                onClick = {
                                    scope.launch { Commands.send("download") { put("queue", false) } }
                                },
                            ) { Text("↓ трек") }
                            OutlinedButton(
                                enabled = canDrive,
                                onClick = {
                                    scope.launch { Commands.send("download") { put("queue", true) } }
                                },
                            ) { Text("↓ очередь") }
                            if ((snapshot?.downloadQueue ?: 0) > 0) {
                                TextButton(
                                    onClick = {
                                        scope.launch { Commands.send("cancel_downloads") }
                                    },
                                ) { Text("отменить") }
                            }
                        }
                    }
                    items(queue.size, key = { "q${queue[it].trackId}$it" }) { index ->
                        TrackRow(
                            position = index + 1,
                            track = queue[index],
                            current = index == (snapshot?.index ?: -1),
                            enabled = canDrive,
                            mark = markFor(snapshot, queue[index].trackId),
                        ) {
                            scope.launch { Commands.send("index") { put("index", index) } }
                        }
                    }
                }

                val downloaded = library?.tracks ?: emptyList()
                if (downloaded.isNotEmpty()) {
                    item { SectionTitle("На устройстве · ${downloaded.size} · ${librarySize(library)}") }
                    item {
                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            // Playing the whole library is how you listen with no
                            // internet: every one of these comes off the disk.
                            OutlinedButton(
                                enabled = canDrive,
                                onClick = {
                                    scope.launch {
                                        Commands.queueTracks(
                                            downloaded.map { it.track },
                                            replace = true,
                                        ).onFailure { SyncHolder.say(it.message) }
                                    }
                                },
                            ) { Text("Играть всё") }
                            OutlinedButton(
                                enabled = canDrive,
                                onClick = {
                                    scope.launch {
                                        Commands.queueTracks(downloaded.map { it.track })
                                            .onFailure { SyncHolder.say(it.message) }
                                    }
                                },
                            ) { Text("В очередь") }
                        }
                    }
                    items(downloaded.size, key = { "c${downloaded[it].track.trackId}" }) { index ->
                        val entry = downloaded[index]
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            TrackRow(
                                position = index + 1,
                                track = entry.track,
                                current = false,
                                enabled = canDrive,
                                modifier = Modifier.weight(1f),
                                trailing = formatSize(entry.bytes),
                            ) {
                                // The metadata was stored beside the audio, so this
                                // needs neither Yandex nor a network at all.
                                scope.launch {
                                    Commands.queueTracks(listOf(entry.track))
                                        .onFailure { error -> SyncHolder.say(error.message) }
                                }
                            }
                            TextButton(
                                onClick = {
                                    scope.launch {
                                        Commands.send("forget") {
                                            put("track_id", entry.track.trackId)
                                        }
                                        library = Commands.library().getOrNull()
                                    }
                                },
                            ) { Text("×") }
                        }
                    }
                }
            }
        }
    }
}

/// Where a track would come from, when that costs no internet: this device's own
/// disk, or somebody else's in the room.
private fun markFor(snapshot: Snapshot?, trackId: String): String = when {
    snapshot == null -> ""
    trackId in snapshot.cached -> "\u2913"
    trackId in snapshot.onLan -> "\u21C4"
    else -> ""
}

private fun librarySize(library: Library?): String {
    val bytes = formatSize(library?.bytes ?: 0)
    val limit = library?.limitBytes ?: 0
    return if (limit > 0) "$bytes из ${formatSize(limit)}" else bytes
}

@Composable
private fun StatusLine(snapshot: Snapshot?, message: String?) {
    Column {
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
            color = if (snapshot?.connected == true) Good else MaterialTheme.colorScheme.onSurfaceVariant,
            style = MaterialTheme.typography.bodySmall,
        )
        // The address the others have to type in. Only known once the socket is
        // bound, which is why it comes from the snapshot and not the settings.
        snapshot?.hosting?.let {
            Text(
                "комната здесь · $it",
                color = Accent,
                style = MaterialTheme.typography.bodySmall,
            )
        }
        // What the engine has to say — a skipped track, a station that stopped
        // answering — would otherwise never reach the screen.
        snapshot?.notice?.let {
            Text(
                it,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                style = MaterialTheme.typography.bodySmall,
            )
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

    Card {
        Column(
            modifier = Modifier.padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            OutlinedTextField(
                value = room,
                onValueChange = { room = it; settings.room = it },
                label = { Text("название комнаты") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = password,
                onValueChange = { password = it; settings.password = it },
                label = { Text("пароль комнаты") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            Text(
                "Название и пароль должны совпадать у всех участников.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )

            HorizontalDivider()

            OutlinedTextField(
                value = relay,
                onValueChange = { relay = it; settings.relay = it },
                label = { Text("адрес комнаты") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )

            // Fills the address in from the network, so nobody has to read an IP
            // off another screen and type it in.
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedButton(
                    enabled = !searching,
                    onClick = {
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
                    },
                ) { Text(if (searching) "Ищу…" else "Найти в сети") }

                Button(onClick = { onEnter(false) }) { Text("Подключиться") }
            }

            found.forEach { candidate ->
                TextButton(
                    // A relay of another version would refuse us, so it is shown
                    // and disabled rather than hidden: that explains the situation.
                    enabled = candidate.compatible,
                    onClick = {
                        relay = candidate.relay
                        settings.relay = candidate.relay
                        // An empty name means that relay holds no rooms yet, so
                        // the name in the field is what will be created.
                        if (candidate.room.isNotEmpty()) {
                            room = candidate.room
                            settings.room = candidate.room
                        }
                        SyncHolder.say("вписал ${candidate.relay}")
                    },
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Text(
                        buildString {
                            append(candidate.room.ifEmpty { "комнат пока нет" })
                            append(" · ${candidate.relay}")
                            if (candidate.listeners > 0) append(" · ${candidate.listeners}")
                            if (!candidate.compatible) append(" · другая версия")
                        },
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }

            HorizontalDivider()

            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    "Комнату будет держать этот телефон: адрес для остальных " +
                        "появится в строке состояния. Так же выглядит и " +
                        "прослушивание скачанного без интернета.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.weight(1f),
                )
                Spacer(Modifier.width(8.dp))
                Button(onClick = { onEnter(true) }) { Text("Хостить") }
            }
        }
    }
}

@Composable
private fun SettingsCard(settings: Settings) {
    var yandexToken by remember { mutableStateOf(settings.yandexToken) }
    var autoCache by remember { mutableStateOf(settings.autoCache) }
    var cacheLimit by remember { mutableStateOf(settings.cacheLimitGb.toString()) }

    Card {
        Column(
            modifier = Modifier.padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            OutlinedTextField(
                value = yandexToken,
                onValueChange = { yandexToken = it; settings.yandexToken = it },
                label = { Text("токен Яндекса (этого устройства)") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            Text(
                "В эмуляторе релей на компьютере доступен как 10.0.2.2",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )

            HorizontalDivider()

            SettingSwitch(
                label = "оставлять всё, что играет",
                hint = "иначе на телефоне остаётся только скачанное кнопками «↓».",
                checked = autoCache,
                onChange = { autoCache = it; settings.autoCache = it },
            )
            OutlinedTextField(
                value = cacheLimit,
                onValueChange = { typed ->
                    cacheLimit = typed.filter { it.isDigit() || it == '.' }
                    // 0 means no limit, and an unreadable value must not be
                    // silently taken for one.
                    settings.cacheLimitGb = cacheLimit.toFloatOrNull() ?: settings.cacheLimitGb
                },
                label = { Text("лимит на скачанное, ГБ (0 — без лимита)") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            Text(
                "Эти два применятся при следующем подключении: ядро читает настройки " +
                    "при старте. Файлы лежат в ${settings.cacheDir} и удаляются вместе " +
                    "с приложением.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
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
        Text(
            hint,
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun SourceRow(
    kind: String,
    onKind: (String) -> Unit,
    value: String,
    onValue: (String) -> Unit,
    enabled: Boolean,
    onLoad: (Boolean) -> Unit,
) {
    val needsValue = kind !in SELF_CONTAINED
    val ready = enabled && (!needsValue || value.isNotBlank())

    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        // Six chips do not fit on one phone-width line.
        FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            listOf(
                "search" to "поиск",
                "album" to "альбом",
                "playlist" to "плейлист",
                "track" to "трек",
                "wave" to "моя волна",
                "likes" to "мне нравится",
                "offline" to "скачанное",
            ).forEach { (id, label) ->
                FilterChip(
                    selected = kind == id,
                    onClick = { onKind(id) },
                    label = { Text(label) },
                )
            }
        }
        if (needsValue) {
            OutlinedTextField(
                value = value,
                onValueChange = onValue,
                label = { Text("что играть") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
        }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedButton(onClick = { onLoad(false) }, enabled = ready) { Text("В очередь") }
            Button(onClick = { onLoad(true) }, enabled = ready) { Text("Играть") }
        }
    }
}

@Composable
private fun SectionTitle(text: String) {
    Column {
        HorizontalDivider()
        Text(
            text,
            style = MaterialTheme.typography.labelMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(vertical = 6.dp),
        )
    }
}

@Composable
private fun TrackRow(
    position: Int,
    track: TrackInfo,
    current: Boolean,
    enabled: Boolean,
    modifier: Modifier = Modifier,
    /// Small hint of where the track would come from; see [markFor].
    mark: String = "",
    /// Replaces the duration on the right, where size matters more.
    trailing: String? = null,
    onClick: () -> Unit,
) {
    TextButton(onClick = onClick, enabled = enabled, modifier = modifier.fillMaxWidth()) {
        Text(
            "$position",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.width(28.dp),
        )
        Text(
            "${track.artist} — ${track.title}",
            color = if (current) Accent else MaterialTheme.colorScheme.onSurface,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.weight(1f),
        )
        if (mark.isNotEmpty()) {
            Text(
                mark,
                style = MaterialTheme.typography.bodySmall,
                color = Good,
                modifier = Modifier.padding(end = 6.dp),
            )
        }
        Text(
            trailing ?: formatMs(track.durationMs),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun PlayerBar(snapshot: Snapshot?, canDrive: Boolean) {
    val scope = rememberCoroutineScope()
    var dragging by remember { mutableStateOf(false) }
    var dragged by remember { mutableFloatStateOf(0f) }

    val duration = snapshot?.durationMs ?: 0L
    val position = snapshot?.positionMs ?: 0L
    val progress = when {
        dragging -> dragged
        duration > 0 -> (position.toFloat() / duration.toFloat()).coerceIn(0f, 1f)
        else -> 0f
    }

    Surface(color = MaterialTheme.colorScheme.surface) {
        Column(modifier = Modifier.padding(horizontal = 12.dp, vertical = 8.dp)) {
            Text(
                snapshot?.track?.let { "${it.artist} — ${it.title}" } ?: "—",
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                style = MaterialTheme.typography.bodyMedium,
            )

            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    if (snapshot?.loading == true) "загрузка…" else formatMs(position),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Slider(
                    value = progress,
                    enabled = canDrive && duration > 0,
                    onValueChange = { dragging = true; dragged = it },
                    onValueChangeFinished = {
                        dragging = false
                        scope.launch {
                            Commands.send("seek") { put("to_ms", (dragged * duration).toLong()) }
                        }
                    },
                    modifier = Modifier
                        .weight(1f)
                        .padding(horizontal = 8.dp),
                )
                Text(
                    formatMs(duration),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }

            Row(verticalAlignment = Alignment.CenterVertically) {
                OutlinedButton(
                    enabled = canDrive,
                    onClick = { scope.launch { Commands.send("prev") } },
                ) { Text("◀◀") }
                Spacer(Modifier.width(6.dp))
                Button(
                    enabled = canDrive,
                    onClick = { scope.launch { Commands.send("toggle") } },
                ) { Text(if (snapshot?.playing == true) "❚❚" else "▶") }
                Spacer(Modifier.width(6.dp))
                OutlinedButton(
                    enabled = canDrive,
                    onClick = { scope.launch { Commands.send("next") } },
                ) { Text("▶▶") }

                Spacer(Modifier.weight(1f))

                snapshot?.driftMs?.let { drift ->
                    val magnitude = kotlin.math.abs(drift)
                    Text(
                        "рассинхрон ${if (drift > 0) "+" else ""}$drift мс",
                        style = MaterialTheme.typography.bodySmall,
                        color = when {
                            magnitude < 100 -> Good
                            magnitude < 300 -> Warn
                            else -> Bad
                        },
                    )
                }
            }
        }
    }
}
