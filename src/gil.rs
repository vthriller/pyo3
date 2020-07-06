// Copyright (c) 2017-present PyO3 Project and Contributors

//! Interaction with python's global interpreter lock

use crate::{ffi, internal_tricks::Unsendable, Python};
use parking_lot::{const_mutex, Mutex};
use std::cell::{Cell, RefCell};
use std::{mem::ManuallyDrop, ptr::NonNull, sync};

static START: sync::Once = sync::Once::new();

thread_local! {
    /// This is a internal counter in pyo3 monitoring whether this thread has the GIL.
    ///
    /// It will be incremented whenever a GILPool is created, and decremented whenever they are
    /// dropped.
    ///
    /// As a result, if this thread has the GIL, GIL_COUNT is greater than zero.
    ///
    /// pub(crate) because it is manipulated temporarily by Python::allow_threads
    pub(crate) static GIL_COUNT: Cell<u32> = Cell::new(0);

    /// Temporally hold objects that will be released when the GILPool drops.
    static OWNED_OBJECTS: RefCell<Vec<NonNull<ffi::PyObject>>> = RefCell::new(Vec::with_capacity(256));
}

/// Check whether the GIL is acquired.
///
/// Note: This uses pyo3's internal count rather than PyGILState_Check for two reasons:
///  1) for performance
///  2) PyGILState_Check always returns 1 if the sub-interpreter APIs have ever been called,
///     which could lead to incorrect conclusions that the GIL is held.
fn gil_is_acquired() -> bool {
    GIL_COUNT.with(|c| c.get() > 0)
}

/// Prepares the use of Python in a free-threaded context.
///
/// If the Python interpreter is not already initialized, this function
/// will initialize it with disabled signal handling
/// (Python will not raise the `KeyboardInterrupt` exception).
/// Python signal handling depends on the notion of a 'main thread', which must be
/// the thread that initializes the Python interpreter.
///
/// If both the Python interpreter and Python threading are already initialized,
/// this function has no effect.
///
/// # Panic
/// If the Python interpreter is initialized but Python threading is not,
/// a panic occurs.
/// It is not possible to safely access the Python runtime unless the main
/// thread (the thread which originally initialized Python) also initializes
/// threading.
///
/// When writing an extension module, the `#[pymodule]` macro
/// will ensure that Python threading is initialized.
///
pub fn prepare_freethreaded_python() {
    // Protect against race conditions when Python is not yet initialized
    // and multiple threads concurrently call 'prepare_freethreaded_python()'.
    // Note that we do not protect against concurrent initialization of the Python runtime
    // by other users of the Python C API.
    START.call_once(|| unsafe {
        if ffi::Py_IsInitialized() != 0 {
            // If Python is already initialized, we expect Python threading to also be initialized,
            // as we can't make the existing Python main thread acquire the GIL.
            #[cfg(not(Py_3_7))]
            assert_ne!(ffi::PyEval_ThreadsInitialized(), 0);
        } else {
            // If Python isn't initialized yet, we expect that Python threading
            // isn't initialized either.
            #[cfg(not(Py_3_7))]
            assert_eq!(ffi::PyEval_ThreadsInitialized(), 0);
            // Initialize Python.
            // We use Py_InitializeEx() with initsigs=0 to disable Python signal handling.
            // Signal handling depends on the notion of a 'main thread', which doesn't exist in this case.
            // Note that the 'main thread' notion in Python isn't documented properly;
            // and running Python without one is not officially supported.

            // PyPy does not support the embedding API
            #[cfg(not(PyPy))]
            {
                ffi::Py_InitializeEx(0);

                // Make sure Py_Finalize will be called before exiting.
                extern "C" fn finalize() {
                    unsafe {
                        if ffi::Py_IsInitialized() != 0 {
                            ffi::PyGILState_Ensure();
                            ffi::Py_Finalize();
                        }
                    }
                }
                libc::atexit(finalize);
            }

            // > Changed in version 3.7: This function is now called by Py_Initialize(), so you don’t have
            // > to call it yourself anymore.
            #[cfg(not(Py_3_7))]
            ffi::PyEval_InitThreads();
            // PyEval_InitThreads() will acquire the GIL,
            // but we don't want to hold it at this point
            // (it's not acquired in the other code paths)
            // So immediately release the GIL:
            #[cfg(not(PyPy))]
            let _thread_state = ffi::PyEval_SaveThread();
            // Note that the PyThreadState returned by PyEval_SaveThread is also held in TLS by the Python runtime,
            // and will be restored by PyGILState_Ensure.
        }
    });
}

