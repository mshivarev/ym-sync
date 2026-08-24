package dev.mshiv.ymsync

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.media.MediaMetadata
import android.media.session.MediaSession
import android.media.session.PlaybackState
import android.net.wifi.WifiManager
import android.os.IBinder
import android.os.PowerManager
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
 * Keeps the session alive, visible, and drivable from outside the app.
 *
 * Playback itself happens in the Rust core, which decodes the track and pushes it
 * to AAudio, so there is no player object here at all. What this service exists
 * for is everything Android insists on: a foreground notification so the process
 * survives the activity, a wake lock so the CPU keeps decoding with the screen
 * off, and — while this phone holds the room — a multicast lock, without which the
 * Wi-Fi driver drops the broadcast that «найти комнаты» on the desktop sends.
 *
 * # The player in the shade
 *
 * The notification is a media one: it carries a [MediaSession] whose metadata and
 * playback state Android draws into the media panel — title, artist, a seek bar
 * that scrubs, and buttons for previous, pause, next and «Мне нравится». The
 * session is not decoration. From Android 13 the system builds those controls from
 * the session's [PlaybackState] rather than from the notification's own actions,
 * and it is also what routes headset and watch buttons here. Both paths are filled
 * in, since older versions read the notification instead.
 *
 * Everything except the heart acts on the *room*: pausing here pauses for
 * everybody, exactly like the buttons on screen. The heart is the one control that
 * is local — likes belong to this account.
 */
