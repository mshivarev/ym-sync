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

/** Mirrors `ymsync::engine::Snapshot`. */
data class Snapshot(
    val role: String,
    val connected: Boolean,
    val peers: Int,
    val queue: List<TrackInfo>,
    val index: Int,
    val track: TrackInfo?,
    val positionMs: Long,
    val durationMs: Long,
    val playing: Boolean,
    val loading: Boolean,
    val driftMs: Long?,
    val rttMs: Long?,
    val notice: String?,
) {
    val isMaster: Boolean get() = role == "master"
}

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

/**
 * Reads a snapshot from the core.
 *
 * The core leaves `queue` out when it has not changed — «Мне нравится» is over a
 * thousand tracks and would otherwise travel five times a second — so an absent
 * queue means "the same as before", not "empty".
 */
fun JSONObject.toSnapshot(previous: Snapshot? = null) =
    Snapshot(
        role = optString("role"),
        connected = optBoolean("connected"),
        peers = optInt("peers"),
        queue = optJSONArray("queue")?.toTrackList() ?: previous?.queue ?: emptyList(),
        index = optInt("index"),
        track = optJSONObject("track")?.toTrackInfo(),
        positionMs = optLong("position_ms"),
        durationMs = optLong("duration_ms"),
        playing = optBoolean("playing"),
        loading = optBoolean("loading"),
        // `optLong` would turn a missing drift into a misleading zero.
        driftMs = if (isNull("drift_ms")) null else optLong("drift_ms"),
        rttMs = if (isNull("rtt_ms")) null else optLong("rtt_ms"),
        notice = if (isNull("notice")) null else optString("notice"),
    )

fun formatMs(ms: Long): String {
    val seconds = (ms.coerceAtLeast(0)) / 1000
    return "%d:%02d".format(seconds / 60, seconds % 60)
}

/**
 * Connection settings, kept in shared preferences. The field names in
 * [configJson] must match `ymsync::config::Config`, which rejects unknown keys.
 */
class Settings(context: Context) {
    private val prefs = context.getSharedPreferences("ymsync", Context.MODE_PRIVATE)

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
            .toString()

    private companion object {
        const val KEY_RELAY = "relay"
        const val KEY_ROOM = "room"
        const val KEY_ROOM_TOKEN = "room_token"
        const val KEY_YANDEX_TOKEN = "yandex_token"
        const val KEY_VOLUME = "volume"
        const val KEY_BIAS = "position_bias_ms"
    }
}
