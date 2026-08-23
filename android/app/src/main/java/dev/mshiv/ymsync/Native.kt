package dev.mshiv.ymsync

/**
 * The Rust core. Every call answers with a JSON object holding either `ok` or
 * `error`; see `crates/android/src/ffi.rs`.
 */
object Native {
    init {
        System.loadLibrary("ymsync_android")
    }

    external fun start(configJson: String): String
    external fun poll(handle: Long, playerStateJson: String): String
    external fun send(handle: Long, requestJson: String): String
    external fun search(handle: Long, query: String, limit: Int): String
    external fun queueFrom(handle: Long, kind: String, value: String, replace: Boolean): String

    /** What is downloaded on this device. Reads an in-memory index; no network. */
    external fun library(handle: Long): String

    /**
     * Asks the local network which rooms are out there.
     *
     * The one call that needs no session: it is what you do before you know where
     * to connect. Blocks for up to `waitMs`, so keep it off the main thread.
     */
    external fun findRooms(waitMs: Int): String
    external fun stop(handle: Long): String
}
