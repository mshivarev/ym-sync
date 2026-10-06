package dev.mshiv.ymsync

/**
 * The Rust core. Every call answers with a JSON object holding either `ok` or
 * `error`; see `crates/android/src/ffi.rs`.
 *
 * The core plays the music itself — decoding in Rust, out through AAudio — so
 * nothing here hands it a player state or takes player commands back. This side
 * asks what to draw and sends what the user pressed.
 */
object Native {
    init {
        System.loadLibrary("ymsync_android")
    }

    /**
     * Starts a session. `context` is the app context: the audio backend asks
     * `android.media.AudioTrack` for its buffer sizes over JNI.
     *
     * Opens the audio device and connects to the room, so keep it off the main
     * thread.
     */
    external fun start(configJson: String, context: Any): String

    /** What to draw right now. Called on a timer. */
    external fun snapshot(handle: Long): String

    external fun send(handle: Long, requestJson: String): String
    external fun search(handle: Long, query: String, limit: Int): String

    /**
     * What to offer while somebody is typing: Yandex's own suggest endpoint.
     * Cheap enough for a keystroke, unlike a full search.
     */
    external fun suggest(handle: Long, part: String): String

    /**
     * Queues tracks the screen already holds in full — a search result, a row of
     * the offline library. Costs no Yandex request, which is the point.
     */
    external fun queueTracks(handle: Long, tracksJson: String, replace: Boolean, start: Int): String

    /** Queues a whole source by name: an album, a playlist, the wave. */
    external fun queueFrom(handle: Long, kind: String, value: String, replace: Boolean): String

    /**
     * Adds a local audio file to this device's downloads.
     *
     * Takes the bytes because the picker answers with a content URI, which only
     * this app may open — so this side reads the file and passes what it read.
     * Needs no account and no network.
     */
    external fun importTrack(handle: Long, name: String, data: ByteArray): String

    /** What is downloaded on this device. Reads an in-memory index; no network. */
    external fun library(handle: Long): String

    /**
     * This account's «Мне нравится», as last read from Yandex.
     *
     * Comes off the disk, so it needs no network and no request — the hearts on
     * every list are drawn from it. Re-reading it from Yandex is
     * `send({"action":"refresh_likes"})`, which does.
     */
    external fun likes(handle: Long): String

    /**
     * Asks the local network which rooms are out there.
     *
     * The one call that needs no session: it is what you do before you know where
     * to connect. Blocks for up to `waitMs`, so keep it off the main thread.
     */
    external fun findRooms(waitMs: Int): String
    external fun stop(handle: Long): String
}
