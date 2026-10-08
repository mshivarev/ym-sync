package dev.mshiv.ymsync

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.withContext
import org.json.JSONObject

/**
 * State shared between the playback service and the UI.
 *
 * Both live in the same process, so a holder is enough — no binder, no
 * serialising snapshots twice.
 */
object SyncHolder {
    private val _snapshot = MutableStateFlow<Snapshot?>(null)
    val snapshot: StateFlow<Snapshot?> = _snapshot.asStateFlow()

    private val _running = MutableStateFlow(false)
    val running: StateFlow<Boolean> = _running.asStateFlow()

    private val _message = MutableStateFlow<String?>(null)
    val message: StateFlow<String?> = _message.asStateFlow()

    @Volatile
    var handle: Long = 0L
        private set

    fun onStarted(handle: Long) {
        this.handle = handle
        _running.value = true
    }

    fun onStopped() {
        handle = 0L
        _running.value = false
        _snapshot.value = null
    }

    fun publish(snapshot: Snapshot) {
        _snapshot.value = snapshot
    }

    fun say(message: String?) {
        _message.value = message
    }
}

/**
 * Calls into the core that the UI makes directly. Each one talks to the network,
 * so none of them belong on the main thread.
 */
object Commands {
    suspend fun send(action: String, build: JSONObject.() -> Unit = {}): String? {
        val handle = SyncHolder.handle
        if (handle == 0L) return "нет подключения"
        val request = JSONObject().put("action", action).apply(build).toString()
        return when (val reply = withContext(Dispatchers.IO) { parseReply(Native.send(handle, request)) }) {
            is NativeResult.Failed -> reply.message
            is NativeResult.Ok -> null
        }
    }

    suspend fun search(query: String, limit: Int = 30): Result<List<TrackInfo>> {
        val handle = SyncHolder.handle
        if (handle == 0L) return Result.failure(IllegalStateException("нет подключения"))
        val reply = withContext(Dispatchers.IO) { parseReply(Native.search(handle, query, limit)) }
        return when (reply) {
            is NativeResult.Failed -> Result.failure(IllegalStateException(reply.message))
            is NativeResult.Ok ->
                Result.success(reply.value.optJSONArray("tracks")?.toTrackList() ?: emptyList())
        }
    }

    /**
     * Queues tracks the screen already holds in full: a search result, a row of
     * the offline library.
     *
     * Costs no Yandex request — which is why tapping a downloaded track now plays
     * off the disk instead of going to the internet for metadata that was already
     * on screen. The core still decides where the audio comes from: this disk
     * first, then somebody in the room, then Yandex.
     */
    suspend fun queueTracks(
        tracks: List<TrackInfo>,
        replace: Boolean = false,
        /** The track to play first when [replace] is set. */
        start: Int = 0,
    ): Result<Int> {
        val handle = SyncHolder.handle
        if (handle == 0L) return Result.failure(IllegalStateException("нет подключения"))
        if (tracks.isEmpty()) return Result.success(0)
        val payload = tracks.toJsonArray().toString()
        val reply = withContext(Dispatchers.IO) {
            parseReply(Native.queueTracks(handle, payload, replace, start))
        }
        return when (reply) {
            is NativeResult.Failed -> Result.failure(IllegalStateException(reply.message))
            is NativeResult.Ok -> Result.success(reply.value.optInt("queued"))
        }
    }

    /**
     * Loads a source into the queue. `replace` throws the current queue away;
     * otherwise the tracks go on the end of it.
     */
    suspend fun queueFrom(kind: String, value: String, replace: Boolean): Result<Int> {
        val handle = SyncHolder.handle
        if (handle == 0L) return Result.failure(IllegalStateException("нет подключения"))
        val reply = withContext(Dispatchers.IO) {
            parseReply(Native.queueFrom(handle, kind, value, replace))
        }
        return when (reply) {
            is NativeResult.Failed -> Result.failure(IllegalStateException(reply.message))
            is NativeResult.Ok -> Result.success(reply.value.optInt("queued"))
        }
    }

    /**
     * What is downloaded on this device.
     *
     * Unlike everything else here this touches no network — it reads an index the
     * core keeps in memory — but it still goes through the core, so it stays off
     * the main thread with the rest.
     */
    suspend fun library(): Result<Library> {
        val handle = SyncHolder.handle
        if (handle == 0L) return Result.failure(IllegalStateException("нет подключения"))
        val reply = withContext(Dispatchers.IO) { parseReply(Native.library(handle)) }
        return when (reply) {
            is NativeResult.Failed -> Result.failure(IllegalStateException(reply.message))
            is NativeResult.Ok -> Result.success(reply.value.toLibrary())
        }
    }

