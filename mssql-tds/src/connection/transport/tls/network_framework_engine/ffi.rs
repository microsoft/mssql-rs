// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bindings for the slice of Network.framework, the TLS half of Security.framework
//! and libdispatch that the engine uses.
//!
//! All of it is C API whose callbacks are Objective-C blocks, built with `block2`.
//! Everything used here exists since macOS 10.14, except the protocol-version and
//! cipher-suite metadata getters (10.15).

use std::ffi::{c_char, c_void};
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::time::Duration;

use block2::Block;
use security_framework_sys::trust::SecTrustRef;

pub(super) type Object = *mut c_void;

/// C99 `bool` in a block signature. `block2` does not encode `bool`, but a block
/// argument of this type is passed exactly as a `u8` that is 0 or 1.
pub(super) type BlockBool = u8;

pub(super) type StateHandler = Block<dyn Fn(u32, Object)>;
pub(super) type ReceiveCompletion = Block<dyn Fn(Object, Object, BlockBool, Object)>;
pub(super) type SendCompletion = Block<dyn Fn(Object)>;
pub(super) type ConfigureProtocol = Block<dyn Fn(Object)>;
pub(super) type VerifyComplete = Block<dyn Fn(BlockBool)>;
pub(super) type VerifyHandler = Block<dyn Fn(Object, Object, *mut VerifyComplete)>;
pub(super) type Task = Block<dyn Fn()>;

pub(super) const STATE_WAITING: u32 = 1;
pub(super) const STATE_READY: u32 = 3;
pub(super) const STATE_FAILED: u32 = 4;
pub(super) const STATE_CANCELLED: u32 = 5;

pub(super) const ERROR_DOMAIN_POSIX: u32 = 1;
pub(super) const ERROR_DOMAIN_TLS: u32 = 3;

pub(super) const TLS_1_2: u16 = 0x0303;
pub(super) const TLS_1_3: u16 = 0x0304;

/// `QOS_CLASS_USER_INITIATED`.
const QOS_USER_INITIATED: isize = 0x19;

const DISPATCH_TIME_NOW: u64 = 0;

#[link(name = "Network", kind = "framework")]
unsafe extern "C" {
    static _nw_content_context_default_stream: Object;

    fn nw_retain(object: Object) -> Object;
    fn nw_release(object: Object);

    pub(super) fn nw_endpoint_create_address(address: *const c_void) -> Object;

    pub(super) fn nw_parameters_create_secure_tcp(
        configure_tls: &ConfigureProtocol,
        configure_tcp: &ConfigureProtocol,
    ) -> Object;
    pub(super) fn nw_parameters_set_prefer_no_proxy(parameters: Object, prefer_no_proxy: bool);
    pub(super) fn nw_tcp_options_set_no_delay(options: Object, no_delay: bool);
    pub(super) fn nw_tcp_options_set_enable_keepalive(options: Object, enable_keepalive: bool);
    pub(super) fn nw_tcp_options_set_keepalive_idle_time(options: Object, seconds: u32);
    pub(super) fn nw_tcp_options_set_keepalive_interval(options: Object, seconds: u32);
    pub(super) fn nw_tls_copy_sec_protocol_options(options: Object) -> Object;
    pub(super) fn nw_tls_copy_sec_protocol_metadata(metadata: Object) -> Object;
    pub(super) fn nw_protocol_copy_tls_definition() -> Object;

    pub(super) fn nw_connection_create(endpoint: Object, parameters: Object) -> Object;
    pub(super) fn nw_connection_set_queue(connection: Object, queue: Object);
    pub(super) fn nw_connection_set_state_changed_handler(
        connection: Object,
        handler: &StateHandler,
    );
    pub(super) fn nw_connection_start(connection: Object);
    pub(super) fn nw_connection_cancel(connection: Object);
    pub(super) fn nw_connection_receive(
        connection: Object,
        minimum_incomplete_length: u32,
        maximum_length: u32,
        completion: &ReceiveCompletion,
    );
    pub(super) fn nw_connection_send(
        connection: Object,
        content: Object,
        context: Object,
        is_complete: bool,
        completion: &SendCompletion,
    );
    pub(super) fn nw_connection_copy_protocol_metadata(
        connection: Object,
        definition: Object,
    ) -> Object;

    pub(super) fn nw_error_get_error_domain(error: Object) -> u32;
    pub(super) fn nw_error_get_error_code(error: Object) -> i32;
    pub(super) fn nw_error_copy_cf_error(error: Object) -> *mut c_void;
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn sec_retain(object: Object) -> Object;
    fn sec_release(object: Object);

    pub(super) fn sec_protocol_options_set_tls_server_name(
        options: Object,
        server_name: *const c_char,
    );
    pub(super) fn sec_protocol_options_add_tls_application_protocol(
        options: Object,
        protocol: *const c_char,
    );
    pub(super) fn sec_protocol_options_set_min_tls_protocol_version(options: Object, version: u16);
    pub(super) fn sec_protocol_options_set_max_tls_protocol_version(options: Object, version: u16);
    pub(super) fn sec_protocol_options_set_tls_resumption_enabled(options: Object, enabled: bool);
    pub(super) fn sec_protocol_options_set_tls_tickets_enabled(options: Object, enabled: bool);
    pub(super) fn sec_protocol_options_set_verify_block(
        options: Object,
        handler: &VerifyHandler,
        queue: Object,
    );

    pub(super) fn sec_protocol_metadata_get_negotiated_tls_protocol_version(
        metadata: Object,
    ) -> u16;
    pub(super) fn sec_protocol_metadata_get_negotiated_tls_ciphersuite(metadata: Object) -> u16;
    // macOS 15.5 replaced this with `sec_protocol_metadata_copy_negotiated_protocol`,
    // which older releases lack, so the deprecated getter is the one that works everywhere.
    pub(super) fn sec_protocol_metadata_get_negotiated_protocol(metadata: Object) -> *const c_char;

    pub(super) fn sec_trust_copy_ref(trust: Object) -> SecTrustRef;
}

