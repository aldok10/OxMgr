use std::env;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Process-wide lock for tests that mutate the environment.
///
/// In edition 2024 `std::env::set_var` and `std::env::remove_var` become
/// `unsafe` and require that no other thread reads or writes the process
/// environment concurrently. This mutex is that guarantee: every test that
/// touches the environment locks it before doing so.
pub fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// RAII guard that sets an environment variable and restores its prior value
/// (including absence) when dropped.
///
/// # Safety
///
/// Holds the shared `env_lock()` to ensure no concurrent environment
/// access.
pub struct EnvGuard {
    key: String,
    previous: Option<String>,
    /// Prior values of variables set or removed via `also_set`/`also_remove`,
    /// restored on drop in reverse order.
    also_previous: Vec<(String, Option<String>)>,
    _lock: MutexGuard<'static, ()>,
}

impl EnvGuard {
    /// Save the current value of `key`, set it to `value`, and return a guard
    /// that restores the original value on drop.
    pub fn set(key: &str, value: &str) -> Self {
        let lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let previous = env::var(key).ok();
        // SAFETY: The env_lock mutex is held, serializing env access.
        unsafe { env::set_var(key, value) };
        Self {
            key: key.to_string(),
            previous,
            also_previous: Vec::new(),
            _lock: lock,
        }
    }

    /// Save the current value of `key`, remove it, and return a guard
    /// that restores the original value on drop.
    pub fn remove(key: &str) -> Self {
        let lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let previous = env::var(key).ok();
        // SAFETY: The env_lock mutex is held, serializing env access.
        unsafe { env::remove_var(key) };
        Self {
            key: key.to_string(),
            previous,
            also_previous: Vec::new(),
            _lock: lock,
        }
    }

    /// Set an additional variable while the lock is held, recording its prior
    /// value so it is restored on drop.
    pub fn also_set(&mut self, key: &str, value: &str) -> &mut Self {
        let previous = env::var(key).ok();
        // SAFETY: The env_lock mutex is held.
        unsafe { env::set_var(key, value) };
        self.also_previous.push((key.to_string(), previous));
        self
    }

    /// Remove an additional variable while the lock is held, recording its
    /// prior value so it is restored on drop.
    pub fn also_remove(&mut self, key: &str) -> &mut Self {
        let previous = env::var(key).ok();
        // SAFETY: The env_lock mutex is held.
        unsafe { env::remove_var(key) };
        self.also_previous.push((key.to_string(), previous));
        self
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // Restore the additional variables first, newest last-set first, then
        // the primary key.
        // SAFETY: The env_lock mutex is held (dropped after this).
        unsafe {
            for (key, previous) in self.also_previous.drain(..).rev() {
                match previous {
                    Some(v) => env::set_var(&key, v),
                    None => env::remove_var(&key),
                }
            }
            match &self.previous {
                Some(v) => env::set_var(&self.key, v),
                None => env::remove_var(&self.key),
            }
        }
    }
}
