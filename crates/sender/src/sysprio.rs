//! System-priority helpers.
//!
//! VTCast's capture + encode workers run flat-out at the target frame rate.
//! On their own that's fine, but during a live broadcast they share the
//! machine with a game, Warudo, OBS (its own NVENC session + compositing) and
//! — critically — an audio-interface driver feeding the streamer's mic. With
//! no priority hints, our threads compete as equals with the audio path, and
//! under full CPU/GPU saturation that can starve the audio engine: the
//! symptom is mic crackle / dropouts that only appear once the broadcast (and
//! its extra load) is running.
//!
//! Lowering our scheduling priority costs nothing in output quality. The
//! pipeline still hits its frame rate — it sleeps between frames and has
//! ample headroom — it just stops elbowing latency-sensitive work (audio,
//! the desktop compositor) out of the way when the system is contended.
//!
//! Two knobs, both deprioritising VTCast relative to everything else:
//!   * CPU: per-worker-thread `THREAD_PRIORITY_BELOW_NORMAL`.
//!   * GPU: the D3D11 device's DXGI GPU-thread priority, set negative so our
//!     submissions queue behind the game / OBS / compositor.

/// Drop the calling thread to `BELOW_NORMAL` so audio + UI threads always
/// preempt it under contention. Best-effort: a failure is logged at debug and
/// otherwise ignored (we'd rather keep running at normal priority than abort).
#[cfg(windows)]
pub fn lower_current_thread_priority(label: &str) {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
    };
    unsafe {
        match SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) {
            Ok(()) => tracing::debug!(label, "worker thread lowered to BELOW_NORMAL"),
            Err(e) => tracing::debug!(label, error = ?e, "SetThreadPriority(BELOW_NORMAL) failed"),
        }
    }
}

#[cfg(not(windows))]
pub fn lower_current_thread_priority(_label: &str) {}

/// Lower the GPU scheduling priority of `device` so its work yields to the
/// game / OBS / desktop compositor on the same GPU. Range is -7..=7; -4 is a
/// firm deprioritisation that still leaves us well clear of the -7 floor.
/// Same value range as ffmpeg's `gpu_priority` / DXVA tuning. Best-effort.
#[cfg(windows)]
pub fn lower_gpu_priority(device: &windows::Win32::Graphics::Direct3D11::ID3D11Device) {
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::IDXGIDevice;

    match device.cast::<IDXGIDevice>() {
        Ok(dxgi) => unsafe {
            match dxgi.SetGPUThreadPriority(-4) {
                Ok(()) => tracing::debug!("GPU thread priority lowered to -4"),
                Err(e) => tracing::debug!(error = ?e, "SetGPUThreadPriority(-4) failed"),
            }
        },
        Err(e) => tracing::debug!(error = ?e, "device.cast::<IDXGIDevice>() failed"),
    }
}
