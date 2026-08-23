package dev.mshiv.ymsync

import android.content.Context
import org.json.JSONArray
import org.json.JSONObject

data class TrackInfo(
    val trackId: String,
    val title: String,
    val artist: String,
    val durationMs: Long,
)

/** One downloaded track, as the offline screen lists it. */
data class CachedTrack(
    val track: TrackInfo,
    val bytes: Long,
    /** Downloaded on purpose rather than picked up while playing. */
    val pinned: Boolean,
)

/** Everything on this device's disk, plus how much room it takes. */
data class Library(
    val tracks: List<CachedTrack>,
    val bytes: Long,
    /** 0 means no limit. */
    val limitBytes: Long,
    val directory: String,
)

/** Mirrors `ymsync::engine::Snapshot`. */
data class Snapshot(
    val connected: Boolean,
    val peers: Int,
    val queue: List<TrackInfo>,
    val index: Int,
    val track: TrackInfo?,
    val positionMs: Long,
    val durationMs: Long,
    val playing: Boolean,
    val loading: Boolean,
    /** The station feeding the room, if any. */
    val station: String?,
    /** Whether this device is the one feeding that station. */
    val feeding: Boolean,
    val driftMs: Long?,
    val rttMs: Long?,
    val notice: String?,
    /** Ids on this device's disk. */
    val cached: Set<String>,
    /** Bumped whenever [cached] changes. */
    val cacheRevision: Long,
    val cacheBytes: Long,
    /** Ids the room can supply over the local network, whoever holds them. */
    val onLan: Set<String>,
    /** How many peers are offering their downloads. */
    val sharingPeers: Int,
    /** The track being downloaded for offline use, if any. */
    val downloading: String?,
    /** How many more are waiting behind it. */
    val downloadQueue: Int,
    /** Set when this device runs the room's relay: the address to give others. */
    val hosting: String?,
)

/** Either the `ok` payload or the `error` text from a native call. */
sealed interface NativeResult {
    data class Ok(val value: JSONObject) : NativeResult
    data class Failed(val message: String) : NativeResult
}

fun parseReply(text: String): NativeResult =
    try {
        val root = JSONObject(text)
        when {
            root.has("error") -> NativeResult.Failed(root.getString("error"))
            // Calls that only acknowledge answer with a null payload.
            root.isNull("ok") -> NativeResult.Ok(JSONObject())
            else -> NativeResult.Ok(root.optJSONObject("ok") ?: JSONObject())
        }
    } catch (error: Exception) {
        NativeResult.Failed("не разобрать ответ ядра: ${error.message}")
    }

fun JSONObject.toTrackInfo() =
    TrackInfo(
        trackId = optString("track_id"),
        title = optString("title"),
        artist = optString("artist"),
        durationMs = optLong("duration_ms"),
    )

fun JSONArray.toTrackList(): List<TrackInfo> =
    (0 until length()).mapNotNull { optJSONObject(it)?.toTrackInfo() }

fun JSONArray.toIdSet(): Set<String> =
    (0 until length()).mapNotNull { optString(it).ifBlank { null } }.toSet()

fun JSONObject.toCachedTrack() =
    CachedTrack(track = toTrackInfo(), bytes = optLong("bytes"), pinned = optBoolean("pinned"))

fun JSONObject.toLibrary() =
    Library(
        tracks = optJSONArray("tracks")
            ?.let { array -> (0 until array.length()).mapNotNull { array.optJSONObject(it)?.toCachedTrack() } }
            ?: emptyList(),
        bytes = optLong("bytes"),
        limitBytes = optLong("limit_bytes"),
        directory = optString("directory"),
    )

/**
 * Reads a snapshot from the core.
 *
 * The core leaves `queue` out when it has not changed — «Мне нравится» is over a
 * thousand tracks and would otherwise travel five times a second — so an absent
 * queue means "the same as before", not "empty". `cached` is trimmed the same way,
 * because a well-used offline library runs into the thousands of ids.
 */
fun JSONObject.toSnapshot(previous: Snapshot? = null) =
    Snapshot(
        connected = optBoolean("connected"),
        peers = optInt("peers"),
        queue = optJSONArray("queue")?.toTrackList() ?: previous?.queue ?: emptyList(),
        index = optInt("index"),
        track = optJSONObject("track")?.toTrackInfo(),
        positionMs = optLong("position_ms"),
        durationMs = optLong("duration_ms"),
        playing = optBoolean("playing"),
        loading = optBoolean("loading"),
        station = if (isNull("station")) null else optString("station"),
        feeding = optBoolean("feeding"),
        // `optLong` would turn a missing drift into a misleading zero.
        driftMs = if (isNull("drift_ms")) null else optLong("drift_ms"),
        rttMs = if (isNull("rtt_ms")) null else optLong("rtt_ms"),
        notice = if (isNull("notice")) null else optString("notice"),
        cached = optJSONArray("cached")?.toIdSet() ?: previous?.cached ?: emptySet(),
        cacheRevision = optLong("cache_revision"),
        cacheBytes = optLong("cache_bytes"),
        onLan = optJSONArray("on_lan")?.toIdSet() ?: emptySet(),
        sharingPeers = optInt("sharing_peers"),
        downloading = if (isNull("downloading")) null else optString("downloading"),
        downloadQueue = optInt("download_queue"),
        hosting = if (isNull("hosting")) null else optString("hosting"),
    )

