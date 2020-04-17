use crate::{
    error::{c4error_init, Error},
    ffi::{
        c4address_fromURL, c4repl_free, c4repl_getStatus, c4repl_new, c4repl_stop, kC4Continuous,
        kC4ReplicatorOptionCookies, kC4ReplicatorOptionOutgoingConflicts, C4Address, C4Replicator,
        C4ReplicatorActivityLevel, C4ReplicatorAfterPullFunction, C4ReplicatorBeforePushFunction,
        C4ReplicatorMode, C4ReplicatorParameters, C4ReplicatorStatus,
        C4ReplicatorStatusChangedCallback, C4RevisionFlags, FLEncoder_BeginDict, FLEncoder_EndDict,
        FLEncoder_Finish, FLEncoder_Free, FLEncoder_New, FLEncoder_WriteBool, FLEncoder_WriteKey,
        FLEncoder_WriteString, FLError_kFLNoError, FLSlice, FLSlice_Copy, FLValue, FLValue_ToJSON,
    },
    fl_slice::{fl_slice_empty, flslice_as_str, AsFlSlice, FlSliceOwner},
    Database, Result,
};
use log::{error, info};
use std::{
    convert::TryFrom, mem, os::raw::c_void, panic::catch_unwind, process::abort, ptr, ptr::NonNull,
};

struct CallContext {
    state_change_ctx: NonNull<c_void>,
    before_push_ctx: *mut c_void,
    after_pull_ctx: *mut c_void,
}

impl Drop for CallContext {
    fn drop(&mut self) {
        println!("CallContext.drop()");
        // fields are dropped implicitely (I think)
    }
}

pub(crate) struct Replicator {
    inner: NonNull<C4Replicator>,
    c_callback_on_status_changed: C4ReplicatorStatusChangedCallback,
    c_call_before_push: C4ReplicatorBeforePushFunction,
    c_call_after_pull: C4ReplicatorAfterPullFunction,
    free_callback_f: unsafe fn(_: *mut c_void),
    callback_context: NonNull<c_void>,
}

/// it should be safe to call replicator API from any thread
/// according to https://github.com/couchbase/couchbase-lite-core/wiki/Thread-Safety
unsafe impl Send for Replicator {}

impl Drop for Replicator {
    fn drop(&mut self) {
        println!("Replicator.drop()");
        unsafe {
            c4repl_free(self.inner.as_ptr());
            (self.free_callback_f)(self.callback_context.as_ptr());
        }
    }
}

pub type RevisionFlags = C4RevisionFlags;

//pub trait ReplicatorHook: FnMut(&str, &str, C4RevisionFlags, FLValue) -> String + Send {}
//impl ReplicatorHook for dyn FnMut(&str, &str, C4RevisionFlags, FLValue) -> String + Send {}
//impl ReplicatorHook for Box<dyn ReplicatorHook> {}
//impl ReplicatorHook for fn(&str, &str, C4RevisionFlags, FLValue) -> String {}