/// RAII type that represents the Global Interpreter Lock acquisition.
///
/// # Example
/// ```
/// use pyo3::Python;
///
/// {
///     let gil_guard = Python::acquire_gil();
///     let py = gil_guard.python();
/// } // GIL is released when gil_guard is dropped
/// ```
#[must_use]
pub struct GILGuard {
    gstate: ffi::PyGILState_STATE,
    pool: ManuallyDrop<Option<GILPool>>,
}

impl GILGuard {
    /// Acquires the global interpreter lock, which allows access to the Python runtime. This is
    /// safe to call multiple times without causing a deadlock.
    ///
    /// If the Python runtime is not already initialized, this function will initialize it.
    /// See [prepare_freethreaded_python()](fn.prepare_freethreaded_python.html) for details.
    ///
    /// If PyO3 does not yet have a `GILPool` for tracking owned PyObject references, then this
    /// new `GILGuard` will also contain a `GILPool`.
    pub fn acquire() -> GILGuard {
        prepare_freethreaded_python();

        unsafe {
            let gstate = ffi::PyGILState_Ensure(); // acquire GIL

            // If there's already a GILPool, we should not create another or this could lead to
            // incorrect dangling references in safe code (see #864).
            let pool = if !gil_is_acquired() {
                Some(GILPool::new())
            } else {
                None
            };

            GILGuard {
                gstate,
                pool: ManuallyDrop::new(pool),
            }
        }
    }

    /// Retrieves the marker type that proves that the GIL was acquired.
    #[inline]
    pub fn python(&self) -> Python {
        unsafe { Python::assume_gil_acquired() }
    }
}

/// The Drop implementation for `GILGuard` will release the GIL.
impl Drop for GILGuard {
    fn drop(&mut self) {
        unsafe {
            // Must drop the objects in the pool before releasing the GILGuard
            ManuallyDrop::drop(&mut self.pool);
            ffi::PyGILState_Release(self.gstate);
        }
    }
}

/// Thread-safe storage for objects which were inc_ref / dec_ref while the GIL was not held.
struct ReferencePool {
    pointers_to_incref: Mutex<Vec<NonNull<ffi::PyObject>>>,
    pointers_to_decref: Mutex<Vec<NonNull<ffi::PyObject>>>,
}

impl ReferencePool {
    const fn new() -> Self {
        Self {
            pointers_to_incref: const_mutex(Vec::new()),
            pointers_to_decref: const_mutex(Vec::new()),
        }
    }

    fn register_incref(&self, obj: NonNull<ffi::PyObject>) {
        self.pointers_to_incref.lock().push(obj)
    }

    fn register_decref(&self, obj: NonNull<ffi::PyObject>) {
        self.pointers_to_decref.lock().push(obj)
    }

    fn update_counts(&self, _py: Python) {
        macro_rules! swap_vec_with_lock {
            // Get vec from one of ReferencePool's mutexes via lock, swap vec if needed, unlock.
            ($cell:expr) => {{
                let mut locked = $cell.lock();
                let mut out = Vec::new();
                if !locked.is_empty() {
                    std::mem::swap(&mut out, &mut *locked);
                }
                drop(locked);
                out
            }};
        };

        // Always increase reference counts first - as otherwise objects which have a
        // nonzero total reference count might be incorrectly dropped by Python during
        // this update.
        for ptr in swap_vec_with_lock!(self.pointers_to_incref) {
            unsafe { ffi::Py_INCREF(ptr.as_ptr()) };
        }

        for ptr in swap_vec_with_lock!(self.pointers_to_decref) {
            unsafe { ffi::Py_DECREF(ptr.as_ptr()) };
        }
    }
}

unsafe impl Sync for ReferencePool {}

static POOL: ReferencePool = ReferencePool::new();

/// A RAII pool which PyO3 uses to store owned Python references.
pub struct GILPool {
    /// Initial length of owned objects and anys.
    /// `Option` is used since TSL can be broken when `new` is called from `atexit`.
    start: Option<usize>,
    no_send: Unsendable,
}

impl GILPool {
    /// Create a new `GILPool`. This function should only ever be called with the GIL.
    ///
    /// It is recommended not to use this API directly, but instead to use `Python::new_pool`, as
    /// that guarantees the GIL is held.
    ///
    /// # Safety
    /// As well as requiring the GIL, see the notes on `Python::new_pool`.
    #[inline]
    pub unsafe fn new() -> GILPool {
        increment_gil_count();
        // Update counts of PyObjects / Py that have been cloned or dropped since last acquisition
        POOL.update_counts(Python::assume_gil_acquired());
        GILPool {
            start: OWNED_OBJECTS.try_with(|o| o.borrow().len()).ok(),
            no_send: Unsendable::default(),
        }
    }

