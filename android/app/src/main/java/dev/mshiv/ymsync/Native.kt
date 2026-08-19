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
    external fun stop(handle: Long): String
}