impl Replicator {
    /// For example: url "ws://192.168.1.132:4984/demo/"
    pub(crate) fn new<F, G, H>(
        db: &Database,
        url: &str,
        token: Option<&str>,
        state_changed_callback: F,
        before_push: Option<G>,
        after_pull: Option<H>,
    ) -> Result<Self>
    where
        F: FnMut(C4ReplicatorStatus) + Send + 'static,
        G: FnMut(&str, &str, RevisionFlags, &str) -> String + Send + 'static,
        H: FnMut(&str, &str, RevisionFlags, &str) -> String + Send + 'static,
    {
        unsafe extern "C" fn call_on_status_changed<F>(
            c4_repl: *mut C4Replicator,
            status: C4ReplicatorStatus,
            ctx: *mut c_void,
        ) where
            F: FnMut(C4ReplicatorStatus) + Send,
        {
            info!("on_status_changed: repl {:?}, status {:?}", c4_repl, status);
            let r = catch_unwind(|| {
                let call_context = ctx as *mut CallContext;
                let call_context = match call_context.as_ref() {
                    None => panic!("Internal error - null callback context"),
                    Some(ctx) => ctx,
                };
                let boxed_state_change = call_context.state_change_ctx.as_ptr() as *mut F;
                (*boxed_state_change)(status);
            });
            if r.is_err() {
                error!("Replicator::call_on_status_changed catch panic aborting");
                abort();
            }
        }
        unsafe extern "C" fn call_before_push<G>(
            id: FLSlice,
            rev: FLSlice,
            flags: C4RevisionFlags,
            body: FLValue,
            ctx: *mut c_void,
        ) -> FLSlice
        where
            G: FnMut(&str, &str, C4RevisionFlags, &str) -> String + Send,
        {
            let call_result = catch_unwind(|| {
                println!("call_before_push");
                let call_context = ctx as *mut CallContext;
                let call_context = match call_context.as_ref() {
                    None => panic!("Internal error - null callback context"),
                    Some(ctx) => ctx,
                };
                let boxed_before_push_hook = call_context.before_push_ctx as *mut G;
                assert!(
                    !boxed_before_push_hook.is_null(),
                    "before push callback is null"
                );
                let rust_id = flslice_as_str(&id);
                let rust_rev = flslice_as_str(&rev);
                let body_str = FLValue_ToJSON(body).as_flslice();
                let body_string = flslice_as_str(&body_str);
                let call_result: String =
                    (*boxed_before_push_hook)(rust_id, rust_rev, flags, body_string);
                call_result
            });
            match call_result {
                Err(_) => {
                    error!("Replicator::call_before_push catch panic aborting");
                    panic!();
                }
                Ok(result) => {
                    println!("before_push hook returned {:?}", result);
                    let copy = FLSlice_Copy(result.as_str().as_flslice());
                    copy.as_flslice()
                }
            }
        }
        unsafe extern "C" fn call_after_pull<G>(
            id: FLSlice,
            rev: FLSlice,
            flags: C4RevisionFlags,
            body: FLValue,
            ctx: *mut c_void,
        ) -> FLSlice
        where
            G: FnMut(&str, &str, C4RevisionFlags, &str) -> String + Send,
        {
            let call_result = catch_unwind(|| {
                let call_context = ctx as *mut CallContext;
                let call_context = match call_context.as_ref() {
                    None => panic!("Internal error - null callback context"),
                    Some(ctx) => ctx,
                };
                let boxed_after_pull_hook = call_context.after_pull_ctx as *mut G;
                assert!(
                    !boxed_after_pull_hook.is_null(),
                    "after pull callback is null"
                );
                let rust_id = flslice_as_str(&id);
                let rust_rev = flslice_as_str(&rev);
                let body_str = FLValue_ToJSON(body).as_flslice();
                let body_string = flslice_as_str(&body_str);
                let call_result: String =
                    (*boxed_after_pull_hook)(rust_id, rust_rev, flags, body_string);
                call_result
            });
            match call_result {
                Err(_) => {
                    error!("Replicator::call_after_pull catch panic aborting");
                    panic!();
                }
                Ok(result) => result.as_str().as_flslice(),
            }
        }

        let with_call_before_push = before_push.is_some();
        let with_call_after_pull = after_pull.is_some();

        let boxed_state_change: *mut F = Box::into_raw(Box::new(state_changed_callback));
        let boxed_before_push: *mut G =
            before_push.map_or_else(ptr::null_mut, |f| Box::into_raw(Box::new(f)));
        println!("boxed_before_push={:?}", boxed_before_push);
        let boxed_after_pull: *mut H =
            after_pull.map_or_else(ptr::null_mut, |f| Box::into_raw(Box::new(f)));
        let callback_context = CallContext {
            state_change_ctx: unsafe { NonNull::new_unchecked(boxed_state_change as *mut c_void) },
            before_push_ctx: boxed_before_push as *mut c_void,
            after_pull_ctx: boxed_after_pull as *mut c_void,
        };
        let boxed_context: *mut CallContext = Box::into_raw(Box::new(callback_context));
        Replicator::do_new(
            db,
            url,
            token,
            free_boxed_value::<CallContext>,
            unsafe { NonNull::new_unchecked(boxed_context as *mut c_void) },
            Some(call_on_status_changed::<F>),
            if with_call_before_push {
                Some(call_before_push::<G>)
            } else {
                None
            },
            if with_call_after_pull {
                Some(call_after_pull::<H>)
            } else {
                None
            },
        )
    }

    pub(crate) fn restart(self, db: &Database, url: &str, token: Option<&str>) -> Result<Self> {
        let Replicator {
            inner: prev_inner,
            free_callback_f,
            callback_context,
            c_callback_on_status_changed,
            c_call_before_push,
            c_call_after_pull,
        } = self;
        mem::forget(self);
        unsafe {
            c4repl_stop(prev_inner.as_ptr());
            c4repl_free(prev_inner.as_ptr());
        }
        Replicator::do_new(
            db,
            url,
            token,
            free_callback_f,
            callback_context,
            c_callback_on_status_changed,
            c_call_before_push,
            c_call_after_pull,
        )
    }

