//! The `extern "system"` surface the JVM binds to.
//!
//! Kotlin side (`dev.mshiv.ymsync.Native`):
//!
//! ```kotlin
//! object Native {
//!     init { System.loadLibrary("ymsync_android") }
//!     external fun start(configJson: String, role: String): String
//!     external fun poll(handle: Long, playerStateJson: String): String
//!     external fun send(handle: Long, requestJson: String): String
//!     external fun search(handle: Long, query: String, limit: Int): String
//!     external fun queueFrom(handle: Long, kind: String, value: String, replace: Boolean): String
//!     external fun stop(handle: Long): String
//! }
//! ```
//!
//! Every call answers with `{"ok": …}` or `{"error": "…"}`. Reporting failures in
//! the payload rather than throwing keeps the boundary to one shape and avoids
//! raising Java exceptions from Rust.

use std::fmt::Display;

use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jint, jlong, jstring};

use crate::PlayerState;
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
    role: JString,
) -> jstring {
    let text = match (read(&mut env, &config_json), read(&mut env, &role)) {
        (Ok(config), Ok(role)) => match Session::start(&config, &role) {
            Ok(session) => {
                let handle = Box::into_raw(Box::new(session)) as jlong;
                ok(serde_json::json!({ "handle": handle }))
            }
            Err(err) => failed(err),
        },
        (Err(err), _) | (_, Err(err)) => failed(err),
    };
    reply(&mut env, &text)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_mshiv_ymsync_Native_poll(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    player_state_json: JString,
) -> jstring {
    let text = match read(&mut env, &player_state_json) {
        Err(err) => failed(err),
        Ok(json) => match serde_json::from_str::<PlayerState>(&json) {
            Err(err) => failed(err),
            Ok(state) => match unsafe { borrow(handle) } {
                None => failed("сессия не запущена"),
                Some(session) => ok(session.poll(state)),
            },
        },
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