unsafe extern "C" {
    fn dispatch_retain(object: Object);
    fn dispatch_release(object: Object);

    fn dispatch_queue_create(label: *const c_char, attr: Object) -> Object;
    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> Object;
    fn dispatch_time(when: u64, delta: i64) -> u64;
    fn dispatch_after(when: u64, queue: Object, block: &Task);
    fn dispatch_data_create(
        buffer: *const c_void,
        size: usize,
        queue: Object,
        destructor: Object,
    ) -> Object;
    fn dispatch_data_create_map(
        data: Object,
        buffer_ptr: *mut *const c_void,
        size_ptr: *mut usize,
    ) -> Object;
}

/// Reference-counting flavor of a Network.framework, Security.framework or
/// libdispatch object.
///
/// # Safety
///
/// `retain` and `release` must be the matching pair for objects of this family,
/// callable from any thread.
pub(super) unsafe trait Family {
    /// # Safety
    ///
    /// `object` is a live object of this family.
    unsafe fn retain(object: Object);

    /// # Safety
    ///
    /// The caller owns a reference to `object` and gives it up.
    unsafe fn release(object: Object);
}

pub(super) struct Nw;
pub(super) struct Sec;
pub(super) struct Dispatch;

// SAFETY: `nw_retain`/`nw_release` are atomic and thread-safe.
unsafe impl Family for Nw {
    unsafe fn retain(object: Object) {
        unsafe { nw_retain(object) };
    }

    unsafe fn release(object: Object) {
        unsafe { nw_release(object) };
    }
}

// SAFETY: `sec_retain`/`sec_release` are atomic and thread-safe.
unsafe impl Family for Sec {
    unsafe fn retain(object: Object) {
        unsafe { sec_retain(object) };
    }

    unsafe fn release(object: Object) {
        unsafe { sec_release(object) };
    }
}

// SAFETY: `dispatch_retain`/`dispatch_release` are atomic and thread-safe.
unsafe impl Family for Dispatch {
    unsafe fn retain(object: Object) {
        unsafe { dispatch_retain(object) };
    }

    unsafe fn release(object: Object) {
        unsafe { dispatch_release(object) };
    }
}

/// An owned reference to a framework object, released on drop.
pub(super) struct Handle<F: Family> {
    object: NonNull<c_void>,
    family: PhantomData<F>,
}

// SAFETY: these objects are reference counted atomically and the framework entry
// points the engine calls on them may be called from any thread.
unsafe impl<F: Family> Send for Handle<F> {}
unsafe impl<F: Family> Sync for Handle<F> {}

impl<F: Family> Handle<F> {
    /// Takes over a +1 reference, as returned by `*_create` and `*_copy_*`.
    ///
    /// # Safety
    ///
    /// `object` is null or a live object of family `F` the caller owns a reference to.
    pub(super) unsafe fn from_owned(object: Object) -> Option<Self> {
        NonNull::new(object).map(|object| Self {
            object,
            family: PhantomData,
        })
    }

