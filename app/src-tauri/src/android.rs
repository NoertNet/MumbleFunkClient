//! Bridge from Rust to the Android foreground service (status notification).
//!
//! Calls the Kotlin object `net.noert.mumblefunk.FunkBridge` through JNI. The class is looked up
//! via the activity's class loader because native threads only see the system class loader.
//! On every other platform this is a no-op.

#[cfg(target_os = "android")]
pub fn update(status: &str, connected: bool) {
    use jni::JavaVM;
    use jni::objects::{JClass, JObject, JValue};

    let result = (|| -> Result<(), jni::errors::Error> {
        let ctx = ndk_context::android_context();
        // SAFETY: the pointers come from ndk-context, which Tauri/tao initialise with the live
        // JavaVM and the activity before any of our code runs.
        let vm = unsafe { JavaVM::from_raw(ctx.vm().cast())? };
        let mut env = vm.attach_current_thread()?;
        let activity = unsafe { JObject::from_raw(ctx.context().cast()) };
        let loader = env.call_method(&activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?.l()?;
        let name = env.new_string("net.noert.mumblefunk.FunkBridge")?;
        let class = env
            .call_method(&loader, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", &[JValue::Object(&name)])?
            .l()?;
        let text = env.new_string(status)?;
        env.call_static_method(
            JClass::from(class),
            "update",
            "(Landroid/content/Context;Ljava/lang/String;Z)V",
            &[JValue::Object(&activity), JValue::Object(&text), JValue::Bool(connected as u8)],
        )?;
        Ok(())
    })();
    if let Err(e) = result {
        tracing::warn!("android status update failed: {e}");
    }
}

#[cfg(not(target_os = "android"))]
pub fn update(_status: &str, _connected: bool) {}