class SyncService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)
    private var pollJob: Job? = null
    private var wakeLock: PowerManager.WakeLock? = null
    private var multicastLock: WifiManager.MulticastLock? = null
    private var session: MediaSession? = null
    /// What the notification currently says, so it is only rebuilt when it changes.
    private var shown: String? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        createChannel()
        openSession()
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
            ACTION_PREV -> scope.launch { Commands.send("prev") }
            ACTION_LIKE -> like()
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        pollJob?.cancel()
        releaseLocks()
        session?.release()
        session = null
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
        // Give up the media panel with the session: a stopped player that still
        // sits in the shade with working buttons is worse than none.
        session?.isActive = false
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
                publishToSession(snapshot)
                showNotification(snapshot)
            }
        }
    }

    /**
     * Hands the current track and playhead to the media session.
     *
     * Done on every poll, unlike the notification: this is what moves the seek bar
     * in the shade, and it costs nothing — the system diffs it. The metadata is
     * only rewritten on a track change, because replacing it resets the bar.
     */
    private fun publishToSession(snapshot: Snapshot) {
        val session = session ?: return
        val track = snapshot.track

        if (track != null && metadataFor != track.trackId) {
            metadataFor = track.trackId
            session.setMetadata(
                MediaMetadata.Builder()
                    .putString(MediaMetadata.METADATA_KEY_TITLE, track.title)
                    .putString(MediaMetadata.METADATA_KEY_ARTIST, track.artist)
                    .putString(MediaMetadata.METADATA_KEY_ALBUM, track.album ?: "")
                    // Without a duration the system draws no seek bar at all.
                    .putLong(MediaMetadata.METADATA_KEY_DURATION, track.durationMs)
                    .build(),
            )
        }

        val state = when {
            snapshot.loading -> PlaybackState.STATE_BUFFERING
            snapshot.playing -> PlaybackState.STATE_PLAYING
            else -> PlaybackState.STATE_PAUSED
        }
        session.setPlaybackState(
            PlaybackState.Builder()
                .setActions(
                    PlaybackState.ACTION_PLAY or
                        PlaybackState.ACTION_PAUSE or
                        PlaybackState.ACTION_PLAY_PAUSE or
                        PlaybackState.ACTION_SKIP_TO_NEXT or
                        PlaybackState.ACTION_SKIP_TO_PREVIOUS or
                        PlaybackState.ACTION_SEEK_TO or
                        PlaybackState.ACTION_STOP,
                )
                // From Android 13 the buttons in the media panel come from here,
                // not from the notification, so the heart has to be a session
                // action as well as a notification one.
                .addCustomAction(
                    PlaybackState.CustomAction.Builder(
                        ACTION_LIKE,
                        if (snapshot.trackLiked) "Убрать из «Мне нравится»" else "Мне нравится",
                        if (snapshot.trackLiked) R.drawable.ic_heart else R.drawable.ic_heart_outline,
                    ).build(),
                )
                // Speed 0 while paused, or the bar keeps creeping on its own.
                .setState(state, snapshot.positionMs, if (snapshot.playing) 1f else 0f)
                .build(),
        )
        session.isActive = track != null
    }

    private fun showNotification(snapshot: Snapshot) {
        val text = when {
            snapshot.loading -> "загрузка…"
            snapshot.track != null -> "${snapshot.track.artist} — ${snapshot.track.title}"
            else -> "очередь пуста"
        }
        val line = "$text|${snapshot.playing}|${snapshot.trackLiked}"
        if (line == shown) return
        shown = line

        val manager = getSystemService(NotificationManager::class.java)
        manager?.notify(
            NOTIFICATION_ID,
            notification(
                text = text,
                title = snapshot.track?.title,
                playing = snapshot.playing,
                liked = snapshot.trackLiked,
            ),
        )
    }

    /**
     * The media notification.
     *
     * Built with the platform builder rather than `NotificationCompat`, because the
     * media style that ties a notification to a [MediaSession] is the platform one
     * — and there is nothing here that needs a support library: the app is API 26
     * and up.
     */
    private fun notification(
        text: String,
        title: String? = null,
        playing: Boolean = false,
        liked: Boolean = false,
    ): Notification {
        val builder = Notification.Builder(this, CHANNEL_ID)
            // Title and text swap roles once something is playing: the track is
            // the subject, and the artist line is what a media notification shows
            // underneath it.
            .setContentTitle(title ?: "ym-sync")
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
            // as the buttons on screen. The heart is the exception: a like belongs
            // to this account alone.
            .addAction(action(android.R.drawable.ic_media_previous, "Назад", ACTION_PREV))
            .addAction(
                action(
                    if (playing) android.R.drawable.ic_media_pause else android.R.drawable.ic_media_play,
                    if (playing) "Пауза" else "Играть",
                    ACTION_TOGGLE,
                ),
            )
            .addAction(action(android.R.drawable.ic_media_next, "Дальше", ACTION_NEXT))
            .addAction(
                action(
                    if (liked) R.drawable.ic_heart else R.drawable.ic_heart_outline,
                    if (liked) "Убрать из «Мне нравится»" else "Мне нравится",
                    ACTION_LIKE,
                ),
            )
            .addAction(action(android.R.drawable.ic_delete, "Отключиться", ACTION_STOP))

        val style = Notification.MediaStyle()
            // Which three survive when the shade is collapsed.
            .setShowActionsInCompactView(0, 1, 2)
        session?.let { style.setMediaSession(it.sessionToken) }
        return builder.setStyle(style).build()
    }

    private fun action(icon: Int, label: String, name: String): Notification.Action =
        Notification.Action.Builder(
            android.graphics.drawable.Icon.createWithResource(this, icon),
            label,
            pending(name),
        ).build()

    private fun pending(name: String): PendingIntent {
        val intent = Intent(this, SyncService::class.java).apply { action = name }
        return PendingIntent.getService(
            this,
            name.hashCode(),
            intent,
            PendingIntent.FLAG_IMMUTABLE,
        )
    }

    /**
     * The session Android draws the media controls from, and routes hardware keys
     * to. Its callbacks are the same commands the buttons on screen send, so a
     * headset, a watch and the shade all drive the room the same way.
     */
    private fun openSession() {
        val session = MediaSession(this, "ymsync")
        session.setCallback(object : MediaSession.Callback() {
            override fun onPlay() {
                scope.launch { Commands.send("toggle") }
            }

            override fun onPause() {
                scope.launch { Commands.send("toggle") }
            }

            override fun onSkipToNext() {
                scope.launch { Commands.send("next") }
            }

            override fun onSkipToPrevious() {
                scope.launch { Commands.send("prev") }
            }

            override fun onSeekTo(pos: Long) {
                scope.launch { Commands.send("seek") { put("to_ms", pos.coerceAtLeast(0)) } }
            }

            override fun onStop() = stopSync()

            override fun onCustomAction(action: String, extras: android.os.Bundle?) {
                if (action == ACTION_LIKE) like()
            }
        })
        this.session = session
    }

    /**
     * The heart, from the shade or from a watch.
     *
     * The wanted state is read off the latest snapshot rather than kept here: a
     * toggle computed from a stale local flag is how a heart ends up meaning the
     * opposite of what was pressed.
     */
    private fun like() {
        val snapshot = SyncHolder.snapshot.value
        val track = snapshot?.track
        if (track == null) {
            SyncHolder.say("сейчас ничего не играет")
            return
        }
        scope.launch { Commands.like(track, !snapshot.trackLiked)?.let { SyncHolder.say(it) } }
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

    /// Which track the session metadata describes. Rewriting it on every poll
    /// would restart the seek bar four times a second.
    private var metadataFor: String? = null

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
        private const val ACTION_PREV = "dev.mshiv.ymsync.PREV"
        private const val ACTION_LIKE = "dev.mshiv.ymsync.LIKE"
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