    /// Retains a borrowed reference, such as a block argument.
    ///
    /// # Safety
    ///
    /// `object` is null or a live object of family `F`.
    pub(super) unsafe fn from_borrowed(object: Object) -> Option<Self> {
        let object = NonNull::new(object)?;
        // SAFETY: per the contract.
        unsafe { F::retain(object.as_ptr()) };
        Some(Self {
            object,
            family: PhantomData,
        })
    }

    pub(super) fn as_ptr(&self) -> Object {
        self.object.as_ptr()
    }
}

impl<F: Family> Clone for Handle<F> {
    fn clone(&self) -> Self {
        // SAFETY: `self` keeps the object alive.
        unsafe { F::retain(self.as_ptr()) };
        Self {
            object: self.object,
            family: PhantomData,
        }
    }
}

impl<F: Family> Drop for Handle<F> {
    fn drop(&mut self) {
        // SAFETY: `self` owns one reference.
        unsafe { F::release(self.as_ptr()) };
    }
}

/// The context all stream data is sent on; sending with `is_complete` set on it
/// closes the write side.
pub(super) fn stream_context() -> Object {
    // SAFETY: an immutable pointer the framework initializes at load time.
    unsafe { _nw_content_context_default_stream }
}

/// A new serial queue, so that one connection's callbacks never wait on another's.
pub(super) fn new_serial_queue() -> Option<Handle<Dispatch>> {
    // SAFETY: a null attribute makes the queue serial; the result is owned (+1).
    unsafe {
        Handle::from_owned(dispatch_queue_create(
            c"mssql-tds.tls".as_ptr(),
            std::ptr::null_mut(),
        ))
    }
}

/// A global queue for work that can block, such as evaluating a certificate chain
/// that may need the network.
pub(super) fn verification_queue() -> Option<Handle<Dispatch>> {
    // SAFETY: global queues are immortal, so retaining one is harmless.
    unsafe { Handle::from_borrowed(dispatch_get_global_queue(QOS_USER_INITIATED, 0)) }
}

/// Runs `task` on `queue` once `delay` has passed.
pub(super) fn run_after(delay: Duration, queue: &Handle<Dispatch>, task: &Task) {
    let delta = i64::try_from(delay.as_nanos()).unwrap_or(i64::MAX);
    // SAFETY: the queue is live, and the call retains it and copies the block.
    unsafe {
        let when = dispatch_time(DISPATCH_TIME_NOW, delta);
        dispatch_after(when, queue.as_ptr(), task);
    }
}

/// Copies `bytes` into a dispatch data object the framework can hold on to.
pub(super) fn copy_to_dispatch_data(bytes: &[u8]) -> Option<Handle<Dispatch>> {
    // SAFETY: a null queue and destructor make the call copy `bytes` before returning.
    unsafe {
        Handle::from_owned(dispatch_data_create(
            bytes.as_ptr().cast(),
            bytes.len(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ))
    }
}

/// A contiguous view of delivered dispatch data.
///
/// Delivered data may consist of several regions. Mapping makes them contiguous and
/// costs nothing when they already are.
pub(super) struct MappedData {
    // Owns the memory `ptr` points into.
    _owner: Handle<Dispatch>,
    ptr: *const u8,
    len: usize,
}

// SAFETY: the mapped bytes are immutable for as long as `_owner` lives.
unsafe impl Send for MappedData {}
unsafe impl Sync for MappedData {}

impl MappedData {
    /// Maps `data`, returning `None` if it is null or empty.
    ///
    /// # Safety
    ///
    /// `data` is null or a live dispatch data object.
    pub(super) unsafe fn new(data: Object) -> Option<Self> {
        if data.is_null() {
            return None;
        }
        let mut ptr: *const c_void = std::ptr::null();
        let mut len = 0usize;
        // SAFETY: per the contract. The result is owned (+1) and keeps `ptr` valid.
        let owner =
            unsafe { Handle::from_owned(dispatch_data_create_map(data, &mut ptr, &mut len)) }?;
        if ptr.is_null() || len == 0 {
            return None;
        }
        Some(Self {
            _owner: owner,
            ptr: ptr.cast(),
            len,
        })
    }

    pub(super) fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr..ptr + len` is the mapped region `_owner` keeps alive.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}
