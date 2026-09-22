//! The `extern "system"` surface the JVM binds to.
//!
//! Kotlin side (`dev.mshiv.ymsync.Native`):
//!
//! ```kotlin
//! object Native {
//!     init { System.loadLibrary("ymsync_android") }
//!     external fun start(configJson: String, context: Any): String
//!     external fun snapshot(handle: Long): String
//!     external fun send(handle: Long, requestJson: String): String
//!     external fun search(handle: Long, query: String, limit: Int): String
//!     external fun queueTracks(handle: Long, tracksJson: String, replace: Boolean): String
//!     external fun queueFrom(handle: Long, kind: String, value: String, replace: Boolean): String
//!     external fun importTrack(handle: Long, name: String, data: ByteArray): String
//!     external fun library(handle: Long): String
//!     external fun likes(handle: Long): String
//!     external fun findRooms(waitMs: Int): String
//!     external fun stop(handle: Long): String
//! }
//! ```
//!
//! Every call answers with `{"ok": …}` or `{"error": "…"}`. Reporting failures in
//! the payload rather than throwing keeps the boundary to one shape and avoids
//! raising Java exceptions from Rust.
//!
//! All of them take a session handle except `findRooms`, which is what you call
//! before you know where to connect.
//!
//! `start` takes the app context as well: the audio backend reaches
//! `android.media.AudioTrack` over JNI — see [`crate::install_android_context`].

use std::fmt::Display;
use std::time::Duration;

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::sys::{jboolean, jint, jlong, jstring};

use crate::session::{Request, Session};

/// Reconstructs a borrowed session from a handle.
///
/// # Safety
///
/// `handle` must be a value returned by [`Java_dev_mshiv_ymsync_Native_start`]
/// that has not yet been passed to [`Java_dev_mshiv_ymsync_Native_stop`].
unsafe fn borrow(handle: jlong) -> Option<&'static Session> {
    if handle == 0 {
        return None;
    }
    Some(unsafe { &*(handle as *const Session) })
}

fn ok(value: serde_json::Value) -> String {
    serde_json::json!({ "ok": value }).to_string()
}

fn failed(error: impl Display) -> String {
    serde_json::json!({ "error": format!("{error:#}") }).to_string()
}