    /**
     * Albums matching the query. An empty list on failure: the tracks below are
     * the search's answer, and a missing row of albums is not worth a message.
     */
    suspend fun searchAlbums(query: String, limit: Int = 12): List<AlbumInfo> {
        val handle = SyncHolder.handle
        if (handle == 0L) return emptyList()
        val reply = withContext(Dispatchers.IO) { parseReply(Native.searchAlbums(handle, query, limit)) }
        return when (reply) {
            is NativeResult.Failed -> emptyList()
            is NativeResult.Ok -> reply.value.optJSONArray("albums")
                ?.let { array -> (0 until array.length()).mapNotNull { array.optJSONObject(it)?.toAlbumInfo() } }
                ?: emptyList()
        }
    }

    /** An album's tracks, in disc order, for its screen. */
    suspend fun albumTracks(albumId: String): Result<List<TrackInfo>> {
        val handle = SyncHolder.handle
        if (handle == 0L) return Result.failure(IllegalStateException("нет подключения"))
        val reply = withContext(Dispatchers.IO) { parseReply(Native.albumTracks(handle, albumId)) }
        return when (reply) {
            is NativeResult.Failed -> Result.failure(IllegalStateException(reply.message))
            is NativeResult.Ok -> Result.success(reply.value.optJSONArray("tracks")?.toTrackList() ?: emptyList())
        }
    }

    /**
     * What to offer while the listener is typing.
     *
     * Failures are answered with an empty list rather than reported: a dropdown
     * that did not appear is not worth a message over the player.
     */
    suspend fun suggest(part: String): Suggest {
        val handle = SyncHolder.handle
        val empty = Suggest(best = null, suggestions = emptyList())
        if (handle == 0L) return empty
        val reply = withContext(Dispatchers.IO) { parseReply(Native.suggest(handle, part)) }
        return when (reply) {
            is NativeResult.Failed -> empty
            is NativeResult.Ok -> reply.value.toSuggest()
        }
    }

    /**
     * Adds a local audio file to the downloads on this device.
     *
     * Answers whether the file was new: `false` means the same file was already
     * among the downloads, and the screen counts the two separately — «добавлено»
     * must not include what was there before. Reading and storing a file is disk
     * work, hence [Dispatchers.IO].
     */
    suspend fun importTrack(name: String, data: ByteArray): Result<Boolean> {
        val handle = SyncHolder.handle
        if (handle == 0L) return Result.failure(IllegalStateException("нет подключения"))
        val reply = withContext(Dispatchers.IO) {
            parseReply(Native.importTrack(handle, name, data))
        }
        return when (reply) {
            is NativeResult.Failed -> Result.failure(IllegalStateException(reply.message))
            is NativeResult.Ok -> Result.success(!reply.value.optBoolean("already_there"))
        }
    }

    /**
     * Asks the local network which rooms are out there.
     *
     * The one call here that needs no session: it is what you do before you know
     * where to connect, so there is no handle to check.
     */
    suspend fun findRooms(waitMs: Int = 700): Result<List<FoundRoom>> {
        val reply = withContext(Dispatchers.IO) { parseReply(Native.findRooms(waitMs)) }
        return when (reply) {
            is NativeResult.Failed -> Result.failure(IllegalStateException(reply.message))
            is NativeResult.Ok ->
                Result.success(reply.value.optJSONArray("rooms")?.toFoundRooms() ?: emptyList())
        }
    }

    /**
     * This account's «Мне нравится», as last read.
     *
     * Like [library] this reads what the core already holds — the list is stored
     * on disk — so it answers with no internet. Re-reading it from Yandex is
     * `send("refresh_likes")`.
     */
    suspend fun likes(): Result<Likes> {
        val handle = SyncHolder.handle
        if (handle == 0L) return Result.failure(IllegalStateException("нет подключения"))
        val reply = withContext(Dispatchers.IO) { parseReply(Native.likes(handle)) }
        return when (reply) {
            is NativeResult.Failed -> Result.failure(IllegalStateException(reply.message))
            is NativeResult.Ok -> Result.success(reply.value.toLikes())
        }
    }

    /**
     * Puts a track into «Мне нравится», or takes it out.
     *
     * The whole track goes down, not its id: a like from a search result then costs
     * no lookup, and the stored list gets a row that can be played. Likes belong to
     * this account — the rest of the room sees nothing.
     */
    suspend fun like(track: TrackInfo, liked: Boolean): String? =
        send("like") {
            put("track", track.toJson())
            put("liked", liked)
        }
}