    fn do_new(
        db: &Database,
        url: &str,
        token: Option<&str>,
        free_callback_f: unsafe fn(_: *mut c_void),
        callback_context: NonNull<c_void>,
        call_on_status_changed: C4ReplicatorStatusChangedCallback,
        call_before_push: C4ReplicatorBeforePushFunction,
        call_after_pull: C4ReplicatorAfterPullFunction,
    ) -> Result<Self> {
        let mut remote_addr = C4Address {
            scheme: fl_slice_empty(),
            hostname: fl_slice_empty(),
            port: 0,
            path: fl_slice_empty(),
        };
        let mut db_name = fl_slice_empty();
        if !unsafe {
            c4address_fromURL(url.as_bytes().as_flslice(), &mut remote_addr, &mut db_name)
        } {
            return Err(Error::LogicError(format!("Can not parse URL {}", url)));
        }

        let token_cookie = format!("{}={}", "SyncGatewaySession", token.unwrap_or(""));
        let option_cookies = &kC4ReplicatorOptionCookies[0..kC4ReplicatorOptionCookies.len() - 1];
        let option_allow_conflicts = &kC4ReplicatorOptionOutgoingConflicts
            [0..kC4ReplicatorOptionOutgoingConflicts.len() - 1];
        let options: FlSliceOwner = if token.is_some() {
            unsafe {
                let enc = FLEncoder_New();

                FLEncoder_BeginDict(enc, 2);
                FLEncoder_WriteKey(enc, option_cookies.as_flslice());
                FLEncoder_WriteString(enc, token_cookie.as_bytes().as_flslice());

                FLEncoder_WriteKey(enc, option_allow_conflicts.as_flslice());
                FLEncoder_WriteBool(enc, true);
                FLEncoder_EndDict(enc);

                let mut fl_err = FLError_kFLNoError;
                let res = FLEncoder_Finish(enc, &mut fl_err);
                FLEncoder_Free(enc);
                if fl_err != FLError_kFLNoError {
                    return Err(Error::FlError(fl_err));
                }
                res.into()
            }
        } else {
            unsafe {
                let enc = FLEncoder_New();

                FLEncoder_BeginDict(enc, 1);
                FLEncoder_WriteKey(enc, option_allow_conflicts.as_flslice());
                FLEncoder_WriteBool(enc, true);
                FLEncoder_EndDict(enc);

                let mut fl_err = FLError_kFLNoError;
                let res = FLEncoder_Finish(enc, &mut fl_err);
                FLEncoder_Free(enc);
                if fl_err != FLError_kFLNoError {
                    return Err(Error::FlError(fl_err));
                }
                res.into()
            }
        };

        let repl_params = C4ReplicatorParameters {
            push: kC4Continuous as C4ReplicatorMode,
            pull: kC4Continuous as C4ReplicatorMode,
            optionsDictFleece: options.as_bytes().as_flslice(),
            pushFilter: None,
            validationFunc: None,
            onStatusChanged: call_on_status_changed,
            onDocumentsEnded: None,
            onBlobProgress: None,
            callbackContext: callback_context.as_ptr(),
            socketFactory: ptr::null_mut(),
            dontStart: false,
            beforePush: call_before_push,
            afterPull: call_after_pull,
        };

        let mut c4err = c4error_init();
        let repl = unsafe {
            c4repl_new(
                db.inner.0.as_ptr(),
                remote_addr,
                db_name,
                ptr::null_mut(),
                repl_params,
                &mut c4err,
            )
        };
        NonNull::new(repl)
            .map(|inner| Replicator {
                inner,
                free_callback_f,
                callback_context,
                c_callback_on_status_changed: call_on_status_changed,
                c_call_before_push: call_before_push,
                c_call_after_pull: call_after_pull,
            })
            .ok_or_else(|| {
                unsafe { free_callback_f(callback_context.as_ptr()) };
                c4err.into()
            })
    }

    pub(crate) fn stop(self) {
        unsafe { c4repl_stop(self.inner.as_ptr()) };
    }

    pub(crate) fn status(&self) -> C4ReplicatorStatus {
        unsafe { c4repl_getStatus(self.inner.as_ptr()) }
    }
}

/// The possible states of a replicator
#[derive(Debug)]
pub enum ReplicatorState {
    /// Finished, or got a fatal error.
    Stopped(Error),
    /// Offline, replication doesn't not work
    Offline,
    /// Connection is in progress.
    Connecting,
    /// Continuous replicator has caught up and is waiting for changes.
    Idle,
    ///< Connected and actively working.
    Busy,
}

unsafe fn free_boxed_value<T>(p: *mut c_void) {
    drop(Box::from_raw(p as *mut T));
}

impl TryFrom<C4ReplicatorStatus> for ReplicatorState {
    type Error = Error;
    fn try_from(status: C4ReplicatorStatus) -> Result<Self> {
        #![allow(non_upper_case_globals)]
        macro_rules! define_activity_level {
            ($const_name:ident) => {
                const $const_name: C4ReplicatorActivityLevel =
                    crate::ffi::$const_name as C4ReplicatorActivityLevel;
            };
        }

        //TODO: use bindgen and https://github.com/rust-lang/rust/issues/44109
        //when it becomes stable
        define_activity_level!(kC4Stopped);
        define_activity_level!(kC4Offline);
        define_activity_level!(kC4Connecting);
        define_activity_level!(kC4Idle);
        define_activity_level!(kC4Busy);

        match status.level {
            kC4Stopped => Ok(ReplicatorState::Stopped(status.error.into())),
            kC4Offline => Ok(ReplicatorState::Offline),
            kC4Connecting => Ok(ReplicatorState::Connecting),
            kC4Idle => Ok(ReplicatorState::Idle),
            kC4Busy => Ok(ReplicatorState::Busy),
            _ => Err(Error::LogicError(format!("unknown level for {:?}", status))),
        }
    }
}