/// Builds the Java string to hand back. A null return means even that failed,
/// which the JVM sees as a pending exception.
fn reply(env: &mut JNIEnv, text: &str) -> jstring {
    match env.new_string(text) {
        Ok(value) => value.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

fn read(env: &mut JNIEnv, value: &JString) -> Result<String, String> {
    env.get_string(value)
        .map(Into::into)
        .map_err(|err| format!("{err}"))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_mshiv_ymsync_Native_start(
    mut env: JNIEnv,
    _class: JClass,
    config_json: JString,
    context: JObject,
) -> jstring {
    let text = match read(&mut env, &config_json) {
        Err(err) => failed(err),
        // Before the session, because building it opens the audio device, and the
        // audio device is reached through the JVM.
        Ok(config) => match crate::install_android_context(&mut env, &context) {
            Err(err) => failed(err),
            Ok(()) => match Session::start(&config) {
                Ok(session) => {
                    let handle = Box::into_raw(Box::new(session)) as jlong;
                    ok(serde_json::json!({ "handle": handle }))
                }
                Err(err) => failed(err),
            },
        },
    };
    reply(&mut env, &text)
}

/// What to draw right now.
///
/// Kotlin calls this on a timer. Nothing is handed in: since the player lives in
/// Rust, the engine no longer has to be told what the audio is doing.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_mshiv_ymsync_Native_snapshot(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    let text = match unsafe { borrow(handle) } {
        None => failed("сессия не запущена"),
        Some(session) => ok(session.snapshot()),
    };
    reply(&mut env, &text)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_mshiv_ymsync_Native_send(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    request_json: JString,
) -> jstring {
    let text = match read(&mut env, &request_json) {
        Err(err) => failed(err),
        Ok(json) => match serde_json::from_str::<Request>(&json) {
            Err(err) => failed(err),
            Ok(request) => match unsafe { borrow(handle) } {
                None => failed("сессия не запущена"),
                Some(session) => {
                    session.send(request);
                    ok(serde_json::Value::Null)
                }
            },
        },
    };
    reply(&mut env, &text)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_mshiv_ymsync_Native_search(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    query: JString,
    limit: jint,
) -> jstring {
    let text = match read(&mut env, &query) {
        Err(err) => failed(err),
        Ok(query) => match unsafe { borrow(handle) } {
            None => failed("сессия не запущена"),
            Some(session) => match session.search(&query, limit.max(1) as usize) {
                Ok(tracks) => ok(serde_json::json!({ "tracks": tracks })),
                Err(err) => failed(err),
            },
        },
    };
    reply(&mut env, &text)
}

/// Queues tracks the screen already holds in full.
///
/// Separate from `queueFrom` because a search result and a downloaded track need
/// nothing looked up: tapping one used to cost a Yandex request for metadata that
/// was already on screen — and for a track on this disk, a trip to the internet to
/// play something that was already here.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_mshiv_ymsync_Native_queueTracks(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    tracks_json: JString,
    replace: jboolean,
) -> jstring {
    let text = match read(&mut env, &tracks_json) {
        Err(err) => failed(err),
        Ok(json) => match unsafe { borrow(handle) } {
            None => failed("сессия не запущена"),
            Some(session) => match session.queue_tracks(&json, replace != 0) {
                Ok(length) => ok(serde_json::json!({ "queued": length })),
                Err(err) => failed(err),
            },
        },
    };
    reply(&mut env, &text)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_mshiv_ymsync_Native_queueFrom(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    kind: JString,
    value: JString,
    replace: jboolean,
) -> jstring {
    let text = match (read(&mut env, &kind), read(&mut env, &value)) {
        (Err(err), _) | (_, Err(err)) => failed(err),
        (Ok(kind), Ok(value)) => match unsafe { borrow(handle) } {
            None => failed("сессия не запущена"),
            Some(session) => match session.queue_from(&kind, &value, replace != 0) {
                Ok(length) => ok(serde_json::json!({ "queued": length })),
                Err(err) => failed(err),
            },
        },
    };
    reply(&mut env, &text)
}

/// Adds a local audio file to this device's downloads.
///
/// Takes the bytes rather than a path because Android's picker answers with a
/// content URI: only the app that asked can open it, so Kotlin reads the file and
/// hands over what it read.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_mshiv_ymsync_Native_importTrack(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    name: JString,
    data: JByteArray,
) -> jstring {
    let bytes = env
        .convert_byte_array(&data)
        .map_err(|err| format!("не удалось прочитать файл: {err}"));

    let text = match (read(&mut env, &name), bytes) {
        (Err(err), _) => failed(err),
        (_, Err(err)) => failed(err),
        (Ok(name), Ok(bytes)) => match unsafe { borrow(handle) } {
            None => failed("сессия не запущена"),
            Some(session) => match session.import(&name, bytes) {
                Ok(value) => ok(value),
                Err(err) => failed(err),
            },
        },
    };
    reply(&mut env, &text)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_mshiv_ymsync_Native_library(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    let text = match unsafe { borrow(handle) } {
        None => failed("сессия не запущена"),
        Some(session) => ok(session.library()),
    };
    reply(&mut env, &text)
}

/// This account's «Мне нравится».
///
/// Reads the stored list, so it costs no network and no Yandex request — the
/// hearts on every screen are drawn from it. Refreshing it from Yandex is a
/// `send({"action":"refresh_likes"})` instead, because that one does.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_mshiv_ymsync_Native_likes(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    let text = match unsafe { borrow(handle) } {
        None => failed("сессия не запущена"),
        Some(session) => ok(session.likes()),
    };
    reply(&mut env, &text)
}

/// Asks the local network which rooms are out there.
///
/// The one call with no handle: discovery is what you do *before* connecting, so
/// there is no session to hang it on. A one-shot runtime is cheaper than keeping
/// one alive for a button that may never be pressed, and the call blocks for the
/// wait it was given — Kotlin makes it off the main thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_mshiv_ymsync_Native_findRooms(
    mut env: JNIEnv,
    _class: JClass,
    wait_ms: jint,
) -> jstring {
    let text = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Err(err) => failed(err),
        Ok(runtime) => {
            let wait = Duration::from_millis(wait_ms.clamp(100, 5_000) as u64);
            match runtime.block_on(ymsync::discover::find_rooms(wait)) {
                Ok(rooms) => ok(serde_json::json!({ "rooms": rooms })),
                Err(err) => failed(err),
            }
        }
    };
    reply(&mut env, &text)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_mshiv_ymsync_Native_stop(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    let text = if handle == 0 {
        failed("сессия не запущена")
    } else {
        // Takes ownership back from the JVM side and shuts the engine down.
        let session = unsafe { Box::from_raw(handle as *mut Session) };
        session.stop();
        ok(serde_json::Value::Null)
    };
    reply(&mut env, &text)
}
