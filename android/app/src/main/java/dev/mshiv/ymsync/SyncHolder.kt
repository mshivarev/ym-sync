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
}