    /// Get the Python token associated with this `GILPool`.
    pub fn python(&self) -> Python {
        unsafe { Python::assume_gil_acquired() }
    }
}

impl Drop for GILPool {
    fn drop(&mut self) {
        unsafe {
            if let Some(obj_len_start) = self.start {
                let dropping_obj = OWNED_OBJECTS.with(|holder| {
                    // `holder` must be dropped before calling Py_DECREF, or Py_DECREF may call
                    // `GILPool::drop` recursively, resulting in invalid borrowing.
                    let mut holder = holder.borrow_mut();
                    if obj_len_start < holder.len() {
                        holder.split_off(obj_len_start)
                    } else {
                        Vec::new()
                    }
                });
                for obj in dropping_obj {
                    ffi::Py_DECREF(obj.as_ptr());
                }
            }
        }
        decrement_gil_count();
    }
}

/// Register a Python object pointer inside the release pool, to have reference count increased
/// next time the GIL is acquired in pyo3.
///
/// If the GIL is held, the reference count will be increased immediately instead of being queued
/// for later.
///
/// # Safety
/// The object must be an owned Python reference.
pub unsafe fn register_incref(obj: NonNull<ffi::PyObject>) {
    if gil_is_acquired() {
        ffi::Py_INCREF(obj.as_ptr())
    } else {
        POOL.register_incref(obj);
    }
}

/// Register a Python object pointer inside the release pool, to have reference count decreased
/// next time the GIL is acquired in pyo3.
///
/// If the GIL is held, the reference count will be decreased immediately instead of being queued
/// for later.
///
/// # Safety
/// The object must be an owned Python reference.
pub unsafe fn register_decref(obj: NonNull<ffi::PyObject>) {
    if gil_is_acquired() {
        ffi::Py_DECREF(obj.as_ptr())
    } else {
        POOL.register_decref(obj);
    }
}

/// Register an owned object inside the GILPool.
///
/// # Safety
/// The object must be an owned Python reference.
pub unsafe fn register_owned(_py: Python, obj: NonNull<ffi::PyObject>) {
    debug_assert!(gil_is_acquired());
    // Ignore the error since we should do nothing when the TLS is broken,
    let _ = OWNED_OBJECTS.try_with(|holder| holder.borrow_mut().push(obj));
}

/// Increment pyo3's internal GIL count - to be called whenever GILPool or GILGuard is created.
// Ignores the error in case this function called from `atexit`.
#[inline(always)]
fn increment_gil_count() {
    let _ = GIL_COUNT.with(|c| c.set(c.get() + 1));
}

/// Decrement pyo3's internal GIL count - to be called whenever GILPool or GILGuard is dropped.
// Ignores the error in case this function called from `atexit`.
#[inline(always)]
fn decrement_gil_count() {
    let _ = GIL_COUNT.try_with(|c| {
        let current = c.get();
        debug_assert!(
            current > 0,
            "Negative GIL count detected. Please report this error to the PyO3 repo as a bug."
        );
        c.set(current - 1);
    });
}

/// Ensure the GIL is held, useful in implementation of APIs like PyErr::new where it's
/// inconvenient to force the user to acquire the GIL.
#[doc(hidden)]
pub fn ensure_gil() -> EnsureGIL {
    if gil_is_acquired() {
        EnsureGIL(None)
    } else {
        EnsureGIL(Some(GILGuard::acquire()))
    }
}

/// Struct used internally which avoids acquiring the GIL where it's not necessary.
#[doc(hidden)]
pub struct EnsureGIL(Option<GILGuard>);

impl EnsureGIL {
    /// Get the GIL token.
    ///
    /// # Safety
    /// If `self.0` is `None`, then this calls [Python::assume_gil_acquired].
    /// Thus this method could be used to get access to a GIL token while the GIL is not held.
    /// Care should be taken to only use the returned Python in contexts where it is certain the
    /// GIL continues to be held.
    pub unsafe fn python(&self) -> Python {
        match &self.0 {
            Some(gil) => gil.python(),
            None => Python::assume_gil_acquired(),
        }
    }
}

#[cfg(test)]
mod test {
    use super::{gil_is_acquired, GILPool, GIL_COUNT, OWNED_OBJECTS, POOL};
    use crate::{ffi, gil, AsPyPointer, IntoPyPointer, PyObject, Python, ToPyObject};
    use std::ptr::NonNull;

