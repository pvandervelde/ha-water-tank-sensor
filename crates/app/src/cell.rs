// Based on code from here: https://github.com/claudiomattera/esp32c3-embassy/

//! An `UnsafeCell` that implements `Sync`
//!
//! This is a placeholder until `core::cell::SyncUnsafeCell` is stabilized.

use core::cell::UnsafeCell;

/// An `UnsafeCell` that implements `Sync`
#[expect(clippy::module_name_repetitions, reason = "Allow repeating the name")]
pub struct SyncUnsafeCell<T> {
    /// The inner cell
    inner: UnsafeCell<T>,
}

impl<T> SyncUnsafeCell<T> {
    /// Create a new cell
    #[must_use]
    pub const fn new(inner: T) -> Self {
        Self {
            inner: UnsafeCell::new(inner),
        }
    }

    /// Get a mutable pointer to the wrapped value
    pub fn get(&self) -> *mut T {
        self.inner.get()
    }
}

// SAFETY:
// There is only one thread on a ESP32-C3.
unsafe impl<T: Sync> Sync for SyncUnsafeCell<T> {}
