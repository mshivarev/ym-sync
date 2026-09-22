package dev.mshiv.ymsync

import android.content.Context
import org.json.JSONArray
import org.json.JSONObject

data class TrackInfo(
    val trackId: String,
    val title: String,
    val artist: String,
    val durationMs: Long,
    /** Kept so a track queued from this screen loses nothing on the way. */
    val albumId: String? = null,
    /** The album's name, when the core knows it. What the album view groups by. */
    val album: String? = null,
    /** Yandex's cover template, `avatars.yandex.net/…/%%`; see [coverUrl]. */
    val coverUri: String? = null,
) {
    /**
     * The cover at [size]×[size] pixels, or null when the core has none — a track
     * downloaded before covers were stored, or one that came from another peer.
     */
    fun coverUrl(size: Int): String? {
        val uri = coverUri?.takeIf { it.isNotBlank() } ?: return null
        val sized = uri.replace("%%", "${size}x$size")
        return if (sized.startsWith("http")) sized else "https://$sized"
    }

    /**
     * The shape `Native.queueTracks` takes: `ymsync_proto::TrackRef`.
     *
     * A row on screen already holds everything the room needs, so playing it costs
     * no lookup — see the note on `Session::queue_tracks`.
     */
    fun toJson(): JSONObject =
        JSONObject()
            .put("track_id", trackId)
            .put("title", title)
            .put("artist", artist)
            .put("duration_ms", durationMs)
            .apply {
                albumId?.let { put("album_id", it) }
                album?.let { put("album", it) }
                coverUri?.let { put("cover_uri", it) }
            }
}

/** A list of tracks as the core expects it. */
fun List<TrackInfo>.toJsonArray(): JSONArray =
    JSONArray().apply { this@toJsonArray.forEach { put(it.toJson()) } }

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

/**
 * This account's «Мне нравится», as the core last read it.
 *
 * Stored on disk, so it is on screen with no internet — and the ids in it are
 * what every heart on every list is drawn from.
 */
data class Likes(
    val tracks: List<TrackInfo>,
    /** When Yandex was last asked, in Unix ms. 0 means never. */
    val updatedMs: Long,
) {
    val ids: Set<String> = tracks.map { it.trackId }.toSet()
}

/** A room somebody on this network is holding. */
data class FoundRoom(
    /** Ready to put in the relay field: `ws://<address>:<port>`. */
    val relay: String,
    val room: String,
    val listeners: Int,
    /** False when that relay speaks another protocol version and would refuse us. */
    val compatible: Boolean,
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
    /** Whether the track playing right now is in «Мне нравится». */
    val trackLiked: Boolean,
    /** Bumped whenever «Мне нравится» changes, here or on Yandex. */
    val likedRevision: Long,
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
        albumId = if (isNull("album_id")) null else optString("album_id"),
        album = if (isNull("album")) null else optString("album"),
        coverUri = if (isNull("cover_uri")) null else optString("cover_uri"),
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

fun JSONObject.toLikes() =
    Likes(
        tracks = optJSONArray("tracks")?.toTrackList() ?: emptyList(),
        updatedMs = optLong("updated_ms"),
    )

fun JSONObject.toFoundRoom() =
    FoundRoom(
        relay = optString("relay"),
        room = optString("room"),
        listeners = optInt("listeners"),
        compatible = optBoolean("compatible"),
    )

fun JSONArray.toFoundRooms(): List<FoundRoom> =
    (0 until length()).mapNotNull { optJSONObject(it)?.toFoundRoom() }

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
        trackLiked = optBoolean("track_liked"),
        likedRevision = optLong("liked_revision"),
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

    /** The room's password. Everyone in the room needs the same one. */
    var password: String
        get() = prefs.getString(KEY_ROOM_TOKEN, "")!!
        set(value) = prefs.edit().putString(KEY_ROOM_TOKEN, value).apply()

    var yandexToken: String
        get() = prefs.getString(KEY_YANDEX_TOKEN, "")!!
        set(value) = prefs.edit().putString(KEY_YANDEX_TOKEN, value).apply()

    var volume: Float
        get() = prefs.getFloat(KEY_VOLUME, 0.8f)
        set(value) = prefs.edit().putFloat(KEY_VOLUME, value).apply()

    /** Keep every track that plays, not only the ones downloaded on purpose. */
    var autoCache: Boolean
        get() = prefs.getBoolean(KEY_AUTO_CACHE, false)
        set(value) = prefs.edit().putBoolean(KEY_AUTO_CACHE, value).apply()

    /** Gigabytes. Smaller than the desktop default: this is a phone. */
    var cacheLimitGb: Float
        get() = prefs.getFloat(KEY_CACHE_LIMIT, 2f)
        set(value) = prefs.edit().putFloat(KEY_CACHE_LIMIT, value).apply()

    /**
     * Fills in what a room needs to come up by itself.
     *
     * Pressing «Моя волна» or «Скачанное» outside a room raises one on this phone,
     * and neither a name nor a password is something to stop and ask for at that
     * moment. The password is random rather than empty because the relay refuses
     * a room without one; it is on the room page for whoever wants to join.
     */
    fun prepareForSolo() {
        if (room.isBlank()) room = "home"
        if (password.isBlank()) password = newPassword()
    }

    private fun newPassword(): String {
        val alphabet = "abcdefghijkmnpqrstuvwxyz23456789"
        val random = java.security.SecureRandom()
        return (1..12).map { alphabet[random.nextInt(alphabet.length)] }.joinToString("")
    }

    /** What still has to be filled in before a session can start. */
    val missing: List<String>
        get() = buildList {
            if (yandexToken.isBlank()) add("токен Яндекса")
            if (password.isBlank()) add("пароль комнаты")
            if (room.isBlank()) add("название комнаты")
        }

    /**
     * The settings as the core reads them.
     *
     * `host` is which button was pressed: «Хостить» runs the room's relay on this
     * phone — which is also what listening with no internet looks like, since the
     * queue and the playhead need an authority — and «Подключиться» dials [relay].
     */
    fun configJson(host: Boolean): String =
        JSONObject()
            .put("relay", relay)
            .put("room", room)
            .put("room_token", password)
            .put("yandex_token", yandexToken)
            .put("volume", volume.toDouble())
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
                    .put("enabled", host)
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
        const val KEY_AUTO_CACHE = "auto_cache"
        const val KEY_CACHE_LIMIT = "cache_limit_gb"
    }
}
