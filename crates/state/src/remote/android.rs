use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::Result;
use gpui::App;
use jni::objects::{Global, JClass, JObject, JString, JValue};
use jni::{Env, EnvUnowned, JavaVM, NativeMethod, jni_sig, jni_str};
use music::Track;
use tokio::sync::mpsc;

use super::Command;
use crate::{PlaybackState, Repeat};

static COMMANDS: Mutex<Option<mpsc::UnboundedSender<Command>>> = Mutex::new(None);
static ACTIVITY_CLASS: OnceLock<Global<JClass<'static>>> = OnceLock::new();
static REGISTERED: OnceLock<()> = OnceLock::new();
static INIT_LOCK: Mutex<()> = Mutex::new(());

/// Android `MediaSession` and media notification controls bridged to `SonoraActivity` via JNI.
pub struct Controls {
    last_state: i32,
}

impl Controls {
    pub fn new(
        _hwnd: Option<*mut c_void>,
        commands: mpsc::UnboundedSender<Command>,
        _cx: &mut App,
    ) -> Result<Self> {
        *COMMANDS.lock().unwrap_or_else(|e| e.into_inner()) = Some(commands);
        match register_natives() {
            Ok(()) => log::info!("remote: android media controls attached"),
            Err(error) => log::warn!("remote: cannot register Android media callback: {error:#}"),
        }
        Ok(Self { last_state: 0 })
    }

    pub fn describe(&mut self, track: Option<&Track>, cover: Option<&str>, _cx: &App) {
        let (title, artist, album, duration_ms) = match track {
            Some(track) => (
                track.name.as_str(),
                track.artists.as_str(),
                track.album.as_str(),
                i64::try_from(track.duration.as_millis()).unwrap_or(0),
            ),
            None => ("", "", "", 0),
        };
        let cover_path = cover
            .unwrap_or("")
            .strip_prefix("file://")
            .unwrap_or_else(|| cover.unwrap_or(""));
        log::debug!(
            "remote: android describe title={title:?} cover={}",
            !cover_path.is_empty()
        );
        with_activity(|env, cls| {
            let jtitle = env.new_string(title)?;
            let jartist = env.new_string(artist)?;
            let jalbum = env.new_string(album)?;
            let jcover = env.new_string(cover_path)?;
            env.call_static_method(
                cls,
                jni_str!("updateMediaMetadata"),
                jni_sig!((java.lang.String, java.lang.String, java.lang.String, long, java.lang.String) -> void),
                &[
                    JValue::from(&jtitle),
                    JValue::from(&jartist),
                    JValue::from(&jalbum),
                    JValue::Long(duration_ms),
                    JValue::from(&jcover),
                ],
            )?;
            Ok(())
        });
    }

    pub fn set_playback(&mut self, state: &PlaybackState, at: Duration) {
        let (code, speed) = match state {
            PlaybackState::Playing => (1, 1.0f32),
            PlaybackState::Paused => (2, 0.0f32),
            PlaybackState::Loading => (3, 0.0f32),
            PlaybackState::Idle | PlaybackState::Failed(_) => (0, 0.0f32),
        };
        self.last_state = code;
        let pos_ms = i64::try_from(at.as_millis()).unwrap_or(0);
        log::debug!("remote: android playback state={code} pos_ms={pos_ms}");
        with_activity(|env, cls| {
            env.call_static_method(
                cls,
                jni_str!("updateMediaPlayback"),
                jni_sig!((int, long, float) -> void),
                &[
                    JValue::Int(code),
                    JValue::Long(pos_ms),
                    JValue::Float(speed),
                ],
            )?;
            Ok(())
        });
    }

    pub fn seeked(&mut self, at: Duration) {
        let code = self.last_state;
        let speed = match code {
            1 => 1.0f32,
            _ => 0.0f32,
        };
        let pos_ms = i64::try_from(at.as_millis()).unwrap_or(0);
        with_activity(|env, cls| {
            env.call_static_method(
                cls,
                jni_str!("updateMediaPlayback"),
                jni_sig!((int, long, float) -> void),
                &[
                    JValue::Int(code),
                    JValue::Long(pos_ms),
                    JValue::Float(speed),
                ],
            )?;
            Ok(())
        });
    }

    pub fn set_volume(&mut self, _level: f64) {}

    pub fn set_shuffle(&mut self, _on: bool) {}

    pub fn set_repeat(&mut self, _repeat: Repeat) {}
}

