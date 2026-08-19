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
private val SELF_CONTAINED = setOf("wave", "likes")

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

    var role by remember { mutableStateOf("master") }
    var showSettings by remember { mutableStateOf(settings.missing.isNotEmpty()) }
    var kind by remember { mutableStateOf("search") }
    var source by remember { mutableStateOf("") }
    var query by remember { mutableStateOf("") }
    var results by remember { mutableStateOf(emptyList<TrackInfo>()) }

    // Media3 needs notification permission for its playback notification.
    val askNotifications = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { }
    LaunchedEffect(Unit) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            askNotifications.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
    }

    val canDrive = running && (snapshot?.isMaster ?: (role == "master"))

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
                    Spacer(Modifier.width(12.dp))
                    FilterChip(
                        selected = role == "master",
                        onClick = { role = "master" },
                        enabled = !running,
                        label = { Text("ведущий") },
                    )
                    Spacer(Modifier.width(6.dp))
                    FilterChip(
                        selected = role == "slave",
                        onClick = { role = "slave" },
                        enabled = !running,
                        label = { Text("ведомый") },
                    )
                }
            }

            item {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Button(
                        onClick = {
                            if (running) {
                                SyncService.stop(context)
                            } else {
                                val absent = settings.missing
                                if (absent.isEmpty()) {
                                    SyncService.start(context, settings.configJson(), role)
                                } else {
                                    SyncHolder.say("не заполнено: ${absent.joinToString(", ")}")
                                    showSettings = true
                                }
                            }
                        },
                    ) { Text(if (running) "Отключиться" else "Подключиться") }

                    Spacer(Modifier.width(8.dp))
                    TextButton(onClick = { showSettings = !showSettings }) { Text("Настройки") }
                }
            }

            item { StatusLine(snapshot, message) }

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
                        ) {
                            scope.launch {
                                Commands.queueFrom("track", track.trackId, replace = false)
                                    .onFailure { error -> SyncHolder.say(error.message) }
                            }
                        }
                    }
                }

                val queue = snapshot?.queue ?: emptyList()
                if (queue.isNotEmpty()) {
                    item { SectionTitle("Очередь · ${queue.size}") }
                    items(queue.size, key = { "q${queue[it].trackId}$it" }) { index ->
                        TrackRow(
                            position = index + 1,
                            track = queue[index],
                            current = index == (snapshot?.index ?: -1),
                            enabled = canDrive,
                        ) {
                            scope.launch { Commands.send("index") { put("index", index) } }
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun StatusLine(snapshot: Snapshot?, message: String?) {
    Column {
        val text = when {
            snapshot == null -> "не подключено"
            else -> buildString {
                append(if (snapshot.isMaster) "ведущий" else "ведомый")
                append(" · участников ${snapshot.peers}")
                snapshot.rttMs?.let { append(" · rtt $it мс") }
            }
        }
        Text(
            text,
            color = if (snapshot?.connected == true) Good else MaterialTheme.colorScheme.onSurfaceVariant,
            style = MaterialTheme.typography.bodySmall,
        )
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

@Composable
private fun SettingsCard(settings: Settings) {
    var relay by remember { mutableStateOf(settings.relay) }
    var room by remember { mutableStateOf(settings.room) }
    var roomToken by remember { mutableStateOf(settings.roomToken) }
    var yandexToken by remember { mutableStateOf(settings.yandexToken) }
    var bias by remember { mutableStateOf(settings.positionBiasMs.toString()) }

    Card {
        Column(
            modifier = Modifier.padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            OutlinedTextField(
                value = relay,
                onValueChange = { relay = it; settings.relay = it },
                label = { Text("релей") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = room,
                onValueChange = { room = it; settings.room = it },
                label = { Text("комната") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = roomToken,
                onValueChange = { roomToken = it; settings.roomToken = it },
                label = { Text("токен комнаты") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = yandexToken,
                onValueChange = { yandexToken = it; settings.yandexToken = it },
                label = { Text("токен Яндекса (этого устройства)") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = bias,
                onValueChange = { typed ->
                    bias = typed.filter { it.isDigit() || it == '-' }
                    settings.positionBiasMs = bias.toIntOrNull() ?: 0
                },
                label = { Text("поправка позиции, мс") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            Text(
                "ExoPlayer сообщает позицию с постоянным отставанием. Посмотрите, " +
                    "на какой величине стоит рассинхрон, и впишите её с обратным " +
                    "знаком: «−400 мс» → 400.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Text(
                "В эмуляторе релей на компьютере доступен как 10.0.2.2",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
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
    onClick: () -> Unit,
) {
    TextButton(onClick = onClick, enabled = enabled, modifier = Modifier.fillMaxWidth()) {
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
        Text(
            formatMs(track.durationMs),
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