fun formatMs(ms: Long): String {
    val seconds = (ms.coerceAtLeast(0)) / 1000
    return "%d:%02d".format(seconds / 60, seconds % 60)
}

/** Binary units, to match what a file manager on the phone would report. */
fun formatSize(bytes: Long): String {
    val mib = bytes.coerceAtLeast(0) / (1024.0 * 1024.0)
    return if (mib >= 1024) "%.1f ГиБ".format(mib / 1024) else "%.1f МиБ".format(mib)
}

/**
 * Connection settings, kept in shared preferences. The field names in
 * [configJson] must match `ymsync::config::Config`, which rejects unknown keys.
 */
class Settings(context: Context) {
    private val prefs = context.getSharedPreferences("ymsync", Context.MODE_PRIVATE)

    /**
     * Where downloads live. Inside the app's own files directory, which is the one
     * place this process may write without asking for a permission — and it is
     * removed with the app, so nothing is left behind.
     */
    val cacheDir: String = context.filesDir.resolve("tracks").absolutePath

    var relay: String
        get() = prefs.getString(KEY_RELAY, "ws://10.0.2.2:8787")!!
        set(value) = prefs.edit().putString(KEY_RELAY, value).apply()

    var room: String
        get() = prefs.getString(KEY_ROOM, "home")!!
        set(value) = prefs.edit().putString(KEY_ROOM, value).apply()

    var roomToken: String
        get() = prefs.getString(KEY_ROOM_TOKEN, "")!!
        set(value) = prefs.edit().putString(KEY_ROOM_TOKEN, value).apply()

    var yandexToken: String
        get() = prefs.getString(KEY_YANDEX_TOKEN, "")!!
        set(value) = prefs.edit().putString(KEY_YANDEX_TOKEN, value).apply()

    var volume: Float
        get() = prefs.getFloat(KEY_VOLUME, 0.8f)
        set(value) = prefs.edit().putFloat(KEY_VOLUME, value).apply()

    /**
     * Cancels ExoPlayer's constant reporting lag. Calibrate from the drift the
     * app shows, with the sign flipped: a steady `-400 мс` means 400 here.
     */
    var positionBiasMs: Int
        get() = prefs.getInt(KEY_BIAS, 0)
        set(value) = prefs.edit().putInt(KEY_BIAS, value).apply()

    /**
     * Run the room's relay on this phone instead of dialling one.
     *
     * This is also what listening with no internet looks like: the queue and the
     * playhead need an authority, and with nothing to connect to the phone becomes
     * that authority for itself.
     */
    var hostRoom: Boolean
        get() = prefs.getBoolean(KEY_HOST, false)
        set(value) = prefs.edit().putBoolean(KEY_HOST, value).apply()

    /** Keep every track that plays, not only the ones downloaded on purpose. */
    var autoCache: Boolean
        get() = prefs.getBoolean(KEY_AUTO_CACHE, false)
        set(value) = prefs.edit().putBoolean(KEY_AUTO_CACHE, value).apply()

    /** Gigabytes. Smaller than the desktop default: this is a phone. */
    var cacheLimitGb: Float
        get() = prefs.getFloat(KEY_CACHE_LIMIT, 2f)
        set(value) = prefs.edit().putFloat(KEY_CACHE_LIMIT, value).apply()

    val missing: List<String>
        get() = buildList {
            if (yandexToken.isBlank()) add("токен Яндекса")
            if (roomToken.isBlank()) add("токен комнаты")
        }

    fun configJson(): String =
        JSONObject()
            .put("relay", relay)
            .put("room", room)
            .put("room_token", roomToken)
            .put("yandex_token", yandexToken)
            .put("volume", volume.toDouble())
            // Every other sync field keeps its default on the Rust side.
            .put("sync", JSONObject().put("position_bias_ms", positionBiasMs))
            .put(
                "cache",
                JSONObject()
                    .put("dir", cacheDir)
                    .put("limit_gb", cacheLimitGb.toDouble())
                    .put("auto", autoCache),
            )
            // `port` 0 lets the system pick: a phone has no firewall rule that
            // needs a fixed one.
            .put("share", JSONObject().put("enabled", true).put("port", 0))
            .put(
                "host",
                JSONObject()
                    .put("enabled", hostRoom)
                    .put("bind", "0.0.0.0")
                    .put("port", 8787),
            )
            .toString()

    private companion object {
        const val KEY_RELAY = "relay"
        const val KEY_ROOM = "room"
        const val KEY_ROOM_TOKEN = "room_token"
        const val KEY_YANDEX_TOKEN = "yandex_token"
        const val KEY_VOLUME = "volume"
        const val KEY_BIAS = "position_bias_ms"
        const val KEY_HOST = "host_room"
        const val KEY_AUTO_CACHE = "auto_cache"
        const val KEY_CACHE_LIMIT = "cache_limit_gb"
    }
}