fn activity_class<'env>(
    env: &mut Env<'env>,
    app_ptr: jni::sys::jobject,
) -> jni::errors::Result<&'static Global<JClass<'static>>> {
    if let Some(cls) = ACTIVITY_CLASS.get() {
        return Ok(cls);
    }
    let _guard = INIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cls) = ACTIVITY_CLASS.get() {
        return Ok(cls);
    }
    // `ndk_context` holds the Application Context, so load `SonoraActivity` via its
    // `ClassLoader` rather than `get_object_class` (which yields `android.app.Application`).
    let app = std::mem::ManuallyDrop::new(unsafe { JObject::from_raw(env, app_ptr) });
    let loader = env
        .call_method(
            &*app,
            jni_str!("getClassLoader"),
            jni_sig!(() -> java.lang.ClassLoader),
            &[],
        )?
        .l()?;
    let cls_name = env.new_string("io.github.sonorahq.sonora.SonoraActivity")?;
    let cls_obj = env
        .call_method(
            &loader,
            jni_str!("loadClass"),
            jni_sig!((java.lang.String) -> java.lang.Class),
            &[JValue::from(&cls_name)],
        )?
        .l()?;
    let cls = env.cast_local::<JClass>(cls_obj)?;
    let global = env.new_global_ref(cls)?;
    let _ = ACTIVITY_CLASS.set(global);
    Ok(ACTIVITY_CLASS.get().expect("activity class just set"))
}

fn ensure_registered(env: &mut Env<'_>, cls: &Global<JClass<'static>>) -> jni::errors::Result<()> {
    if REGISTERED.get().is_some() {
        return Ok(());
    }
    let _guard = INIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if REGISTERED.get().is_some() {
        return Ok(());
    }
    let methods = unsafe {
        [
            NativeMethod::from_raw_parts(
                jni_str!("nativeMediaCommand"),
                jni_str!("(Ljava/lang/String;)V"),
                native_media_command as *mut c_void,
            ),
            NativeMethod::from_raw_parts(
                jni_str!("nativeMediaLog"),
                jni_str!("(Ljava/lang/String;)V"),
                native_media_log as *mut c_void,
            ),
        ]
    };
    unsafe { env.register_native_methods(&**cls, &methods) }?;
    let _ = REGISTERED.set(());
    Ok(())
}

fn register_natives() -> jni::errors::Result<()> {
    let ctx = ndk_context::android_context();
    let vm_ptr = ctx.vm().cast::<jni::sys::JavaVM>();
    let app_ptr = ctx.context() as jni::sys::jobject;
    if vm_ptr.is_null() || app_ptr.is_null() {
        return Err(jni::errors::Error::NullPtr("ndk_context"));
    }
    let vm = unsafe { JavaVM::from_raw(vm_ptr) };
    vm.attach_current_thread(|env| -> jni::errors::Result<()> {
        let cls = activity_class(env, app_ptr)?;
        ensure_registered(env, cls)
    })
}

fn with_activity<F>(call: F)
where
    F: FnOnce(&mut Env<'_>, &JClass<'_>) -> jni::errors::Result<()>,
{
    let ctx = ndk_context::android_context();
    let vm_ptr = ctx.vm().cast::<jni::sys::JavaVM>();
    let app_ptr = ctx.context() as jni::sys::jobject;
    if vm_ptr.is_null() || app_ptr.is_null() {
        log::warn!("remote: android context unavailable");
        return;
    }
    let vm = unsafe { JavaVM::from_raw(vm_ptr) };
    if let Err(error) = vm.attach_current_thread(|env| -> jni::errors::Result<()> {
        let cls = activity_class(env, app_ptr)?;
        ensure_registered(env, cls)?;
        call(env, cls)
    }) {
        log::warn!("remote: android jni call failed: {error:#}");
    }
}

extern "system" fn native_media_command<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
    cmd: JString<'local>,
) {
    let _ = unowned_env
        .with_env(|env| -> jni::errors::Result<()> {
            let text = cmd.try_to_string(env)?;
            if let Some(command) = parse_command(&text)
                && let Some(sender) = COMMANDS.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
            {
                let _ = sender.send(command);
            }
            Ok(())
        })
        .resolve::<jni::errors::LogErrorAndDefault>();
}

extern "system" fn native_media_log<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
    msg: JString<'local>,
) {
    let _ = unowned_env
        .with_env(|env| -> jni::errors::Result<()> {
            let text = msg.try_to_string(env)?;
            log::info!("remote: {text}");
            Ok(())
        })
        .resolve::<jni::errors::LogErrorAndDefault>();
}

fn parse_command(text: &str) -> Option<Command> {
    if let Some(ms) = text.strip_prefix("SEEK:")
        && let Ok(ms) = ms.parse::<u64>()
    {
        return Some(Command::Seek(Duration::from_millis(ms)));
    }
    match text {
        "PLAY" => Some(Command::Play),
        "PAUSE" => Some(Command::Pause),
        "TOGGLE" => Some(Command::Toggle),
        "NEXT" => Some(Command::Next),
        "PREVIOUS" => Some(Command::Previous),
        _ => None,
    }
}