    fn get_object(py: Python) -> PyObject {
        // Convenience function for getting a single unique object, using `new_pool` so as to leave
        // the original pool state unchanged.
        let pool = unsafe { py.new_pool() };
        let py = pool.python();

        let obj = py.eval("object()", None, None).unwrap();
        obj.to_object(py)
    }

    fn owned_object_count() -> usize {
        OWNED_OBJECTS.with(|holder| holder.borrow().obj.len())
    }

    #[test]
    fn test_owned() {
        let gil = Python::acquire_gil();
        let py = gil.python();
        let obj = get_object(py);
        let obj_ptr = obj.as_ptr();
        // Ensure that obj does not get freed
        let _ref = obj.clone_ref(py);

        unsafe {
            {
                let pool = py.new_pool();
                gil::register_owned(pool.python(), NonNull::new_unchecked(obj.into_ptr()));

                assert_eq!(owned_object_count(), 1);
                assert_eq!(ffi::Py_REFCNT(obj_ptr), 2);
            }
            {
                let _pool = py.new_pool();
                assert_eq!(owned_object_count(), 0);
                assert_eq!(ffi::Py_REFCNT(obj_ptr), 1);
            }
        }
    }

    #[test]
    fn test_owned_nested() {
        let gil = Python::acquire_gil();
        let py = gil.python();
        let obj = get_object(py);
        // Ensure that obj does not get freed
        let _ref = obj.clone_ref(py);
        let obj_ptr = obj.as_ptr();

        unsafe {
            {
                let _pool = py.new_pool();
                assert_eq!(owned_object_count(), 0);

                gil::register_owned(py, NonNull::new_unchecked(obj.into_ptr()));

                assert_eq!(owned_object_count(), 1);
                assert_eq!(ffi::Py_REFCNT(obj_ptr), 2);
                {
                    let _pool = py.new_pool();
                    let obj = get_object(py);
                    gil::register_owned(py, NonNull::new_unchecked(obj.into_ptr()));
                    assert_eq!(owned_object_count(), 2);
                }
                assert_eq!(owned_object_count(), 1);
            }
            {
                assert_eq!(owned_object_count(), 0);
                assert_eq!(ffi::Py_REFCNT(obj_ptr), 1);
            }
        }
    }

    #[test]
    fn test_pyobject_drop_with_gil_decreases_refcnt() {
        let gil = Python::acquire_gil();
        let py = gil.python();
        let obj = get_object(py);
        // Ensure that obj does not get freed
        let _ref = obj.clone_ref(py);
        let obj_ptr = obj.as_ptr();

        unsafe {
            {
                assert_eq!(owned_object_count(), 0);
                assert_eq!(ffi::Py_REFCNT(obj_ptr), 2);
            }

            // With the GIL held, obj can be dropped immediately
            drop(obj);
            assert_eq!(ffi::Py_REFCNT(obj_ptr), 1);
        }
    }

    #[test]
    fn test_pyobject_drop_without_gil_doesnt_decrease_refcnt() {
        let gil = Python::acquire_gil();
        let py = gil.python();
        let obj = get_object(py);
        // Ensure that obj does not get freed
        let _ref = obj.clone_ref(py);
        let obj_ptr = obj.as_ptr();

        unsafe {
            {
                assert_eq!(owned_object_count(), 0);
                assert_eq!(ffi::Py_REFCNT(obj_ptr), 2);
            }

            // Without the GIL held, obj cannot be dropped until the next GIL acquire
            drop(gil);
            drop(obj);
            assert_eq!(ffi::Py_REFCNT(obj_ptr), 2);

            {
                // Next time the GIL is acquired, the object is released
                let _gil = Python::acquire_gil();
                assert_eq!(ffi::Py_REFCNT(obj_ptr), 1);
            }
        }
    }

    #[test]
    fn test_gil_counts() {
        // Check GILGuard and GILPool both increase counts correctly
        let get_gil_count = || GIL_COUNT.with(|c| c.get());

        assert_eq!(get_gil_count(), 0);
        let gil = Python::acquire_gil();
        assert_eq!(get_gil_count(), 1);

        assert_eq!(get_gil_count(), 1);
        let pool = unsafe { GILPool::new() };
        assert_eq!(get_gil_count(), 2);

        let pool2 = unsafe { GILPool::new() };
        assert_eq!(get_gil_count(), 3);

        drop(pool);
        assert_eq!(get_gil_count(), 2);

        // Creating a new GILGuard should not increment the gil count if a GILPool already exists
        let gil2 = Python::acquire_gil();
        assert_eq!(get_gil_count(), 2);

        drop(pool2);
        assert_eq!(get_gil_count(), 1);

        drop(gil2);
        assert_eq!(get_gil_count(), 1);

        drop(gil);
        assert_eq!(get_gil_count(), 0);
    }

