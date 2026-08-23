package dev.mshiv.ymsync

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.net.wifi.WifiManager
import android.os.IBinder
import android.os.PowerManager
import androidx.core.app.NotificationCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * Keeps the session alive and visible.
 *
 * Playback itself happens in the Rust core, which decodes the track and pushes it
 * to AAudio, so there is no player object here at all. What this service exists
 * for is everything Android insists on: a foreground notification so the process
 * survives the activity, a wake lock so the CPU keeps decoding with the screen
 * off, and — while this phone holds the room — a multicast lock, without which the
 * Wi-Fi driver drops the broadcast that «найти комнаты» on the desktop sends.
 */
class SyncService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)
    private var pollJob: Job? = null
    private var wakeLock: PowerManager.WakeLock? = null
    private var multicastLock: WifiManager.MulticastLock? = null
    /// What the notification currently says, so it is only rebuilt when it changes.
    private var shown: String? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        createChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_START -> {
                val config = intent.getStringExtra(EXTRA_CONFIG)
                if (config != null) {
                    startForeground(NOTIFICATION_ID, notification("подключаюсь…"))
                    startSync(config, intent.getBooleanExtra(EXTRA_HOST, false))
                }
            }
            ACTION_STOP -> stopSync()
            ACTION_TOGGLE -> scope.launch { Commands.send("toggle") }
            ACTION_NEXT -> scope.launch { Commands.send("next") }
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        pollJob?.cancel()
        releaseLocks()
        scope.cancel()
        super.onDestroy()
    }

    private fun startSync(configJson: String, hosting: Boolean) {
        if (SyncHolder.handle != 0L) return
        acquireLocks(hosting)
        scope.launch {
            SyncHolder.say("подключаюсь…")
            // Opens the audio device and dials the room, so not on the main thread.
            val reply = withContext(Dispatchers.IO) {
                parseReply(Native.start(configJson, applicationContext))
            }
            when (reply) {
                is NativeResult.Failed -> {
                    SyncHolder.say(reply.message)
                    releaseLocks()
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
        releaseLocks()
        if (handle != 0L) {
            // Shutting the engine down stops the audio, waits for the relay
            // goodbye and closes the ports, so keep it off the main thread.
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

    /** Takes the engine's snapshot and shows it, on screen and in the shade. */
    private fun pollOnce(handle: Long) {
        when (val reply = parseReply(Native.snapshot(handle))) {
            is NativeResult.Failed -> SyncHolder.say(reply.message)
            is NativeResult.Ok -> {
                // An unchanged queue is left out of the reply, so the previous
                // snapshot supplies it.
                val snapshot = reply.value.toSnapshot(SyncHolder.snapshot.value)
                SyncHolder.publish(snapshot)
                showNotification(snapshot)
            }
        }
    }

    private fun showNotification(snapshot: Snapshot) {
        val text = when {
            snapshot.loading -> "загрузка…"
            snapshot.track != null -> "${snapshot.track.artist} — ${snapshot.track.title}"
            else -> "очередь пуста"
        }
        val line = "$text|${snapshot.playing}"
        if (line == shown) return
        shown = line

        val manager = getSystemService(NotificationManager::class.java)
        manager?.notify(NOTIFICATION_ID, notification(text, snapshot.playing))
    }

    private fun notification(text: String, playing: Boolean = false): Notification =
        NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle("ym-sync")
            .setContentText(text)
            .setSmallIcon(android.R.drawable.ic_media_play)
            .setOnlyAlertOnce(true)
            .setOngoing(true)
            .setContentIntent(
                PendingIntent.getActivity(
                    this,
                    0,
                    Intent(this, MainActivity::class.java),
                    PendingIntent.FLAG_IMMUTABLE,
                ),
            )
            // The room is shared, so these act on everybody's playback — the same
            // as the buttons on screen.
            .addAction(
                if (playing) android.R.drawable.ic_media_pause else android.R.drawable.ic_media_play,
                if (playing) "Пауза" else "Играть",
                action(ACTION_TOGGLE),
            )
            .addAction(android.R.drawable.ic_media_next, "Дальше", action(ACTION_NEXT))
            .addAction(android.R.drawable.ic_delete, "Отключиться", action(ACTION_STOP))
            .build()

    private fun action(name: String): PendingIntent {
        val intent = Intent(this, SyncService::class.java).apply { action = name }
        return PendingIntent.getService(
            this,
            name.hashCode(),
            intent,
            PendingIntent.FLAG_IMMUTABLE,
        )
    }

    private fun createChannel() {
        val channel = NotificationChannel(
            CHANNEL_ID,
            "Воспроизведение",
            NotificationManager.IMPORTANCE_LOW,
        ).apply { setShowBadge(false) }
        getSystemService(NotificationManager::class.java)?.createNotificationChannel(channel)
    }

    /**
     * Takes the two locks playback needs.
     *
     * The wake lock is for the decoder: with the screen off the CPU would
     * otherwise be free to sleep between audio buffers. The multicast lock is only
     * needed while this phone answers «кто держит комнату»: Android's Wi-Fi stack
     * filters out packets that were not addressed to this device, and a discovery
     * query is a broadcast — which is why the desktop could not find a room held
     * on the phone.
     */
    private fun acquireLocks(hosting: Boolean) {
        val power = getSystemService(PowerManager::class.java)
        wakeLock = power?.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "ymsync:playback")?.apply {
            setReferenceCounted(false)
            acquire(WAKE_LOCK_TIMEOUT_MS)
        }
        if (hosting) {
            val wifi = applicationContext.getSystemService(WifiManager::class.java)
            multicastLock = wifi?.createMulticastLock("ymsync:discovery")?.apply {
                setReferenceCounted(false)
                acquire()
            }
        }
    }

    private fun releaseLocks() {
        multicastLock?.takeIf { it.isHeld }?.release()
        multicastLock = null
        wakeLock?.takeIf { it.isHeld }?.release()
        wakeLock = null
    }

    companion object {
        private const val POLL_INTERVAL_MS = 200L
        private const val NOTIFICATION_ID = 1
        private const val CHANNEL_ID = "playback"
        /// A listening session that has run this long without being stopped is a
        /// forgotten one; the lock is not worth holding for ever.
        private const val WAKE_LOCK_TIMEOUT_MS = 8L * 60 * 60 * 1000
        private const val ACTION_START = "dev.mshiv.ymsync.START"
        private const val ACTION_STOP = "dev.mshiv.ymsync.STOP"
        private const val ACTION_TOGGLE = "dev.mshiv.ymsync.TOGGLE"
        private const val ACTION_NEXT = "dev.mshiv.ymsync.NEXT"
        private const val EXTRA_CONFIG = "config"
        private const val EXTRA_HOST = "host"

        /** Joins a room, or holds it on this phone when `host` is set. */
        fun start(context: Context, configJson: String, host: Boolean) {
            val intent = Intent(context, SyncService::class.java).apply {
                action = ACTION_START
                putExtra(EXTRA_CONFIG, configJson)
                putExtra(EXTRA_HOST, host)
            }
            context.startForegroundService(intent)
        }

        fun stop(context: Context) {
            val intent = Intent(context, SyncService::class.java).apply { action = ACTION_STOP }
            context.startService(intent)
        }
    }
}
