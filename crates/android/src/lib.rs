//! JNI surface for the Android client.
//!
//! The phone runs the same engine as the desktop, and since the player moved in
//! here it runs the same audio path too: a track is decoded from memory by
//! `rodio` and handed to a `cpal` stream, which reaches the speakers through
//! AAudio. Kotlin is left with the screen, the foreground service and the
//! permissions.
//!
//! # Why the player moved out of ExoPlayer
//!
//! ExoPlayer streamed the signed URL itself and reported a playhead that trailed
//! its own audio pipeline by a few hundred milliseconds. The engine lines this
//! device up with the room by seeking, so it seeked, was told it had landed short,
//! waited out the cool-down and seeked again — for ever. On the phone that is
//! audible as a stutter every couple of seconds that never settles. A sink that
//! reports the samples it has actually consumed removes the cause instead of
//! tuning around it, and seeking within a buffered track costs a decoder reset
//! rather than a re-buffer.
//!
//! Kotlin therefore no longer reports a player state or applies player commands;
//! it polls for snapshots to draw, and that is all.

use std::mem::ManuallyDrop;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use jni::JNIEnv;
use jni::objects::JObject;

mod ffi;
mod session;

/// Hands the JavaVM and the app context to `ndk_context`, once per process.
///
/// `cpal`'s AAudio backend asks `android.media.AudioTrack.getMinBufferSize` over
/// JNI while working out which stream configurations exist, and reads the device
/// list from the Java `AudioManager`. Both go through `ndk_context` and *panic*
/// when nobody has filled it in, so this has to happen before the first player is
/// built.
pub fn install_android_context(env: &mut JNIEnv, context: &JObject) -> Result<()> {
    static DONE: OnceLock<()> = OnceLock::new();
    if DONE.get().is_some() {
        return Ok(());
    }

    let vm = env
        .get_java_vm()
        .context("не удалось получить JavaVM для звукового вывода")?;
    let context = env
        .new_global_ref(context)
        .context("не удалось сохранить контекст приложения")?;
    // Leaked deliberately: `ndk_context` keeps these as raw pointers for the life
    // of the process, and a dropped global reference would leave it dangling.
    let context = ManuallyDrop::new(context);
    unsafe {
        ndk_context::initialize_android_context(
            vm.get_java_vm_pointer().cast(),
            context.as_raw().cast(),
        );
    }

    let _ = DONE.set(());
    Ok(())
}