    #[test]
    fn test_allow_threads() {
        // allow_threads should temporarily release GIL in Py03's internal tracking too.
        let gil = Python::acquire_gil();
        let py = gil.python();

        assert!(gil_is_acquired());

        py.allow_threads(move || {
            assert!(!gil_is_acquired());

            let gil = Python::acquire_gil();
            assert!(gil_is_acquired());

            drop(gil);
            assert!(!gil_is_acquired());
        });

        assert!(gil_is_acquired());
    }

    #[test]
    fn dropping_gil_does_not_invalidate_references() {
        // Acquiring GIL for the second time should be safe - see #864
        let gil = Python::acquire_gil();
        let py = gil.python();
        let obj;

        let gil2 = Python::acquire_gil();
        obj = py.eval("object()", None, None).unwrap();
        drop(gil2);

        // After gil2 drops, obj should still have a reference count of one
        assert_eq!(obj.get_refcnt(), 1);
    }

    #[test]
    fn test_clone_with_gil() {
        let gil = Python::acquire_gil();
        let py = gil.python();

        let obj = get_object(py);
        let count = obj.get_refcnt(py);

        // Cloning with the GIL should increase reference count immediately
        #[allow(clippy::redundant_clone)]
        let c = obj.clone();
        assert_eq!(count + 1, c.get_refcnt(py));
    }

    #[test]
    fn test_clone_without_gil() {
        let gil = Python::acquire_gil();
        let py = gil.python();
        let obj = get_object(py);
        let count = obj.get_refcnt(py);

        // Cloning without GIL should not update reference count
        drop(gil);
        let c = obj.clone();
        assert_eq!(
            count,
            obj.get_refcnt(unsafe { Python::assume_gil_acquired() })
        );

        // Acquring GIL will clear this pending change
        let gil = Python::acquire_gil();
        let py = gil.python();

        // Total reference count should be one higher
        assert_eq!(count + 1, obj.get_refcnt(py));

        // Clone dropped
        drop(c);

        // Overall count is now back to the original, and should be no pending change
        assert_eq!(count, obj.get_refcnt(py));
    }

    #[test]
    fn test_clone_in_other_thread() {
        let gil = Python::acquire_gil();
        let py = gil.python();
        let obj = get_object(py);
        let count = obj.get_refcnt(py);

        // Move obj to a thread which does not have the GIL, and clone it
        let t = std::thread::spawn(move || {
            // Cloning without GIL should not update reference count
            #[allow(clippy::redundant_clone)]
            let _ = obj.clone();
            assert_eq!(
                count,
                obj.get_refcnt(unsafe { Python::assume_gil_acquired() })
            );

            // Return obj so original thread can continue to use
            obj
        });

        let obj = t.join().unwrap();
        let ptr = NonNull::new(obj.as_ptr()).unwrap();

        // The pointer should appear once in the incref pool, and once in the
        // decref pool (for the clone being created and also dropped)
        assert_eq!(&*POOL.pointers_to_incref.lock(), &vec![ptr]);
        assert_eq!(&*POOL.pointers_to_decref.lock(), &vec![ptr]);

        // Re-acquring GIL will clear these pending changes
        drop(gil);
        let gil = Python::acquire_gil();

        assert!(POOL.pointers_to_incref.lock().is_empty());
        assert!(POOL.pointers_to_decref.lock().is_empty());

        // Overall count is still unchanged
        assert_eq!(count, obj.get_refcnt(gil.python()));
    }

    #[test]
    fn test_update_counts_does_not_deadlock() {
        // update_counts can run arbitrary Python code during Py_DECREF.
        // if the locking is implemented incorrectly, it will deadlock.

        let gil = Python::acquire_gil();
        let obj = get_object(gil.python());

        unsafe {
            unsafe extern "C" fn capsule_drop(capsule: *mut ffi::PyObject) {
                // This line will implicitly call update_counts
                // -> and so cause deadlock if update_counts is not handling recursion correctly.
                let pool = GILPool::new();

                // Rebuild obj so that it can be dropped
                PyObject::from_owned_ptr(
                    pool.python(),
                    ffi::PyCapsule_GetPointer(capsule, std::ptr::null()) as _,
                );
            }

            let ptr = obj.into_ptr();
            let capsule = ffi::PyCapsule_New(ptr as _, std::ptr::null(), Some(capsule_drop));

            POOL.register_decref(NonNull::new(capsule).unwrap());

            // Updating the counts will call decref on the capsule, which calls capsule_drop
            POOL.update_counts(gil.python())
        }
    }
}
