package dev.mshiv.ymsync

import android.content.Context
import android.content.Intent
import androidx.media3.common.MediaItem
import androidx.media3.common.MediaMetadata
import androidx.media3.common.PlaybackException
import androidx.media3.common.Player
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.session.MediaSession
import androidx.media3.session.MediaSessionService
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONObject

/**
 * Runs playback and the poll loop that ties ExoPlayer to the Rust engine.
 *
 * It is a [MediaSessionService] so playback survives the activity and gets
 * lock-screen controls and a notification for free.
 */
class SyncService : MediaSessionService() {
    private lateinit var player: ExoPlayer
    private var session: MediaSession? = null
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)
    private var pollJob: Job? = null

    override fun onCreate() {
        super.onCreate()
        player = ExoPlayer.Builder(this).build()
        // Without this a failed stream leaves the UI showing a staged track that
        // silently never starts: the engine correctly refuses to correct against
        // a player that never reported the track as ready.
        player.addListener(object : Player.Listener {
            override fun onPlayerError(error: PlaybackException) {
                SyncHolder.say("плеер: ${error.errorCodeName}${error.message?.let { " — $it" } ?: ""}")
            }
        })
        session = MediaSession.Builder(this, player).build()
    }

    override fun onGetSession(controllerInfo: MediaSession.ControllerInfo): MediaSession? = session

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_START -> {
                val config = intent.getStringExtra(EXTRA_CONFIG)
                if (config != null) {
                    startSync(config)
                }
            }
            ACTION_STOP -> stopSync()
        }
        return super.onStartCommand(intent, flags, startId)
    }

    override fun onDestroy() {
        pollJob?.cancel()
        session?.release()
        session = null
        player.release()
        scope.cancel()
        super.onDestroy()
    }

    private fun startSync(configJson: String) {
        if (SyncHolder.handle != 0L) return
        scope.launch {
            SyncHolder.say("подключаюсь…")
            val reply = withContext(Dispatchers.IO) { parseReply(Native.start(configJson)) }
            when (reply) {
                is NativeResult.Failed -> {
                    SyncHolder.say(reply.message)
                    stopSelf()
                }
                is NativeResult.Ok -> {
                    SyncHolder.onStarted(reply.value.optLong("handle"))
                    SyncHolder.say(null)
                    startPolling()
                }
            }
        }
    }

    private fun stopSync() {
        pollJob?.cancel()
        pollJob = null
        val handle = SyncHolder.handle
        SyncHolder.onStopped()
        player.pause()
        player.clearMediaItems()
        if (handle != 0L) {
            // Shutting the engine down waits for the relay goodbye, so keep it
            // off the main thread.
            scope.launch(Dispatchers.IO) { Native.stop(handle) }
        }
        stopSelf()
    }

    private fun startPolling() {
        pollJob?.cancel()
        pollJob = scope.launch {
            while (isActive) {
                val handle = SyncHolder.handle
                if (handle == 0L) break
                pollOnce(handle)
                delay(POLL_INTERVAL_MS)
            }
        }
    }

    /** One exchange: our playhead out, the engine's snapshot and orders back. */
    private fun pollOnce(handle: Long) {
        when (val reply = parseReply(Native.poll(handle, playerState().toString()))) {
            is NativeResult.Failed -> SyncHolder.say(reply.message)
            is NativeResult.Ok -> {
                reply.value.optJSONObject("snapshot")?.let {
                    // An unchanged queue is left out of the reply, so the previous
                    // snapshot supplies it.
                    SyncHolder.publish(it.toSnapshot(SyncHolder.snapshot.value))
                }
                reply.value.optJSONArray("commands")?.let(::applyCommands)
            }
        }
    }

    private fun playerState(): JSONObject {
        // Only a prepared player counts as staged. Reporting a track that is
        // still buffering would let the engine correct against a playhead that
        // does not exist yet.
        val prepared = player.playbackState == Player.STATE_READY ||
            player.playbackState == Player.STATE_ENDED
        return JSONObject()
            .put("position_ms", player.currentPosition.coerceAtLeast(0L))
            .put("playing", player.isPlaying)
            .put("finished", player.playbackState == Player.STATE_ENDED)
            .put("loaded_track_id", if (prepared) player.currentMediaItem?.mediaId else null)
            .put("volume", player.volume)
    }

    private fun applyCommands(commands: JSONArray) {
        for (index in 0 until commands.length()) {
            val command = commands.optJSONObject(index) ?: continue
            when (command.optString("kind")) {
                "load" -> load(command)
                "play" -> player.play()
                "pause" -> player.pause()
                "seek" -> player.seekTo(command.optLong("to_ms"))
                "volume" -> player.volume = command.optDouble("value", 1.0).toFloat()
            }
        }
    }

    private fun load(command: JSONObject) {
        val metadata = MediaMetadata.Builder()
            .setTitle(command.optString("title"))
            .setArtist(command.optString("artist"))
            .build()
        val item = MediaItem.Builder()
            // The engine identifies the staged track by this id.
            .setMediaId(command.optString("track_id"))
            .setUri(command.optString("url"))
            .setMediaMetadata(metadata)
            .build()
        player.setMediaItem(item)
        // `playWhenReady` stays false: the engine decides when to start, so this
        // device lands on the room's position instead of playing from zero.
        player.prepare()
    }

    companion object {
        private const val POLL_INTERVAL_MS = 200L
        private const val ACTION_START = "dev.mshiv.ymsync.START"
        private const val ACTION_STOP = "dev.mshiv.ymsync.STOP"
        private const val EXTRA_CONFIG = "config"

        fun start(context: Context, configJson: String) {
            val intent = Intent(context, SyncService::class.java).apply {
                action = ACTION_START
                putExtra(EXTRA_CONFIG, configJson)
            }
            context.startService(intent)
        }

        fun stop(context: Context) {
            val intent = Intent(context, SyncService::class.java).apply { action = ACTION_STOP }
            context.startService(intent)
        }
    }
}
