//! Bridge from Rust to the Android side (`FunkBridge` in Kotlin) through JNI: status notification,
//! "is the app in the background?", diagnostics, and the battery-optimisation dialog.
//!
//! The class is looked up via the activity's class loader because native threads only see the system
//! class loader. Every call is best effort: a failure is logged and never disturbs the radio. On every
//! other platform all functions are no-ops.

#[cfg(target_os = "android")]
mod imp {
    use jni::errors::Error as JniError;
    use jni::objects::{JClass, JObject, JString, JValue};
    use jni::{JNIEnv, JavaVM};

    /// Runs `f` with the JNI environment, the activity and the `FunkBridge` class.
    fn bridge<R>(f: impl FnOnce(&mut JNIEnv, &JObject, JClass) -> Result<R, JniError>) -> Option<R> {
        let ctx = ndk_context::android_context();
        // SAFETY: the pointers come from ndk-context, which Tauri/tao initialise with the live
        // JavaVM and the activity before any of our code runs.
        let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
        let mut env = vm.attach_current_thread().ok()?;
        let activity = unsafe { JObject::from_raw(ctx.context().cast()) };
        let result = (|| {
            let loader = env.call_method(&activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?.l()?;
            let name = env.new_string("net.noert.mumblefunk.FunkBridge")?;
            let class = env
                .call_method(&loader, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", &[JValue::Object(&name)])?
                .l()?;
            f(&mut env, &activity, JClass::from(class))
        })();
        match result {
            Ok(v) => Some(v),
            Err(e) => {
                let _ = env.exception_clear(); // a pending Java exception would break later JNI calls
                tracing::warn!("android bridge call failed: {e}");
                None
            }
        }
    }

    pub fn update(status: &str, connected: bool) {
        bridge(|env, activity, class| {
            let text = env.new_string(status)?;
            env.call_static_method(
                class,
                "update",
                "(Landroid/content/Context;Ljava/lang/String;Z)V",
                &[JValue::Object(activity), JValue::Object(&text), JValue::Bool(connected as u8)],
            )?;
            Ok(())
        });
    }

    /// True while the app's window is not visible. Unknown or failing answers mean "foreground": a
    /// missing signal must never make the radio less responsive.
    pub fn is_background() -> bool {
        bridge(|env, _, class| Ok(env.call_static_method(class, "isBackground", "()Z", &[])?.z()?)).unwrap_or(false)
    }

    /// Appends a line to the diagnostic log kept by the Kotlin side.
    pub fn log(msg: &str) {
        bridge(|env, activity, class| {
            let text = env.new_string(msg)?;
            env.call_static_method(
                class,
                "log",
                "(Landroid/content/Context;Ljava/lang/String;)V",
                &[JValue::Object(activity), JValue::Object(&text)],
            )?;
            Ok(())
        });
    }

    pub fn report() -> Option<String> {
        bridge(|env, activity, class| {
            let obj = env
                .call_static_method(class, "report", "(Landroid/content/Context;)Ljava/lang/String;", &[JValue::Object(activity)])?
                .l()?;
            let s: String = env.get_string(&JString::from(obj))?.into();
            Ok(s)
        })
    }

    /// Opens Android's battery-optimisation dialog for this app. Returns false if it could not be opened.
    pub fn request_battery_exemption() -> bool {
        bridge(|env, activity, class| {
            Ok(env
                .call_static_method(class, "requestBatteryExemption", "(Landroid/app/Activity;)Z", &[JValue::Object(activity)])?
                .z()?)
        })
        .unwrap_or(false)
    }
}

#[cfg(not(target_os = "android"))]
mod imp {
    pub fn update(_status: &str, _connected: bool) {}
    pub fn is_background() -> bool {
        false
    }
    pub fn log(_msg: &str) {}
    pub fn report() -> Option<String> {
        None
    }
    pub fn request_battery_exemption() -> bool {
        false
    }
}

pub use imp::{is_background, log, report, request_battery_exemption, update};
