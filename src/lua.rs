use std::any::TypeId;
use std::cell::{RefCell, UnsafeCell};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fmt;
use std::marker::PhantomData;
use std::os::raw::{c_char, c_int, c_void};
use std::panic::resume_unwind;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::{mem, ptr, str};

use crate::error::{Error, Result};
use crate::ffi;
use crate::function::Function;
use crate::hook::{Debug, HookTriggers};
use crate::scope::Scope;
use crate::stdlib::StdLib;
use crate::string::String;
use crate::table::Table;
use crate::thread::Thread;
use crate::types::{
    Callback, HookCallback, Integer, LightUserData, LuaRef, MaybeSend, Number, RegistryKey,
};
use crate::userdata::{
    AnyUserData, MetaMethod, UserData, UserDataCell, UserDataFields, UserDataMethods,
};
use crate::util::{
    assert_stack, callback_error, check_stack, get_destructed_userdata_metatable, get_gc_userdata,
    get_main_state, get_userdata, get_wrapped_error, init_error_registry, init_gc_metatable_for,
    init_userdata_metatable, pop_error, push_gc_userdata, push_userdata, push_wrapped_error,
    StackGuard, WrappedError, WrappedPanic,
};
use crate::value::{FromLua, FromLuaMulti, MultiValue, Nil, ToLua, ToLuaMulti, Value};

#[cfg(feature = "async")]
use {
    crate::types::AsyncCallback,
    futures_core::{
        future::{Future, LocalBoxFuture},
        task::{Context, Poll, Waker},
    },
    futures_task::noop_waker,
    futures_util::future::{self, TryFutureExt},
};

#[cfg(feature = "serialize")]
use serde::Serialize;

/// Top level Lua struct which holds the Lua state itself.
pub struct Lua {
    pub(crate) state: *mut ffi::lua_State,
    main_state: Option<*mut ffi::lua_State>,
    extra: Arc<Mutex<ExtraData>>,
    ephemeral: bool,
    safe: bool,
    // Lua has lots of interior mutability, should not be RefUnwindSafe
    _no_ref_unwind_safe: PhantomData<UnsafeCell<()>>,
}

// Data associated with the Lua.
struct ExtraData {
    registered_userdata: HashMap<TypeId, c_int>,
    registered_userdata_mt: HashSet<isize>,
    registry_unref_list: Arc<Mutex<Option<Vec<c_int>>>>,

    libs: StdLib,
    mem_info: *mut MemoryInfo,
    safe: bool, // Same as in the Lua struct

    ref_thread: *mut ffi::lua_State,
    ref_stack_size: c_int,
    ref_stack_top: c_int,
    ref_free: Vec<c_int>,

    hook_callback: Option<HookCallback>,
}

#[cfg_attr(any(feature = "lua51", feature = "luajit"), allow(dead_code))]
struct MemoryInfo {
    used_memory: isize,
    memory_limit: isize,
}

/// Mode of the Lua garbage collector (GC).
///
/// In Lua 5.4 GC can work in two modes: incremental and generational.
/// Previous Lua versions support only incremental GC.
///
/// More information can be found in the Lua 5.x [documentation].
///
/// [documentation]: https://www.lua.org/manual/5.4/manual.html#2.5
#[derive(Clone, Copy, Debug)]
pub enum GCMode {
    Incremental,
    /// Requires `feature = "lua54"`
    #[cfg(any(feature = "lua54", doc))]
    Generational,
}

/// Controls Lua interpreter behaviour such as Rust panics handling.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct LuaOptions {
    /// Catch Rust panics when using [`pcall`]/[`xpcall`].
    ///
    /// If disabled, wraps these functions and automatically resumes panic if found.
    /// Also in Lua 5.1 adds ability to provide arguments to [`xpcall`] similar to Lua >= 5.2.
    ///
    /// If enabled, keeps [`pcall`]/[`xpcall`] unmodified.
    /// Panics are still automatically resumed if returned back to the Rust side.
    ///
    /// Default: **true**
    ///
    /// [`pcall`]: https://www.lua.org/manual/5.3/manual.html#pdf-pcall
    /// [`xpcall`]: https://www.lua.org/manual/5.3/manual.html#pdf-xpcall
    pub catch_rust_panics: bool,
}

impl Default for LuaOptions {
    fn default() -> Self {
        LuaOptions {
            catch_rust_panics: true,
        }
    }
}

impl LuaOptions {
    /// Retruns a new instance of `LuaOptions` with default parameters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets [`catch_rust_panics`] option.
    ///
    /// [`catch_rust_panics`]: #structfield.catch_rust_panics
    pub fn catch_rust_panics(mut self, enabled: bool) -> Self {
        self.catch_rust_panics = enabled;
        self
    }
}

#[cfg(feature = "async")]
pub(crate) static ASYNC_POLL_PENDING: u8 = 0;
#[cfg(feature = "async")]
pub(crate) static WAKER_REGISTRY_KEY: u8 = 0;
pub(crate) static EXTRA_REGISTRY_KEY: u8 = 0;

/// Requires `feature = "send"`
#[cfg(feature = "send")]
#[cfg_attr(docsrs, doc(cfg(feature = "send")))]
unsafe impl Send for Lua {}

impl Drop for Lua {
    fn drop(&mut self) {
        unsafe {
            if !self.ephemeral {
                let extra = mlua_expect!(self.extra.lock(), "extra is poisoned");
                mlua_debug_assert!(
                    ffi::lua_gettop(extra.ref_thread) == extra.ref_stack_top
                        && extra.ref_stack_top as usize == extra.ref_free.len(),
                    "reference leak detected"
                );
                let mut unref_list =
                    mlua_expect!(extra.registry_unref_list.lock(), "unref list poisoned");
                *unref_list = None;
                ffi::lua_close(mlua_expect!(self.main_state, "main_state is null"));
                if !extra.mem_info.is_null() {
                    Box::from_raw(extra.mem_info);
                }
            }
        }
    }
}

impl fmt::Debug for Lua {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Lua({:p})", self.state)
    }
}

impl Lua {
    /// Creates a new Lua state and loads the **safe** subset of the standard libraries.
    ///
    /// # Safety
    /// The created Lua state would have _some_ safety guarantees and would not allow to load unsafe
    /// standard libraries or C modules.
    ///
    /// See [`StdLib`] documentation for a list of unsafe modules that cannot be loaded.
    ///
    /// [`StdLib`]: struct.StdLib.html
    #[allow(clippy::new_without_default)]
    pub fn new() -> Lua {
        mlua_expect!(
            Self::new_with(StdLib::ALL_SAFE, LuaOptions::default()),
            "can't create new safe Lua state"
        )
    }

    /// Creates a new Lua state and loads all the standard libraries.
    ///
    /// # Safety
    /// The created Lua state would not have safety guarantees and would allow to load C modules.
    pub unsafe fn unsafe_new() -> Lua {
        Self::unsafe_new_with(StdLib::ALL, LuaOptions::default())
    }

    /// Creates a new Lua state and loads the specified safe subset of the standard libraries.
    ///
    /// Use the [`StdLib`] flags to specifiy the libraries you want to load.
    ///
    /// # Safety
    /// The created Lua state would have _some_ safety guarantees and would not allow to load unsafe
    /// standard libraries or C modules.
    ///
    /// See [`StdLib`] documentation for a list of unsafe modules that cannot be loaded.
    ///
    /// [`StdLib`]: struct.StdLib.html
    pub fn new_with(libs: StdLib, options: LuaOptions) -> Result<Lua> {
        if libs.contains(StdLib::DEBUG) {
            return Err(Error::SafetyError(
                "the unsafe `debug` module can't be loaded using safe `new_with`".to_string(),
            ));
        }
        #[cfg(feature = "luajit")]
        {
            if libs.contains(StdLib::FFI) {
                return Err(Error::SafetyError(
                    "the unsafe `ffi` module can't be loaded using safe `new_with`".to_string(),
                ));
            }
        }

        let mut lua = unsafe { Self::unsafe_new_with(libs, options) };

        if libs.contains(StdLib::PACKAGE) {
            mlua_expect!(lua.disable_c_modules(), "Error during disabling C modules");
        }
        lua.safe = true;
        mlua_expect!(lua.extra.lock(), "extra is poisoned").safe = true;

        Ok(lua)
    }

    /// Creates a new Lua state and loads the specified subset of the standard libraries.
    ///
    /// Use the [`StdLib`] flags to specifiy the libraries you want to load.
    ///
    /// # Safety
    /// The created Lua state will not have safety guarantees and allow to load C modules.
    ///
    /// [`StdLib`]: struct.StdLib.html
    pub unsafe fn unsafe_new_with(libs: StdLib, options: LuaOptions) -> Lua {
        #[cfg_attr(any(feature = "lua51", feature = "luajit"), allow(dead_code))]
        unsafe extern "C" fn allocator(
            extra_data: *mut c_void,
            ptr: *mut c_void,
            osize: usize,
            nsize: usize,
        ) -> *mut c_void {
            use std::alloc;

            let mem_info = &mut *(extra_data as *mut MemoryInfo);

            if nsize == 0 {
                // Free memory
                if !ptr.is_null() {
                    let layout =
                        alloc::Layout::from_size_align_unchecked(osize, ffi::SYS_MIN_ALIGN);
                    alloc::dealloc(ptr as *mut u8, layout);
                    mem_info.used_memory -= osize as isize;
                }
                return ptr::null_mut();
            }

            // Are we fit to the memory limits?
            let mut mem_diff = nsize as isize;
            if !ptr.is_null() {
                mem_diff -= osize as isize;
            }
            let new_used_memory = mem_info.used_memory + mem_diff;
            if mem_info.memory_limit > 0 && new_used_memory > mem_info.memory_limit {
                return ptr::null_mut();
            }

            let new_layout = alloc::Layout::from_size_align_unchecked(nsize, ffi::SYS_MIN_ALIGN);

            if ptr.is_null() {
                // Allocate new memory
                let new_ptr = alloc::alloc(new_layout) as *mut c_void;
                if !new_ptr.is_null() {
                    mem_info.used_memory += mem_diff;
                }
                return new_ptr;
            }

            // Reallocate memory
            let old_layout = alloc::Layout::from_size_align_unchecked(osize, ffi::SYS_MIN_ALIGN);
            let new_ptr = alloc::realloc(ptr as *mut u8, old_layout, nsize) as *mut c_void;

            if !new_ptr.is_null() {
                mem_info.used_memory += mem_diff;
            } else if !ptr.is_null() && nsize < osize {
                // Should not happend
                alloc::handle_alloc_error(new_layout);
            }

            new_ptr
        }

        #[cfg(any(feature = "lua54", feature = "lua53", feature = "lua52"))]
        let mem_info = Box::into_raw(Box::new(MemoryInfo {
            used_memory: 0,
            memory_limit: 0,
        }));

        #[cfg(any(feature = "lua54", feature = "lua53", feature = "lua52"))]
        let state = ffi::lua_newstate(allocator, mem_info as *mut c_void);
        #[cfg(any(feature = "lua51", feature = "luajit"))]
        let state = ffi::luaL_newstate();

        mlua_expect!(
            ffi::safe::luaL_requiref(state, "_G", ffi::luaopen_base, 1),
            "Error during loading base lib"
        );
        ffi::lua_pop(state, 1);

        let mut lua = Lua::init_from_ptr(state);
        lua.ephemeral = false;
        #[cfg(any(feature = "lua54", feature = "lua53", feature = "lua52"))]
        {
            mlua_expect!(lua.extra.lock(), "extra is poisoned").mem_info = mem_info;
        }

        mlua_expect!(
            load_from_std_lib(state, libs),
            "Error during loading standard libraries"
        );
        mlua_expect!(lua.extra.lock(), "extra is poisoned").libs |= libs;

        if !options.catch_rust_panics {
            mlua_expect!(
                (|| -> Result<()> {
                    let _sg = StackGuard::new(lua.state);

                    #[cfg(any(feature = "lua54", feature = "lua53", feature = "lua52"))]
                    ffi::lua_rawgeti(lua.state, ffi::LUA_REGISTRYINDEX, ffi::LUA_RIDX_GLOBALS);
                    #[cfg(any(feature = "lua51", feature = "luajit"))]
                    ffi::lua_pushvalue(lua.state, ffi::LUA_GLOBALSINDEX);

                    ffi::lua_pushcfunction(lua.state, ffi::safe::lua_nopanic_pcall);
                    ffi::safe::lua_rawsetfield(lua.state, -2, "pcall")?;

                    ffi::lua_pushcfunction(lua.state, ffi::safe::lua_nopanic_xpcall);
                    ffi::safe::lua_rawsetfield(lua.state, -2, "xpcall")?;

                    Ok(())
                })(),
                "Error during applying option `catch_rust_panics`"
            )
        }

        lua
    }

    /// Constructs a new Lua instance from an existing raw state.
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn init_from_ptr(state: *mut ffi::lua_State) -> Lua {
        let maybe_main_state = get_main_state(state);
        let main_state = maybe_main_state.unwrap_or(state);
        let main_state_top = ffi::lua_gettop(main_state);

        let ref_thread = mlua_expect!(
            (|state| {
                // Before initializing the error registry, we must set Error/Panic size.
                // Error/Panic keys are not needed during the registry initialization.
                ffi::safe::WRAPPED_ERROR_SIZE = mem::size_of::<WrappedError>();
                ffi::safe::WRAPPED_PANIC_SIZE = mem::size_of::<WrappedPanic>();

                let (wrapped_error_key, wrapped_panic_key) = init_error_registry(state)?;

                ffi::safe::WRAPPED_ERROR_KEY = wrapped_error_key as *const c_void;
                ffi::safe::WRAPPED_PANIC_KEY = wrapped_panic_key as *const c_void;

                // Create the internal metatables and place them in the registry
                // to prevent them from being garbage collected.

                init_gc_metatable_for::<Callback>(state, None)?;
                init_gc_metatable_for::<Lua>(state, None)?;
                init_gc_metatable_for::<Weak<Mutex<ExtraData>>>(state, None)?;
                #[cfg(feature = "async")]
                {
                    init_gc_metatable_for::<AsyncCallback>(state, None)?;
                    init_gc_metatable_for::<LocalBoxFuture<Result<MultiValue>>>(state, None)?;
                    init_gc_metatable_for::<Option<Waker>>(state, None)?;

                    // Create empty Waker slot
                    push_gc_userdata::<Option<Waker>>(state, None)?;
                    let waker_key = &WAKER_REGISTRY_KEY as *const u8 as *const c_void;
                    ffi::safe::lua_rawsetp(state, ffi::LUA_REGISTRYINDEX, waker_key)?;
                }

                // Init serde metatables
                #[cfg(feature = "serialize")]
                crate::serde::init_metatables(state)?;

                // Create ref stack thread and place it in the registry to prevent it from being garbage
                // collected.

                let ref_thread = ffi::safe::lua_newthread(state)?;
                ffi::safe::luaL_ref(state, ffi::LUA_REGISTRYINDEX)?;

                Ok::<_, Error>(ref_thread)
            })(main_state),
            "Error during Lua construction",
        );

        // Create ExtraData

        let extra = Arc::new(Mutex::new(ExtraData {
            registered_userdata: HashMap::new(),
            registered_userdata_mt: HashSet::new(),
            registry_unref_list: Arc::new(Mutex::new(Some(Vec::new()))),
            ref_thread,
            libs: StdLib::NONE,
            mem_info: ptr::null_mut(),
            safe: false,
            // We need 1 extra stack space to move values in and out of the ref stack.
            ref_stack_size: ffi::LUA_MINSTACK - 1,
            ref_stack_top: 0,
            ref_free: Vec::new(),
            hook_callback: None,
        }));

        mlua_expect!(
            push_gc_userdata(main_state, Arc::downgrade(&extra)),
            "Error while storing extra data",
        );
        let extra_key = &EXTRA_REGISTRY_KEY as *const u8 as *const c_void;
        mlua_expect!(
            ffi::safe::lua_rawsetp(main_state, ffi::LUA_REGISTRYINDEX, extra_key,),
            "Error while storing extra data"
        );

        mlua_debug_assert!(
            ffi::lua_gettop(main_state) == main_state_top,
            "stack leak during creation"
        );
        assert_stack(main_state, ffi::LUA_MINSTACK);

        Lua {
            state,
            main_state: maybe_main_state,
            extra,
            ephemeral: true,
            safe: false,
            _no_ref_unwind_safe: PhantomData,
        }
    }

    /// Loads the specified subset of the standard libraries into an existing Lua state.
    ///
    /// Use the [`StdLib`] flags to specifiy the libraries you want to load.
    ///
    /// [`StdLib`]: struct.StdLib.html
    pub fn load_from_std_lib(&self, libs: StdLib) -> Result<()> {
        if self.safe && libs.contains(StdLib::DEBUG) {
            return Err(Error::SafetyError(
                "the unsafe `debug` module can't be loaded in safe mode".to_string(),
            ));
        }
        #[cfg(feature = "luajit")]
        {
            if self.safe && libs.contains(StdLib::FFI) {
                return Err(Error::SafetyError(
                    "the unsafe `ffi` module can't be loaded in safe mode".to_string(),
                ));
            }
        }

        let state = self.main_state.unwrap_or(self.state);
        let res = unsafe { load_from_std_lib(state, libs) };

        // If `package` library loaded into a safe lua state then disable C modules
        let curr_libs = mlua_expect!(self.extra.lock(), "extra is poisoned").libs;
        if self.safe && (curr_libs ^ (curr_libs | libs)).contains(StdLib::PACKAGE) {
            mlua_expect!(self.disable_c_modules(), "Error during disabling C modules");
        }
        mlua_expect!(self.extra.lock(), "extra is poisoned").libs |= libs;

        res
    }

    /// Consumes and leaks `Lua` object, returning a static reference `&'static Lua`.
    ///
    /// This function is useful when the `Lua` object is supposed to live for the remainder
    /// of the program's life.
    /// In particular in asynchronous context this will allow to spawn Lua tasks to execute
    /// in background.
    ///
    /// Dropping the returned reference will cause a memory leak. If this is not acceptable,
    /// the reference should first be wrapped with the [`Lua::from_static`] function producing a `Lua`.
    /// This `Lua` object can then be dropped which will properly release the allocated memory.
    ///
    /// [`Lua::from_static`]: #method.from_static
    pub fn into_static(self) -> &'static Self {
        Box::leak(Box::new(self))
    }

    /// Constructs a `Lua` from a static reference to it.
    ///
    /// # Safety
    /// This function is unsafe because improper use may lead to memory problems or undefined behavior.
    pub unsafe fn from_static(lua: &'static Lua) -> Self {
        *Box::from_raw(lua as *const Lua as *mut Lua)
    }

    // Executes module entrypoint function, which returns only one Value.
    // The returned value then pushed to the Lua stack.
    #[doc(hidden)]
    pub fn entrypoint1<'lua, 'callback, R, F>(&'lua self, func: F) -> Result<c_int>
    where
        'lua: 'callback,
        R: ToLua<'callback>,
        F: 'static + MaybeSend + Fn(&'callback Lua) -> Result<R>,
    {
        let cb = self.create_callback(Box::new(move |lua, _| func(lua)?.to_lua_multi(lua)))?;
        unsafe { self.push_value(cb.call(())?).map(|_| 1) }
    }

    /// Sets a 'hook' function that will periodically be called as Lua code executes.
    ///
    /// When exactly the hook function is called depends on the contents of the `triggers`
    /// parameter, see [`HookTriggers`] for more details.
    ///
    /// The provided hook function can error, and this error will be propagated through the Lua code
    /// that was executing at the time the hook was triggered. This can be used to implement a
    /// limited form of execution limits by setting [`HookTriggers.every_nth_instruction`] and
    /// erroring once an instruction limit has been reached.
    ///
    /// # Example
    ///
    /// Shows each line number of code being executed by the Lua interpreter.
    ///
    /// ```
    /// # use mlua::{Lua, HookTriggers, Result};
    /// # fn main() -> Result<()> {
    /// let lua = Lua::new();
    /// lua.set_hook(HookTriggers {
    ///     every_line: true, ..Default::default()
    /// }, |_lua, debug| {
    ///     println!("line {}", debug.curr_line());
    ///     Ok(())
    /// })?;
    ///
    /// lua.load(r#"
    ///     local x = 2 + 3
    ///     local y = x * 63
    ///     local z = string.len(x..", "..y)
    /// "#).exec()
    /// # }
    /// ```
    ///
    /// [`HookTriggers`]: struct.HookTriggers.html
    /// [`HookTriggers.every_nth_instruction`]: struct.HookTriggers.html#field.every_nth_instruction
    pub fn set_hook<F>(&self, triggers: HookTriggers, callback: F) -> Result<()>
    where
        F: 'static + MaybeSend + FnMut(&Lua, Debug) -> Result<()>,
    {
        let state = self.main_state.ok_or(Error::MainThreadNotAvailable)?;
        unsafe {
            let mut extra = mlua_expect!(self.extra.lock(), "extra is poisoned");
            extra.hook_callback = Some(Arc::new(RefCell::new(callback)));
            ffi::lua_sethook(
                state,
                Some(ffi::safe::lua_call_mlua_hook_proc),
                triggers.mask(),
                triggers.count(),
            );
        }
        Ok(())
    }

    /// Remove any hook previously set by `set_hook`. This function has no effect if a hook was not
    /// previously set.
    pub fn remove_hook(&self) {
        // If main_state is not available, then sethook wasn't called.
        let state = match self.main_state {
            Some(state) => state,
            None => return,
        };
        let mut extra = mlua_expect!(self.extra.lock(), "extra is poisoned");
        unsafe {
            extra.hook_callback = None;
            ffi::lua_sethook(state, None, 0, 0);
        }
    }

    /// Returns the amount of memory (in bytes) currently used inside this Lua state.
    pub fn used_memory(&self) -> usize {
        let extra = mlua_expect!(self.extra.lock(), "extra is poisoned");
        let state = self.main_state.unwrap_or(self.state);
        if extra.mem_info.is_null() {
            // Get data from the Lua GC
            unsafe {
                let used_kbytes = ffi::lua_gc(state, ffi::LUA_GCCOUNT, 0);
                let used_kbytes_rem = ffi::lua_gc(state, ffi::LUA_GCCOUNTB, 0);
                return (used_kbytes as usize) * 1024 + (used_kbytes_rem as usize);
            }
        }
        unsafe { (*extra.mem_info).used_memory as usize }
    }

    /// Sets a memory limit (in bytes) on this Lua state.
    ///
    /// Once an allocation occurs that would pass this memory limit,
    /// a `Error::MemoryError` is generated instead.
    /// Returns previous limit (zero means no limit).
    ///
    /// Does not work on module mode where Lua state is managed externally.
    ///
    /// Requires `feature = "lua54/lua53/lua52"`
    #[cfg(any(feature = "lua54", feature = "lua53", feature = "lua52", doc))]
    pub fn set_memory_limit(&self, memory_limit: usize) -> Result<usize> {
        let mut extra = mlua_expect!(self.extra.lock(), "extra is poisoned");
        if extra.mem_info.is_null() {
            return Err(Error::MemoryLimitNotAvailable);
        }
        unsafe {
            let prev_limit = (*extra.mem_info).memory_limit as usize;
            (*extra.mem_info).memory_limit = memory_limit as isize;
            Ok(prev_limit)
        }
    }

    /// Returns true if the garbage collector is currently running automatically.
    ///
    /// Requires `feature = "lua54/lua53/lua52"`
    #[cfg(any(feature = "lua54", feature = "lua53", feature = "lua52", doc))]
    pub fn gc_is_running(&self) -> bool {
        let state = self.main_state.unwrap_or(self.state);
        unsafe { ffi::lua_gc(state, ffi::LUA_GCISRUNNING, 0) != 0 }
    }

    /// Stop the Lua GC from running
    pub fn gc_stop(&self) {
        let state = self.main_state.unwrap_or(self.state);
        unsafe { ffi::lua_gc(state, ffi::LUA_GCSTOP, 0) };
    }

    /// Restarts the Lua GC if it is not running
    pub fn gc_restart(&self) {
        let state = self.main_state.unwrap_or(self.state);
        unsafe { ffi::lua_gc(state, ffi::LUA_GCRESTART, 0) };
    }

    /// Perform a full garbage-collection cycle.
    ///
    /// It may be necessary to call this function twice to collect all currently unreachable
    /// objects. Once to finish the current gc cycle, and once to start and finish the next cycle.
    pub fn gc_collect(&self) -> Result<()> {
        let state = self.main_state.unwrap_or(self.state);
        unsafe { ffi::safe::lua_gc(state, ffi::LUA_GCCOLLECT, 0).map(|_| ()) }
    }

    /// Steps the garbage collector one indivisible step.
    ///
    /// Returns true if this has finished a collection cycle.
    pub fn gc_step(&self) -> Result<bool> {
        self.gc_step_kbytes(0)
    }

    /// Steps the garbage collector as though memory had been allocated.
    ///
    /// if `kbytes` is 0, then this is the same as calling `gc_step`. Returns true if this step has
    /// finished a collection cycle.
    pub fn gc_step_kbytes(&self, kbytes: c_int) -> Result<bool> {
        let state = self.main_state.unwrap_or(self.state);
        unsafe { Ok(ffi::safe::lua_gc(state, ffi::LUA_GCSTEP, kbytes)? != 0) }
    }

    /// Sets the 'pause' value of the collector.
    ///
    /// Returns the previous value of 'pause'. More information can be found in the [Lua 5.3
    /// documentation][lua_doc].
    ///
    /// [lua_doc]: https://www.lua.org/manual/5.3/manual.html#2.5
    pub fn gc_set_pause(&self, pause: c_int) -> c_int {
        let state = self.main_state.unwrap_or(self.state);
        unsafe { ffi::lua_gc(state, ffi::LUA_GCSETPAUSE, pause) }
    }

    /// Sets the 'step multiplier' value of the collector.
    ///
    /// Returns the previous value of the 'step multiplier'. More information can be found in the
    /// Lua 5.x [documentation][lua_doc].
    ///
    /// [lua_doc]: https://www.lua.org/manual/5.3/manual.html#2.5
    pub fn gc_set_step_multiplier(&self, step_multiplier: c_int) -> c_int {
        let state = self.main_state.unwrap_or(self.state);
        unsafe { ffi::lua_gc(state, ffi::LUA_GCSETSTEPMUL, step_multiplier) }
    }

    /// Changes the collector to incremental mode with the given parameters.
    ///
    /// Returns the previous mode (always `GCMode::Incremental` in Lua < 5.4).
    /// More information can be found in the Lua 5.x [documentation][lua_doc].
    ///
    /// [lua_doc]: https://www.lua.org/manual/5.4/manual.html#2.5.1
    pub fn gc_inc(&self, pause: c_int, step_multiplier: c_int, step_size: c_int) -> GCMode {
        let state = self.main_state.unwrap_or(self.state);

        #[cfg(any(
            feature = "lua53",
            feature = "lua52",
            feature = "lua51",
            feature = "luajit"
        ))]
        {
            if pause > 0 {
                unsafe { ffi::lua_gc(state, ffi::LUA_GCSETPAUSE, pause) };
            }
            if step_multiplier > 0 {
                unsafe { ffi::lua_gc(state, ffi::LUA_GCSETSTEPMUL, step_multiplier) };
            }
            let _ = step_size; // Ignored
            GCMode::Incremental
        }

        #[cfg(feature = "lua54")]
        let prev_mode =
            unsafe { ffi::lua_gc(state, ffi::LUA_GCINC, pause, step_multiplier, step_size) };
        #[cfg(feature = "lua54")]
        match prev_mode {
            ffi::LUA_GCINC => GCMode::Incremental,
            ffi::LUA_GCGEN => GCMode::Generational,
            _ => unreachable!(),
        }
    }

    /// Changes the collector to generational mode with the given parameters.
    ///
    /// Returns the previous mode. More information about the generational GC
    /// can be found in the Lua 5.4 [documentation][lua_doc].
    ///
    /// Requires `feature = "lua54"`
    ///
    /// [lua_doc]: https://www.lua.org/manual/5.4/manual.html#2.5.2
    #[cfg(any(feature = "lua54", doc))]
    pub fn gc_gen(&self, minor_multiplier: c_int, major_multiplier: c_int) -> GCMode {
        let state = self.main_state.unwrap_or(self.state);
        let prev_mode =
            unsafe { ffi::lua_gc(state, ffi::LUA_GCGEN, minor_multiplier, major_multiplier) };
        match prev_mode {
            ffi::LUA_GCGEN => GCMode::Generational,
            ffi::LUA_GCINC => GCMode::Incremental,
            _ => unreachable!(),
        }
    }

    /// Returns Lua source code as a `Chunk` builder type.
    ///
    /// In order to actually compile or run the resulting code, you must call [`Chunk::exec`] or
    /// similar on the returned builder. Code is not even parsed until one of these methods is
    /// called.
    ///
    /// If this `Lua` was created with `unsafe_new`, `load` will automatically detect and load
    /// chunks of either text or binary type, as if passing `bt` mode to `luaL_loadbufferx`.
    ///
    /// [`Chunk::exec`]: struct.Chunk.html#method.exec
    pub fn load<'lua, 'a, S>(&'lua self, source: &'a S) -> Chunk<'lua, 'a>
    where
        S: AsRef<[u8]> + ?Sized,
    {
        Chunk {
            lua: self,
            source: source.as_ref(),
            name: None,
            env: None,
            mode: None,
        }
    }

    fn load_chunk<'lua>(
        &'lua self,
        source: &[u8],
        name: Option<&CString>,
        env: Option<Value<'lua>>,
        mode: Option<ChunkMode>,
    ) -> Result<Function<'lua>> {
        unsafe {
            let _sg = StackGuard::new(self.state);
            check_stack(self.state, 1)?;

            let mode_str = match mode {
                Some(ChunkMode::Binary) if self.safe => {
                    return Err(Error::SafetyError(
                        "binary chunks are disabled in safe mode".to_string(),
                    ))
                }
                Some(ChunkMode::Binary) => cstr!("b"),
                Some(ChunkMode::Text) => cstr!("t"),
                None if source.starts_with(ffi::LUA_SIGNATURE) && self.safe => {
                    return Err(Error::SafetyError(
                        "binary chunks are disabled in safe mode".to_string(),
                    ))
                }
                None => cstr!("bt"),
            };

            match ffi::luaL_loadbufferx(
                self.state,
                source.as_ptr() as *const c_char,
                source.len(),
                name.map(|n| n.as_ptr()).unwrap_or_else(ptr::null),
                mode_str,
            ) {
                ffi::LUA_OK => {
                    if let Some(env) = env {
                        self.push_value(env)?;
                        #[cfg(any(feature = "lua54", feature = "lua53", feature = "lua52"))]
                        ffi::lua_setupvalue(self.state, -2, 1);
                        #[cfg(any(feature = "lua51", feature = "luajit"))]
                        ffi::lua_setfenv(self.state, -2);
                    }
                    Ok(Function(self.pop_ref()))
                }
                err => Err(pop_error(self.state, err)),
            }
        }
    }

    /// Create and return an interned Lua string. Lua strings can be arbitrary [u8] data including
    /// embedded nulls, so in addition to `&str` and `&String`, you can also pass plain `&[u8]`
    /// here.
    pub fn create_string<S>(&self, s: &S) -> Result<String>
    where
        S: AsRef<[u8]> + ?Sized,
    {
        unsafe {
            let _sg = StackGuard::new(self.state);
            check_stack(self.state, 3)?;
            ffi::safe::lua_pushstring(self.state, s)?;
            Ok(String(self.pop_ref()))
        }
    }

    /// Creates and returns a new empty table.
    pub fn create_table(&self) -> Result<Table> {
        unsafe {
            let _sg = StackGuard::new(self.state);
            check_stack(self.state, 2)?;
            ffi::safe::lua_newtable(self.state)?;
            Ok(Table(self.pop_ref()))
        }
    }

    /// Creates and returns a new empty table, with the specified capacity.
    /// `narr` is a hint for how many elements the table will have as a sequence;
    /// `nrec` is a hint for how many other elements the table will have.
    /// Lua may use these hints to preallocate memory for the new table.
    pub fn create_table_with_capacity(&self, narr: c_int, nrec: c_int) -> Result<Table> {
        unsafe {
            let _sg = StackGuard::new(self.state);
            check_stack(self.state, 4)?;
            ffi::safe::lua_createtable(self.state, narr, nrec)?;
            Ok(Table(self.pop_ref()))
        }
    }

    /// Creates a table and fills it with values from an iterator.
    pub fn create_table_from<'lua, K, V, I>(&'lua self, iter: I) -> Result<Table<'lua>>
    where
        K: ToLua<'lua>,
        V: ToLua<'lua>,
        I: IntoIterator<Item = (K, V)>,
    {
        unsafe {
            let _sg = StackGuard::new(self.state);
            check_stack(self.state, 6)?;

            let iter = iter.into_iter();
            let lower_bound = iter.size_hint().0;
            ffi::safe::lua_createtable(self.state, 0, lower_bound as c_int)?;
            for (k, v) in iter {
                self.push_value(k.to_lua(self)?)?;
                self.push_value(v.to_lua(self)?)?;
                ffi::safe::lua_rawset(self.state, -3)?;
            }

            Ok(Table(self.pop_ref()))
        }
    }

    /// Creates a table from an iterator of values, using `1..` as the keys.
    pub fn create_sequence_from<'lua, T, I>(&'lua self, iter: I) -> Result<Table<'lua>>
    where
        T: ToLua<'lua>,
        I: IntoIterator<Item = T>,
    {
        unsafe {
            let _sg = StackGuard::new(self.state);
            check_stack(self.state, 6)?;

            let iter = iter.into_iter();
            let lower_bound = iter.size_hint().0;
            ffi::safe::lua_createtable(self.state, lower_bound as c_int, 0)?;
            for (i, v) in iter.enumerate() {
                self.push_value(v.to_lua(self)?)?;
                ffi::safe::lua_rawseti(self.state, -2, (i + 1) as Integer)?;
            }

            Ok(Table(self.pop_ref()))
        }
    }

    /// Wraps a Rust function or closure, creating a callable Lua function handle to it.
    ///
    /// The function's return value is always a `Result`: If the function returns `Err`, the error
    /// is raised as a Lua error, which can be caught using `(x)pcall` or bubble up to the Rust code
    /// that invoked the Lua code. This allows using the `?` operator to propagate errors through
    /// intermediate Lua code.
    ///
    /// If the function returns `Ok`, the contained value will be converted to one or more Lua
    /// values. For details on Rust-to-Lua conversions, refer to the [`ToLua`] and [`ToLuaMulti`]
    /// traits.
    ///
    /// # Examples
    ///
    /// Create a function which prints its argument:
    ///
    /// ```
    /// # use mlua::{Lua, Result};
    /// # fn main() -> Result<()> {
    /// # let lua = Lua::new();
    /// let greet = lua.create_function(|_, name: String| {
    ///     println!("Hello, {}!", name);
    ///     Ok(())
    /// });
    /// # let _ = greet;    // used
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Use tuples to accept multiple arguments:
    ///
    /// ```
    /// # use mlua::{Lua, Result};
    /// # fn main() -> Result<()> {
    /// # let lua = Lua::new();
    /// let print_person = lua.create_function(|_, (name, age): (String, u8)| {
    ///     println!("{} is {} years old!", name, age);
    ///     Ok(())
    /// });
    /// # let _ = print_person;    // used
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// [`ToLua`]: trait.ToLua.html
    /// [`ToLuaMulti`]: trait.ToLuaMulti.html
    pub fn create_function<'lua, 'callback, A, R, F>(&'lua self, func: F) -> Result<Function<'lua>>
    where
        'lua: 'callback,
        A: FromLuaMulti<'callback>,
        R: ToLuaMulti<'callback>,
        F: 'static + MaybeSend + Fn(&'callback Lua, A) -> Result<R>,
    {
        self.create_callback(Box::new(move |lua, args| {
            func(lua, A::from_lua_multi(args, lua)?)?.to_lua_multi(lua)
        }))
    }

    /// Wraps a Rust mutable closure, creating a callable Lua function handle to it.
    ///
    /// This is a version of [`create_function`] that accepts a FnMut argument. Refer to
    /// [`create_function`] for more information about the implementation.
    ///
    /// [`create_function`]: #method.create_function
    pub fn create_function_mut<'lua, 'callback, A, R, F>(
        &'lua self,
        func: F,
    ) -> Result<Function<'lua>>
    where
        'lua: 'callback,
        A: FromLuaMulti<'callback>,
        R: ToLuaMulti<'callback>,
        F: 'static + MaybeSend + FnMut(&'callback Lua, A) -> Result<R>,
    {
        let func = RefCell::new(func);
        self.create_function(move |lua, args| {
            (&mut *func
                .try_borrow_mut()
                .map_err(|_| Error::RecursiveMutCallback)?)(lua, args)
        })
    }

    /// Wraps a Rust async function or closure, creating a callable Lua function handle to it.
    ///
    /// While executing the function Rust will poll Future and if the result is not ready, call
    /// `yield()` passing internal representation of a `Poll::Pending` value.
    ///
    /// The function must be called inside Lua coroutine ([`Thread`]) to be able to suspend its execution.
    /// An executor should be used to poll [`AsyncThread`] and mlua will take a provided Waker
    /// in that case. Otherwise noop waker will be used if try to call the function outside of Rust
    /// executors.
    ///
    /// The family of `call_async()` functions takes care about creating [`Thread`].
    ///
    /// Requires `feature = "async"`
    ///
    /// # Examples
    ///
    /// Non blocking sleep:
    ///
    /// ```
    /// use std::time::Duration;
    /// use futures_timer::Delay;
    /// use mlua::{Lua, Result};
    ///
    /// async fn sleep(_lua: &Lua, n: u64) -> Result<&'static str> {
    ///     Delay::new(Duration::from_millis(n)).await;
    ///     Ok("done")
    /// }
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<()> {
    ///     let lua = Lua::new();
    ///     lua.globals().set("sleep", lua.create_async_function(sleep)?)?;
    ///     let res: String = lua.load("return sleep(...)").call_async(100).await?; // Sleep 100ms
    ///     assert_eq!(res, "done");
    ///     Ok(())
    /// }
    /// ```
    ///
    /// [`Thread`]: struct.Thread.html
    /// [`AsyncThread`]: struct.AsyncThread.html
    #[cfg(feature = "async")]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    pub fn create_async_function<'lua, 'callback, A, R, F, FR>(
        &'lua self,
        func: F,
    ) -> Result<Function<'lua>>
    where
        'lua: 'callback,
        A: FromLuaMulti<'callback>,
        R: ToLuaMulti<'callback>,
        F: 'static + MaybeSend + Fn(&'callback Lua, A) -> FR,
        FR: 'lua + Future<Output = Result<R>>,
    {
        self.create_async_callback(Box::new(move |lua, args| {
            let args = match A::from_lua_multi(args, lua) {
                Ok(args) => args,
                Err(e) => return Box::pin(future::err(e)),
            };
            Box::pin(func(lua, args).and_then(move |ret| future::ready(ret.to_lua_multi(lua))))
        }))
    }

    /// Wraps a Lua function into a new thread (or coroutine).
    ///
    /// Equivalent to `coroutine.create`.
    pub fn create_thread<'lua>(&'lua self, func: Function<'lua>) -> Result<Thread<'lua>> {
        unsafe {
            let _sg = StackGuard::new(self.state);
            check_stack(self.state, 2)?;

            let thread_state = ffi::safe::lua_newthread(self.state)?;
            self.push_ref(&func.0);
            ffi::lua_xmove(self.state, thread_state, 1);

            Ok(Thread(self.pop_ref()))
        }
    }

    /// Create a Lua userdata object from a custom userdata type.
    pub fn create_userdata<T>(&self, data: T) -> Result<AnyUserData>
    where
        T: 'static + MaybeSend + UserData,
    {
        unsafe { self.make_userdata(UserDataCell::new(data)) }
    }

    /// Create a Lua userdata object from a custom serializable userdata type.
    ///
    /// Requires `feature = "serialize"`
    #[cfg(feature = "serialize")]
    #[cfg_attr(docsrs, doc(cfg(feature = "serialize")))]
    pub fn create_ser_userdata<T>(&self, data: T) -> Result<AnyUserData>
    where
        T: 'static + MaybeSend + UserData + Serialize,
    {
        unsafe { self.make_userdata(UserDataCell::new_ser(data)) }
    }

    /// Returns a handle to the global environment.
    pub fn globals(&self) -> Table {
        unsafe {
            let _sg = StackGuard::new(self.state);
            assert_stack(self.state, 1);
            #[cfg(any(feature = "lua54", feature = "lua53", feature = "lua52"))]
            ffi::lua_rawgeti(self.state, ffi::LUA_REGISTRYINDEX, ffi::LUA_RIDX_GLOBALS);
            #[cfg(any(feature = "lua51", feature = "luajit"))]
            ffi::lua_pushvalue(self.state, ffi::LUA_GLOBALSINDEX);
            Table(self.pop_ref())
        }
    }

    /// Returns a handle to the active `Thread`. For calls to `Lua` this will be the main Lua thread,
    /// for parameters given to a callback, this will be whatever Lua thread called the callback.
    pub fn current_thread(&self) -> Thread {
        unsafe {
            let _sg = StackGuard::new(self.state);
            assert_stack(self.state, 1);
            ffi::lua_pushthread(self.state);
            Thread(self.pop_ref())
        }
    }

    /// Calls the given function with a `Scope` parameter, giving the function the ability to create
    /// userdata and callbacks from rust types that are !Send or non-'static.
    ///
    /// The lifetime of any function or userdata created through `Scope` lasts only until the
    /// completion of this method call, on completion all such created values are automatically
    /// dropped and Lua references to them are invalidated. If a script accesses a value created
    /// through `Scope` outside of this method, a Lua error will result. Since we can ensure the
    /// lifetime of values created through `Scope`, and we know that `Lua` cannot be sent to another
    /// thread while `Scope` is live, it is safe to allow !Send datatypes and whose lifetimes only
    /// outlive the scope lifetime.
    ///
    /// Inside the scope callback, all handles created through Scope will share the same unique 'lua
    /// lifetime of the parent `Lua`. This allows scoped and non-scoped values to be mixed in
    /// API calls, which is very useful (e.g. passing a scoped userdata to a non-scoped function).
    /// However, this also enables handles to scoped values to be trivially leaked from the given
    /// callback. This is not dangerous, though!  After the callback returns, all scoped values are
    /// invalidated, which means that though references may exist, the Rust types backing them have
    /// dropped. `Function` types will error when called, and `AnyUserData` will be typeless. It
    /// would be impossible to prevent handles to scoped values from escaping anyway, since you
    /// would always be able to smuggle them through Lua state.
    pub fn scope<'lua, 'scope, R, F>(&'lua self, f: F) -> Result<R>
    where
        'lua: 'scope,
        R: 'static,
        F: FnOnce(&Scope<'lua, 'scope>) -> Result<R>,
    {
        f(&Scope::new(self))
    }

    /// An asynchronous version of [`scope`] that allows to create scoped async functions and
    /// execute them.
    ///
    /// Requires `feature = "async"`
    ///
    /// [`scope`]: #method.scope
    #[cfg(feature = "async")]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    pub fn async_scope<'lua, 'scope, R, F, FR>(
        &'lua self,
        f: F,
    ) -> LocalBoxFuture<'scope, Result<R>>
    where
        'lua: 'scope,
        R: 'static,
        F: FnOnce(Scope<'lua, 'scope>) -> FR,
        FR: 'scope + Future<Output = Result<R>>,
    {
        Box::pin(f(Scope::new(self)))
    }

    /// Attempts to coerce a Lua value into a String in a manner consistent with Lua's internal
    /// behavior.
    ///
    /// To succeed, the value must be a string (in which case this is a no-op), an integer, or a
    /// number.
    pub fn coerce_string<'lua>(&'lua self, v: Value<'lua>) -> Result<Option<String<'lua>>> {
        Ok(match v {
            Value::String(s) => Some(s),
            v => unsafe {
                let _sg = StackGuard::new(self.state);
                check_stack(self.state, 5)?;

                self.push_value(v)?;
                if !ffi::safe::lua_tolstring(self.state, -1, ptr::null_mut())?.is_null() {
                    Some(String(self.pop_ref()))
                } else {
                    None
                }
            },
        })
    }

    /// Attempts to coerce a Lua value into an integer in a manner consistent with Lua's internal
    /// behavior.
    ///
    /// To succeed, the value must be an integer, a floating point number that has an exact
    /// representation as an integer, or a string that can be converted to an integer. Refer to the
    /// Lua manual for details.
    pub fn coerce_integer(&self, v: Value) -> Result<Option<Integer>> {
        Ok(match v {
            Value::Integer(i) => Some(i),
            v => unsafe {
                let _sg = StackGuard::new(self.state);
                check_stack(self.state, 2)?;

                self.push_value(v)?;
                let mut isint = 0;
                let i = ffi::lua_tointegerx(self.state, -1, &mut isint);
                if isint == 0 {
                    None
                } else {
                    Some(i)
                }
            },
        })
    }

    /// Attempts to coerce a Lua value into a Number in a manner consistent with Lua's internal
    /// behavior.
    ///
    /// To succeed, the value must be a number or a string that can be converted to a number. Refer
    /// to the Lua manual for details.
    pub fn coerce_number(&self, v: Value) -> Result<Option<Number>> {
        Ok(match v {
            Value::Number(n) => Some(n),
            v => unsafe {
                let _sg = StackGuard::new(self.state);
                check_stack(self.state, 2)?;

                self.push_value(v)?;
                let mut isnum = 0;
                let n = ffi::lua_tonumberx(self.state, -1, &mut isnum);
                if isnum == 0 {
                    None
                } else {
                    Some(n)
                }
            },
        })
    }

    /// Converts a value that implements `ToLua` into a `Value` instance.
    pub fn pack<'lua, T: ToLua<'lua>>(&'lua self, t: T) -> Result<Value<'lua>> {
        t.to_lua(self)
    }

    /// Converts a `Value` instance into a value that implements `FromLua`.
    pub fn unpack<'lua, T: FromLua<'lua>>(&'lua self, value: Value<'lua>) -> Result<T> {
        T::from_lua(value, self)
    }

    /// Converts a value that implements `ToLuaMulti` into a `MultiValue` instance.
    pub fn pack_multi<'lua, T: ToLuaMulti<'lua>>(&'lua self, t: T) -> Result<MultiValue<'lua>> {
        t.to_lua_multi(self)
    }

    /// Converts a `MultiValue` instance into a value that implements `FromLuaMulti`.
    pub fn unpack_multi<'lua, T: FromLuaMulti<'lua>>(
        &'lua self,
        value: MultiValue<'lua>,
    ) -> Result<T> {
        T::from_lua_multi(value, self)
    }

    /// Set a value in the Lua registry based on a string name.
    ///
    /// This value will be available to rust from all `Lua` instances which share the same main
    /// state.
    pub fn set_named_registry_value<'lua, S, T>(&'lua self, name: &S, t: T) -> Result<()>
    where
        S: AsRef<[u8]> + ?Sized,
        T: ToLua<'lua>,
    {
        let t = t.to_lua(self)?;
        unsafe {
            let _sg = StackGuard::new(self.state);
            check_stack(self.state, 5)?;

            self.push_value(t)?;
            ffi::safe::lua_rawsetfield(self.state, ffi::LUA_REGISTRYINDEX, name)
        }
    }

    /// Get a value from the Lua registry based on a string name.
    ///
    /// Any Lua instance which shares the underlying main state may call this method to
    /// get a value previously set by [`set_named_registry_value`].
    ///
    /// [`set_named_registry_value`]: #method.set_named_registry_value
    pub fn named_registry_value<'lua, S, T>(&'lua self, name: &S) -> Result<T>
    where
        S: AsRef<[u8]> + ?Sized,
        T: FromLua<'lua>,
    {
        let value = unsafe {
            let _sg = StackGuard::new(self.state);
            check_stack(self.state, 3)?;

            ffi::safe::lua_pushstring(self.state, name)?;
            ffi::lua_rawget(self.state, ffi::LUA_REGISTRYINDEX);

            self.pop_value()
        };
        T::from_lua(value, self)
    }

    /// Removes a named value in the Lua registry.
    ///
    /// Equivalent to calling [`set_named_registry_value`] with a value of Nil.
    ///
    /// [`set_named_registry_value`]: #method.set_named_registry_value
    pub fn unset_named_registry_value<S>(&self, name: &S) -> Result<()>
    where
        S: AsRef<[u8]> + ?Sized,
    {
        self.set_named_registry_value(name, Nil)
    }

    /// Place a value in the Lua registry with an auto-generated key.
    ///
    /// This value will be available to rust from all `Lua` instances which share the same main
    /// state.
    ///
    /// Be warned, garbage collection of values held inside the registry is not automatic, see
    /// [`RegistryKey`] for more details.
    ///
    /// [`RegistryKey`]: struct.RegistryKey.html
    pub fn create_registry_value<'lua, T: ToLua<'lua>>(&'lua self, t: T) -> Result<RegistryKey> {
        let t = t.to_lua(self)?;
        unsafe {
            let _sg = StackGuard::new(self.state);
            check_stack(self.state, 4)?;

            self.push_value(t)?;
            let registry_id = ffi::safe::luaL_ref(self.state, ffi::LUA_REGISTRYINDEX)?;

            let extra = mlua_expect!(self.extra.lock(), "extra is poisoned");

            Ok(RegistryKey {
                registry_id,
                unref_list: extra.registry_unref_list.clone(),
            })
        }
    }

    /// Get a value from the Lua registry by its `RegistryKey`
    ///
    /// Any Lua instance which shares the underlying main state may call this method to get a value
    /// previously placed by [`create_registry_value`].
    ///
    /// [`create_registry_value`]: #method.create_registry_value
    pub fn registry_value<'lua, T: FromLua<'lua>>(&'lua self, key: &RegistryKey) -> Result<T> {
        if !self.owns_registry_value(key) {
            return Err(Error::MismatchedRegistryKey);
        }

        let value = unsafe {
            let _sg = StackGuard::new(self.state);
            check_stack(self.state, 1)?;

            ffi::lua_rawgeti(
                self.state,
                ffi::LUA_REGISTRYINDEX,
                key.registry_id as Integer,
            );
            self.pop_value()
        };
        T::from_lua(value, self)
    }

    /// Removes a value from the Lua registry.
    ///
    /// You may call this function to manually remove a value placed in the registry with
    /// [`create_registry_value`]. In addition to manual `RegistryKey` removal, you can also call
    /// [`expire_registry_values`] to automatically remove values from the registry whose
    /// `RegistryKey`s have been dropped.
    ///
    /// [`create_registry_value`]: #method.create_registry_value
    /// [`expire_registry_values`]: #method.expire_registry_values
    pub fn remove_registry_value(&self, key: RegistryKey) -> Result<()> {
        if !self.owns_registry_value(&key) {
            return Err(Error::MismatchedRegistryKey);
        }
        unsafe {
            ffi::luaL_unref(self.state, ffi::LUA_REGISTRYINDEX, key.take());
        }
        Ok(())
    }

    /// Returns true if the given `RegistryKey` was created by a `Lua` which shares the underlying
    /// main state with this `Lua` instance.
    ///
    /// Other than this, methods that accept a `RegistryKey` will return
    /// `Error::MismatchedRegistryKey` if passed a `RegistryKey` that was not created with a
    /// matching `Lua` state.
    pub fn owns_registry_value(&self, key: &RegistryKey) -> bool {
        let extra = mlua_expect!(self.extra.lock(), "extra is poisoned");
        Arc::ptr_eq(&key.unref_list, &extra.registry_unref_list)
    }

    /// Remove any registry values whose `RegistryKey`s have all been dropped.
    ///
    /// Unlike normal handle values, `RegistryKey`s do not automatically remove themselves on Drop,
    /// but you can call this method to remove any unreachable registry values not manually removed
    /// by `Lua::remove_registry_value`.
    pub fn expire_registry_values(&self) {
        unsafe {
            let extra = mlua_expect!(self.extra.lock(), "extra is poisoned");
            let mut unref_list =
                mlua_expect!(extra.registry_unref_list.lock(), "unref list poisoned");
            let unref_list = mem::replace(&mut *unref_list, Some(Vec::new()));
            for id in mlua_expect!(unref_list, "unref list not set") {
                ffi::luaL_unref(self.state, ffi::LUA_REGISTRYINDEX, id);
            }
        }
    }

    // Uses 2 stack spaces, does not call checkstack
    pub(crate) unsafe fn push_value(&self, value: Value) -> Result<()> {
        match value {
            Value::Nil => {
                ffi::lua_pushnil(self.state);
            }

            Value::Boolean(b) => {
                ffi::lua_pushboolean(self.state, if b { 1 } else { 0 });
            }

            Value::LightUserData(ud) => {
                ffi::lua_pushlightuserdata(self.state, ud.0);
            }

            Value::Integer(i) => {
                ffi::lua_pushinteger(self.state, i);
            }

            Value::Number(n) => {
                ffi::lua_pushnumber(self.state, n);
            }

            Value::String(s) => {
                self.push_ref(&s.0);
            }

            Value::Table(t) => {
                self.push_ref(&t.0);
            }

            Value::Function(f) => {
                self.push_ref(&f.0);
            }

            Value::Thread(t) => {
                self.push_ref(&t.0);
            }

            Value::UserData(ud) => {
                self.push_ref(&ud.0);
            }

            Value::Error(e) => {
                push_wrapped_error(self.state, e)?;
            }
        }

        Ok(())
    }

    // Uses 2 stack spaces, does not call checkstack
    pub(crate) unsafe fn pop_value(&self) -> Value {
        let state = self.state;
        match ffi::lua_type(state, -1) {
            ffi::LUA_TNIL => {
                ffi::lua_pop(state, 1);
                Nil
            }

            ffi::LUA_TBOOLEAN => {
                let b = Value::Boolean(ffi::lua_toboolean(state, -1) != 0);
                ffi::lua_pop(state, 1);
                b
            }

            ffi::LUA_TLIGHTUSERDATA => {
                let ud = Value::LightUserData(LightUserData(ffi::lua_touserdata(state, -1)));
                ffi::lua_pop(state, 1);
                ud
            }

            ffi::LUA_TNUMBER => {
                if ffi::lua_isinteger(state, -1) != 0 {
                    let i = Value::Integer(ffi::lua_tointeger(state, -1));
                    ffi::lua_pop(state, 1);
                    i
                } else {
                    let n = Value::Number(ffi::lua_tonumber(state, -1));
                    ffi::lua_pop(state, 1);
                    n
                }
            }

            ffi::LUA_TSTRING => Value::String(String(self.pop_ref())),

            ffi::LUA_TTABLE => Value::Table(Table(self.pop_ref())),

            ffi::LUA_TFUNCTION => Value::Function(Function(self.pop_ref())),

            ffi::LUA_TUSERDATA => {
                // We must prevent interaction with userdata types other than UserData OR a WrappedError.
                // WrappedPanics are automatically resumed.
                if let Some(err) = get_wrapped_error(state, -1).as_ref() {
                    let err = err.0.clone();
                    ffi::lua_pop(state, 1);
                    Value::Error(err)
                } else if let Some(panic) = get_gc_userdata::<WrappedPanic>(state, -1).as_mut() {
                    if let Some(panic) = (*panic).0.take() {
                        ffi::lua_pop(state, 1);
                        resume_unwind(panic);
                    }
                    // Previously resumed panic?
                    ffi::lua_pop(state, 1);
                    Nil
                } else {
                    Value::UserData(AnyUserData(self.pop_ref()))
                }
            }

            ffi::LUA_TTHREAD => Value::Thread(Thread(self.pop_ref())),

            _ => mlua_panic!("LUA_TNONE in pop_value"),
        }
    }

    // Pushes a LuaRef value onto the stack, uses 1 stack space, does not call checkstack
    pub(crate) unsafe fn push_ref<'lua>(&'lua self, lref: &LuaRef<'lua>) {
        assert!(
            Arc::ptr_eq(&lref.lua.extra, &self.extra),
            "Lua instance passed Value created from a different main Lua state"
        );
        let extra = mlua_expect!(self.extra.lock(), "extra is poisoned");
        ffi::lua_pushvalue(extra.ref_thread, lref.index);
        ffi::lua_xmove(extra.ref_thread, self.state, 1);
    }

    // Pops the topmost element of the stack and stores a reference to it. This pins the object,
    // preventing garbage collection until the returned `LuaRef` is dropped.
    //
    // References are stored in the stack of a specially created auxiliary thread that exists only
    // to store reference values. This is much faster than storing these in the registry, and also
    // much more flexible and requires less bookkeeping than storing them directly in the currently
    // used stack. The implementation is somewhat biased towards the use case of a relatively small
    // number of short term references being created, and `RegistryKey` being used for long term
    // references.
    pub(crate) unsafe fn pop_ref(&self) -> LuaRef {
        let extra = mlua_expect!(self.extra.lock(), "extra is poisoned");
        ffi::lua_xmove(self.state, extra.ref_thread, 1);
        let index = ref_stack_pop(extra);
        LuaRef { lua: self, index }
    }

    pub(crate) fn clone_ref<'lua>(&'lua self, lref: &LuaRef<'lua>) -> LuaRef<'lua> {
        unsafe {
            let extra = mlua_expect!(self.extra.lock(), "extra is poisoned");
            ffi::lua_pushvalue(extra.ref_thread, lref.index);
            let index = ref_stack_pop(extra);
            LuaRef { lua: self, index }
        }
    }

    pub(crate) fn drop_ref<'lua>(&'lua self, lref: &mut LuaRef<'lua>) {
        unsafe {
            let mut extra = mlua_expect!(self.extra.lock(), "extra is poisoned");
            ffi::lua_pushnil(extra.ref_thread);
            ffi::lua_replace(extra.ref_thread, lref.index);
            extra.ref_free.push(lref.index);
        }
    }

    pub(crate) unsafe fn push_userdata_metatable<T: 'static + UserData>(&self) -> Result<()> {
        let type_id = TypeId::of::<T>();
        if let Some(&table_id) = mlua_expect!(self.extra.lock(), "extra is poisoned")
            .registered_userdata
            .get(&type_id)
        {
            ffi::lua_rawgeti(self.state, ffi::LUA_REGISTRYINDEX, table_id as Integer);
            return Ok(());
        }

        let _sg = StackGuard::new_extra(self.state, 1);
        check_stack(self.state, 13)?;

        let mut fields = StaticUserDataFields::default();
        let mut methods = StaticUserDataMethods::default();
        T::add_fields(&mut fields);
        T::add_methods(&mut methods);

        // Prepare metatable, add meta methods first and then meta fields
        let metatable_nrec = methods.meta_methods.len() + fields.meta_fields.len();
        ffi::safe::lua_createtable(self.state, 0, metatable_nrec as c_int)?;
        for (k, m) in methods.meta_methods {
            self.push_value(Value::Function(self.create_callback(m)?))?;
            ffi::safe::lua_rawsetfield(self.state, -2, k.validate()?.name())?;
        }
        for (k, f) in fields.meta_fields {
            self.push_value(f(self)?)?;
            ffi::safe::lua_rawsetfield(self.state, -2, k.validate()?.name())?;
        }
        let metatable_index = ffi::lua_absindex(self.state, -1);

        let mut extra_tables_count = 0;

        let mut field_getters_index = None;
        let field_getters_nrec = fields.field_getters.len();
        if field_getters_nrec > 0 {
            ffi::safe::lua_createtable(self.state, 0, field_getters_nrec as c_int)?;
            for (k, m) in fields.field_getters {
                self.push_value(Value::Function(self.create_callback(m)?))?;
                ffi::safe::lua_rawsetfield(self.state, -2, &k)?;
            }
            field_getters_index = Some(ffi::lua_absindex(self.state, -1));
            extra_tables_count += 1;
        }

        let mut field_setters_index = None;
        let field_setters_nrec = fields.field_setters.len();
        if field_setters_nrec > 0 {
            ffi::safe::lua_createtable(self.state, 0, field_setters_nrec as c_int)?;
            for (k, m) in fields.field_setters {
                self.push_value(Value::Function(self.create_callback(m)?))?;
                ffi::safe::lua_rawsetfield(self.state, -2, &k)?;
            }
            field_setters_index = Some(ffi::lua_absindex(self.state, -1));
            extra_tables_count += 1;
        }

        let mut methods_index = None;
        #[cfg(feature = "async")]
        let methods_nrec = methods.methods.len() + methods.async_methods.len();
        #[cfg(not(feature = "async"))]
        let methods_nrec = methods.methods.len();
        if methods_nrec > 0 {
            ffi::safe::lua_createtable(self.state, 0, methods_nrec as c_int)?;
            for (k, m) in methods.methods {
                self.push_value(Value::Function(self.create_callback(m)?))?;
                ffi::safe::lua_rawsetfield(self.state, -2, &k)?;
            }
            #[cfg(feature = "async")]
            for (k, m) in methods.async_methods {
                self.push_value(Value::Function(self.create_async_callback(m)?))?;
                ffi::safe::lua_rawsetfield(self.state, -2, &k)?;
            }
            methods_index = Some(ffi::lua_absindex(self.state, -1));
            extra_tables_count += 1;
        }

        init_userdata_metatable::<UserDataCell<T>>(
            self.state,
            metatable_index,
            field_getters_index,
            field_setters_index,
            methods_index,
        )?;

        // Pop extra tables to get metatable on top of the stack
        ffi::lua_pop(self.state, extra_tables_count);

        let ptr = ffi::lua_topointer(self.state, -1);
        ffi::lua_pushvalue(self.state, -1);
        let id = ffi::safe::luaL_ref(self.state, ffi::LUA_REGISTRYINDEX)?;

        let mut extra = mlua_expect!(self.extra.lock(), "extra is poisoned");
        extra.registered_userdata.insert(type_id, id);
        extra.registered_userdata_mt.insert(ptr as isize);

        Ok(())
    }

    pub(crate) fn register_userdata_metatable(&self, id: isize) {
        let mut extra = mlua_expect!(self.extra.lock(), "extra is poisoned");
        extra.registered_userdata_mt.insert(id);
    }

    pub(crate) fn deregister_userdata_metatable(&self, id: isize) {
        let mut extra = mlua_expect!(self.extra.lock(), "extra is poisoned");
        extra.registered_userdata_mt.remove(&id);
    }

    // Pushes a LuaRef value onto the stack, checking that it's a registered
    // and not destructed UserData.
    // Uses 3 stack spaces, does not call checkstack.
    pub(crate) unsafe fn push_userdata_ref(&self, lref: &LuaRef) -> Result<()> {
        self.push_ref(lref);
        if ffi::lua_getmetatable(self.state, -1) == 0 {
            return Err(Error::UserDataTypeMismatch);
        }
        // Check that userdata is registered
        let ptr = ffi::lua_topointer(self.state, -1);
        let extra = mlua_expect!(self.extra.lock(), "extra is poisoned");
        if extra.registered_userdata_mt.contains(&(ptr as isize)) {
            ffi::lua_pop(self.state, 1);
            return Ok(());
        }
        // Maybe userdata was destructed?
        get_destructed_userdata_metatable(self.state);
        if ffi::lua_rawequal(self.state, -1, -2) != 0 {
            ffi::lua_pop(self.state, 2);
            return Err(Error::UserDataDestructed);
        }
        ffi::lua_pop(self.state, 2);
        Err(Error::UserDataTypeMismatch)
    }

    // Creates a Function out of a Callback containing a 'static Fn. This is safe ONLY because the
    // Fn is 'static, otherwise it could capture 'callback arguments improperly. Without ATCs, we
    // cannot easily deal with the "correct" callback type of:
    //
    // Box<for<'lua> Fn(&'lua Lua, MultiValue<'lua>) -> Result<MultiValue<'lua>>)>
    //
    // So we instead use a caller provided lifetime, which without the 'static requirement would be
    // unsafe.
    pub(crate) fn create_callback<'lua, 'callback>(
        &'lua self,
        func: Callback<'callback, 'static>,
    ) -> Result<Function<'lua>>
    where
        'lua: 'callback,
    {
        unsafe extern "C" fn call_callback(state: *mut ffi::lua_State) -> c_int {
            callback_error(state, |nargs| {
                let upvalue_idx1 = ffi::lua_upvalueindex(2);
                let upvalue_idx2 = ffi::lua_upvalueindex(3);
                if ffi::lua_type(state, upvalue_idx1) == ffi::LUA_TNIL
                    || ffi::lua_type(state, upvalue_idx2) == ffi::LUA_TNIL
                {
                    return Err(Error::CallbackDestructed);
                }
                let func = get_userdata::<Callback>(state, upvalue_idx1);
                let lua = get_userdata::<Lua>(state, upvalue_idx2);

                if nargs < ffi::LUA_MINSTACK {
                    check_stack(state, ffi::LUA_MINSTACK - nargs)?;
                }

                let lua = &mut *lua;
                lua.state = state;

                let mut args = MultiValue::new();
                args.reserve(nargs as usize);
                for _ in 0..nargs {
                    args.push_front(lua.pop_value());
                }

                let results = (*func)(lua, args)?;
                let nresults = results.len() as c_int;

                check_stack(state, nresults)?;
                for r in results {
                    lua.push_value(r)?;
                }

                Ok(nresults)
            })
        }

        unsafe {
            let _sg = StackGuard::new(self.state);
            check_stack(self.state, 5)?;

            push_gc_userdata::<Callback>(self.state, mem::transmute(func))?;
            push_gc_userdata(self.state, self.clone())?;
            ffi::safe::lua_pushrclosure(self.state, call_callback, 2)?;

            Ok(Function(self.pop_ref()))
        }
    }

    #[cfg(feature = "async")]
    pub(crate) fn create_async_callback<'lua, 'callback>(
        &'lua self,
        func: AsyncCallback<'callback, 'static>,
    ) -> Result<Function<'lua>>
    where
        'lua: 'callback,
    {
        #[cfg(any(feature = "lua54", feature = "lua53", feature = "lua52"))]
        {
            let libs = mlua_expect!(self.extra.lock(), "extra is poisoned").libs;
            if !libs.contains(StdLib::COROUTINE) {
                self.load_from_std_lib(StdLib::COROUTINE)?;
            }
        }

        unsafe extern "C" fn call_callback(state: *mut ffi::lua_State) -> c_int {
            callback_error(state, |nargs| {
                let upvalue_idx1 = ffi::lua_upvalueindex(2);
                let upvalue_idx2 = ffi::lua_upvalueindex(3);
                if ffi::lua_type(state, upvalue_idx1) == ffi::LUA_TNIL
                    || ffi::lua_type(state, upvalue_idx2) == ffi::LUA_TNIL
                {
                    return Err(Error::CallbackDestructed);
                }
                let func = get_userdata::<AsyncCallback>(state, upvalue_idx1);
                let lua = get_userdata::<Lua>(state, upvalue_idx2);

                if nargs < ffi::LUA_MINSTACK {
                    check_stack(state, ffi::LUA_MINSTACK - nargs)?;
                }

                let lua = &mut *lua;
                lua.state = state;

                let mut args = MultiValue::new();
                args.reserve(nargs as usize);
                for _ in 0..nargs {
                    args.push_front(lua.pop_value());
                }

                let fut = (*func)(lua, args);
                push_gc_userdata(state, fut)?;
                push_gc_userdata(state, lua.clone())?;

                ffi::safe::lua_pushrclosure(state, poll_future, 2)?;

                Ok(1)
            })
        }

        unsafe extern "C" fn poll_future(state: *mut ffi::lua_State) -> c_int {
            callback_error(state, |nargs| {
                let upvalue_idx1 = ffi::lua_upvalueindex(2);
                let upvalue_idx2 = ffi::lua_upvalueindex(3);
                if ffi::lua_type(state, upvalue_idx1) == ffi::LUA_TNIL
                    || ffi::lua_type(state, upvalue_idx2) == ffi::LUA_TNIL
                {
                    return Err(Error::CallbackDestructed);
                }
                let fut = get_userdata::<LocalBoxFuture<Result<MultiValue>>>(state, upvalue_idx1);
                let lua = get_userdata::<Lua>(state, upvalue_idx2);

                if nargs < ffi::LUA_MINSTACK {
                    check_stack(state, ffi::LUA_MINSTACK - nargs)?;
                }

                let lua = &mut *lua;

                // Try to get an outer poll waker
                let waker_key = &WAKER_REGISTRY_KEY as *const u8 as *const c_void;
                ffi::lua_rawgetp(state, ffi::LUA_REGISTRYINDEX, waker_key);
                let waker = match get_gc_userdata::<Option<Waker>>(state, -1).as_ref() {
                    Some(Some(waker)) => waker.clone(),
                    _ => noop_waker(),
                };
                ffi::lua_pop(state, 1);

                let mut ctx = Context::from_waker(&waker);

                match (*fut).as_mut().poll(&mut ctx) {
                    Poll::Pending => {
                        check_stack(state, 1)?;
                        ffi::lua_pushboolean(state, 0);
                        Ok(1)
                    }
                    Poll::Ready(results) => {
                        let results = results?;
                        let nresults = results.len() as Integer;
                        let results = lua.create_sequence_from(results)?;
                        check_stack(state, 3)?;
                        ffi::lua_pushboolean(state, 1);
                        lua.push_value(Value::Table(results))?;
                        lua.push_value(Value::Integer(nresults))?;
                        Ok(3)
                    }
                }
            })
        }

        let get_poll = unsafe {
            let _sg = StackGuard::new(self.state);
            check_stack(self.state, 5)?;

            push_gc_userdata::<AsyncCallback>(self.state, mem::transmute(func))?;
            push_gc_userdata(self.state, self.clone())?;
            ffi::safe::lua_pushrclosure(self.state, call_callback, 2)?;

            Function(self.pop_ref())
        };

        let coroutine = self.globals().get::<_, Table>("coroutine")?;

        let env = self.create_table_with_capacity(0, 4)?;
        env.set("get_poll", get_poll)?;
        env.set("yield", coroutine.get::<_, Function>("yield")?)?;
        env.set(
            "unpack",
            self.create_function(|_, (tbl, len): (Table, Integer)| {
                Ok(MultiValue::from_vec(
                    tbl.raw_sequence_values_by_len(Some(len))
                        .collect::<Result<Vec<Value>>>()?,
                ))
            })?,
        )?;
        env.set("pending", {
            LightUserData(&ASYNC_POLL_PENDING as *const u8 as *mut c_void)
        })?;

        // We set `poll` variable in the env table to be able to destroy upvalues
        self.load(
            r#"
            poll = get_poll(...)
            local poll, pending, yield, unpack = poll, pending, yield, unpack
            while true do
                local ready, res, nres = poll()
                if ready then
                    return unpack(res, nres)
                end
                yield(pending)
            end
            "#,
        )
        .set_name("_mlua_async_poll")?
        .set_environment(env)?
        .into_function()
    }

    pub(crate) unsafe fn make_userdata<T>(&self, data: UserDataCell<T>) -> Result<AnyUserData>
    where
        T: 'static + UserData,
    {
        let _sg = StackGuard::new(self.state);
        check_stack(self.state, 2)?;

        push_userdata(self.state, data)?;
        self.push_userdata_metatable::<T>()?;
        ffi::lua_setmetatable(self.state, -2);

        Ok(AnyUserData(self.pop_ref()))
    }

    pub(crate) fn clone(&self) -> Self {
        Lua {
            state: self.state,
            main_state: self.main_state,
            extra: self.extra.clone(),
            ephemeral: true,
            safe: self.safe,
            _no_ref_unwind_safe: PhantomData,
        }
    }

    fn disable_c_modules(&self) -> Result<()> {
        let package: Table = self.globals().get("package")?;

        package.set(
            "loadlib",
            self.create_function(|_, ()| -> Result<()> {
                Err(Error::SafetyError(
                    "package.loadlib is disabled in safe mode".to_string(),
                ))
            })?,
        )?;

        #[cfg(any(feature = "lua54", feature = "lua53", feature = "lua52"))]
        let searchers: Table = package.get("searchers")?;
        #[cfg(any(feature = "lua51", feature = "luajit"))]
        let searchers: Table = package.get("loaders")?;

        let loader = self.create_function(|_, ()| Ok("\n\tcan't load C modules in safe mode"))?;

        // The third and fourth searchers looks for a loader as a C library
        searchers.raw_set(3, loader.clone())?;
        searchers.raw_remove(4)?;

        Ok(())
    }

    pub(crate) unsafe fn make_from_ptr(state: *mut ffi::lua_State) -> Self {
        let _sg = StackGuard::new(state);
        assert_stack(state, 1);

        let extra_key = &EXTRA_REGISTRY_KEY as *const u8 as *const c_void;
        ffi::lua_rawgetp(state, ffi::LUA_REGISTRYINDEX, extra_key);
        let extra = mlua_expect!(
            (*get_gc_userdata::<Weak<Mutex<ExtraData>>>(state, -1)).upgrade(),
            "extra is destroyed"
        );
        ffi::lua_pop(state, 1);

        let safe = mlua_expect!(extra.lock(), "extra is poisoned").safe;

        Lua {
            state,
            main_state: get_main_state(state),
            extra,
            ephemeral: true,
            safe,
            _no_ref_unwind_safe: PhantomData,
        }
    }

    pub(crate) unsafe fn hook_callback(&self) -> Option<HookCallback> {
        let extra = mlua_expect!(self.extra.lock(), "extra is poisoned");
        extra.hook_callback.clone()
    }
}

/// Returned from [`Lua::load`] and is used to finalize loading and executing Lua main chunks.
///
/// [`Lua::load`]: struct.Lua.html#method.load
#[must_use = "`Chunk`s do nothing unless one of `exec`, `eval`, `call`, or `into_function` are called on them"]
pub struct Chunk<'lua, 'a> {
    lua: &'lua Lua,
    source: &'a [u8],
    name: Option<CString>,
    env: Option<Value<'lua>>,
    mode: Option<ChunkMode>,
}

/// Represents chunk mode (text or binary).
#[derive(Clone, Copy, Debug)]
pub enum ChunkMode {
    Text,
    Binary,
}

impl<'lua, 'a> Chunk<'lua, 'a> {
    /// Sets the name of this chunk, which results in more informative error traces.
    pub fn set_name<S: AsRef<[u8]> + ?Sized>(mut self, name: &S) -> Result<Chunk<'lua, 'a>> {
        let name =
            CString::new(name.as_ref().to_vec()).map_err(|e| Error::ToLuaConversionError {
                from: "&str",
                to: "string",
                message: Some(e.to_string()),
            })?;
        self.name = Some(name);
        Ok(self)
    }

    /// Sets the first upvalue (`_ENV`) of the loaded chunk to the given value.
    ///
    /// Lua main chunks always have exactly one upvalue, and this upvalue is used as the `_ENV`
    /// variable inside the chunk. By default this value is set to the global environment.
    ///
    /// Calling this method changes the `_ENV` upvalue to the value provided, and variables inside
    /// the chunk will refer to the given environment rather than the global one.
    ///
    /// All global variables (including the standard library!) are looked up in `_ENV`, so it may be
    /// necessary to populate the environment in order for scripts using custom environments to be
    /// useful.
    pub fn set_environment<V: ToLua<'lua>>(mut self, env: V) -> Result<Chunk<'lua, 'a>> {
        self.env = Some(env.to_lua(self.lua)?);
        Ok(self)
    }

    /// Sets whether the chunk is text or binary (autodetected by default).
    ///
    /// Lua does not check the consistency of binary chunks, therefore this mode is allowed only
    /// for instances created with [`Lua::unsafe_new`].
    ///
    /// [`Lua::unsafe_new`]: struct.Lua.html#method.unsafe_new
    pub fn set_mode(mut self, mode: ChunkMode) -> Chunk<'lua, 'a> {
        self.mode = Some(mode);
        self
    }

    /// Execute this chunk of code.
    ///
    /// This is equivalent to calling the chunk function with no arguments and no return values.
    pub fn exec(self) -> Result<()> {
        self.call(())?;
        Ok(())
    }

    /// Asynchronously execute this chunk of code.
    ///
    /// See [`Chunk::exec`] for more details.
    ///
    /// Requires `feature = "async"`
    ///
    /// [`Chunk::exec`]: struct.Chunk.html#method.exec
    #[cfg(feature = "async")]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    pub fn exec_async<'fut>(self) -> LocalBoxFuture<'fut, Result<()>>
    where
        'lua: 'fut,
    {
        self.call_async(())
    }

    /// Evaluate the chunk as either an expression or block.
    ///
    /// If the chunk can be parsed as an expression, this loads and executes the chunk and returns
    /// the value that it evaluates to. Otherwise, the chunk is interpreted as a block as normal,
    /// and this is equivalent to calling `exec`.
    pub fn eval<R: FromLuaMulti<'lua>>(self) -> Result<R> {
        // Bytecode is always interpreted as a statement.
        // For source code, first try interpreting the lua as an expression by adding
        // "return", then as a statement. This is the same thing the
        // actual lua repl does.
        if self.source.starts_with(ffi::LUA_SIGNATURE) {
            self.call(())
        } else if let Ok(function) = self.lua.load_chunk(
            &self.expression_source(),
            self.name.as_ref(),
            self.env.clone(),
            self.mode,
        ) {
            function.call(())
        } else {
            self.call(())
        }
    }

    /// Asynchronously evaluate the chunk as either an expression or block.
    ///
    /// See [`Chunk::eval`] for more details.
    ///
    /// Requires `feature = "async"`
    ///
    /// [`Chunk::eval`]: struct.Chunk.html#method.eval
    #[cfg(feature = "async")]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    pub fn eval_async<'fut, R>(self) -> LocalBoxFuture<'fut, Result<R>>
    where
        'lua: 'fut,
        R: FromLuaMulti<'lua> + 'fut,
    {
        if self.source.starts_with(ffi::LUA_SIGNATURE) {
            self.call_async(())
        } else if let Ok(function) = self.lua.load_chunk(
            &self.expression_source(),
            self.name.as_ref(),
            self.env.clone(),
            self.mode,
        ) {
            function.call_async(())
        } else {
            self.call_async(())
        }
    }

    /// Load the chunk function and call it with the given arguemnts.
    ///
    /// This is equivalent to `into_function` and calling the resulting function.
    pub fn call<A: ToLuaMulti<'lua>, R: FromLuaMulti<'lua>>(self, args: A) -> Result<R> {
        self.into_function()?.call(args)
    }

    /// Load the chunk function and asynchronously call it with the given arguemnts.
    ///
    /// See [`Chunk::call`] for more details.
    ///
    /// Requires `feature = "async"`
    ///
    /// [`Chunk::call`]: struct.Chunk.html#method.call
    #[cfg(feature = "async")]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    pub fn call_async<'fut, A, R>(self, args: A) -> LocalBoxFuture<'fut, Result<R>>
    where
        'lua: 'fut,
        A: ToLuaMulti<'lua>,
        R: FromLuaMulti<'lua> + 'fut,
    {
        match self.into_function() {
            Ok(func) => func.call_async(args),
            Err(e) => Box::pin(future::err(e)),
        }
    }

    /// Load this chunk into a regular `Function`.
    ///
    /// This simply compiles the chunk without actually executing it.
    pub fn into_function(self) -> Result<Function<'lua>> {
        self.lua
            .load_chunk(self.source, self.name.as_ref(), self.env, self.mode)
    }

    fn expression_source(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(b"return ".len() + self.source.len());
        buf.extend(b"return ");
        buf.extend(self.source);
        buf
    }
}

unsafe fn load_from_std_lib(state: *mut ffi::lua_State, libs: StdLib) -> Result<()> {
    #[cfg(feature = "luajit")]
    // Stop collector during library initialization
    ffi::lua_gc(state, ffi::LUA_GCSTOP, 0);

    #[cfg(any(feature = "lua54", feature = "lua53", feature = "lua52"))]
    {
        if libs.contains(StdLib::COROUTINE) {
            ffi::safe::luaL_requiref(state, ffi::LUA_COLIBNAME, ffi::luaopen_coroutine, 1)?;
            ffi::lua_pop(state, 1);
        }
    }

    if libs.contains(StdLib::TABLE) {
        ffi::safe::luaL_requiref(state, ffi::LUA_TABLIBNAME, ffi::luaopen_table, 1)?;
        ffi::lua_pop(state, 1);
    }

    if libs.contains(StdLib::IO) {
        ffi::safe::luaL_requiref(state, ffi::LUA_IOLIBNAME, ffi::luaopen_io, 1)?;
        ffi::lua_pop(state, 1);
    }

    if libs.contains(StdLib::OS) {
        ffi::safe::luaL_requiref(state, ffi::LUA_OSLIBNAME, ffi::luaopen_os, 1)?;
        ffi::lua_pop(state, 1);
    }

    if libs.contains(StdLib::STRING) {
        ffi::safe::luaL_requiref(state, ffi::LUA_STRLIBNAME, ffi::luaopen_string, 1)?;
        ffi::lua_pop(state, 1);
    }

    #[cfg(any(feature = "lua54", feature = "lua53"))]
    {
        if libs.contains(StdLib::UTF8) {
            ffi::safe::luaL_requiref(state, ffi::LUA_UTF8LIBNAME, ffi::luaopen_utf8, 1)?;
            ffi::lua_pop(state, 1);
        }
    }

    #[cfg(feature = "lua52")]
    {
        if libs.contains(StdLib::BIT) {
            ffi::safe::luaL_requiref(state, ffi::LUA_BITLIBNAME, ffi::luaopen_bit32, 1)?;
            ffi::lua_pop(state, 1);
        }
    }

    #[cfg(feature = "luajit")]
    {
        if libs.contains(StdLib::BIT) {
            ffi::safe::luaL_requiref(state, ffi::LUA_BITLIBNAME, ffi::luaopen_bit, 1)?;
            ffi::lua_pop(state, 1);
        }
    }

    if libs.contains(StdLib::MATH) {
        ffi::safe::luaL_requiref(state, ffi::LUA_MATHLIBNAME, ffi::luaopen_math, 1)?;
        ffi::lua_pop(state, 1);
    }

    if libs.contains(StdLib::DEBUG) {
        ffi::safe::luaL_requiref(state, ffi::LUA_DBLIBNAME, ffi::luaopen_debug, 1)?;
        ffi::lua_pop(state, 1);
    }

    if libs.contains(StdLib::PACKAGE) {
        ffi::safe::luaL_requiref(state, ffi::LUA_LOADLIBNAME, ffi::luaopen_package, 1)?;
        ffi::lua_pop(state, 1);
    }

    #[cfg(feature = "luajit")]
    {
        if libs.contains(StdLib::JIT) {
            ffi::safe::luaL_requiref(state, ffi::LUA_JITLIBNAME, ffi::luaopen_jit, 1)?;
            ffi::lua_pop(state, 1);
        }

        if libs.contains(StdLib::FFI) {
            ffi::safe::luaL_requiref(state, ffi::LUA_FFILIBNAME, ffi::luaopen_ffi, 1)?;
            ffi::lua_pop(state, 1);
        }
    }

    #[cfg(feature = "luajit")]
    ffi::lua_gc(state, ffi::LUA_GCRESTART, -1);

    Ok(())
}

unsafe fn ref_stack_pop(mut extra: MutexGuard<ExtraData>) -> c_int {
    if let Some(free) = extra.ref_free.pop() {
        ffi::lua_replace(extra.ref_thread, free);
        return free;
    }

    // Try to grow max stack size
    if extra.ref_stack_top >= extra.ref_stack_size {
        let mut inc = extra.ref_stack_size; // Try to double stack size
        while inc > 0 && ffi::lua_checkstack(extra.ref_thread, inc) == 0 {
            inc /= 2;
        }
        if inc == 0 {
            // Pop item on top of the stack to avoid stack leaking and successfully run destructors
            // during unwinding.
            ffi::lua_pop(extra.ref_thread, 1);
            let top = extra.ref_stack_top;
            drop(extra);
            // It is a user error to create enough references to exhaust the Lua max stack size for
            // the ref thread.
            panic!(
                "cannot create a Lua reference, out of auxiliary stack space (used {} slots)",
                top
            );
        }
        extra.ref_stack_size += inc;
    }
    extra.ref_stack_top += 1;
    extra.ref_stack_top
}

struct StaticUserDataMethods<'lua, T: 'static + UserData> {
    methods: Vec<(Vec<u8>, Callback<'lua, 'static>)>,
    #[cfg(feature = "async")]
    async_methods: Vec<(Vec<u8>, AsyncCallback<'lua, 'static>)>,
    meta_methods: Vec<(MetaMethod, Callback<'lua, 'static>)>,
    _type: PhantomData<T>,
}

impl<'lua, T: 'static + UserData> Default for StaticUserDataMethods<'lua, T> {
    fn default() -> StaticUserDataMethods<'lua, T> {
        StaticUserDataMethods {
            methods: Vec::new(),
            #[cfg(feature = "async")]
            async_methods: Vec::new(),
            meta_methods: Vec::new(),
            _type: PhantomData,
        }
    }
}

impl<'lua, T: 'static + UserData> UserDataMethods<'lua, T> for StaticUserDataMethods<'lua, T> {
    fn add_method<S, A, R, M>(&mut self, name: &S, method: M)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + Fn(&'lua Lua, &T, A) -> Result<R>,
    {
        self.methods
            .push((name.as_ref().to_vec(), Self::box_method(method)));
    }

    fn add_method_mut<S, A, R, M>(&mut self, name: &S, method: M)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + FnMut(&'lua Lua, &mut T, A) -> Result<R>,
    {
        self.methods
            .push((name.as_ref().to_vec(), Self::box_method_mut(method)));
    }

    #[cfg(feature = "async")]
    fn add_async_method<S, A, R, M, MR>(&mut self, name: &S, method: M)
    where
        T: Clone,
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + Fn(&'lua Lua, T, A) -> MR,
        MR: 'lua + Future<Output = Result<R>>,
    {
        self.async_methods
            .push((name.as_ref().to_vec(), Self::box_async_method(method)));
    }

    fn add_function<S, A, R, F>(&mut self, name: &S, function: F)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua, A) -> Result<R>,
    {
        self.methods
            .push((name.as_ref().to_vec(), Self::box_function(function)));
    }

    fn add_function_mut<S, A, R, F>(&mut self, name: &S, function: F)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + FnMut(&'lua Lua, A) -> Result<R>,
    {
        self.methods
            .push((name.as_ref().to_vec(), Self::box_function_mut(function)));
    }

    #[cfg(feature = "async")]
    fn add_async_function<S, A, R, F, FR>(&mut self, name: &S, function: F)
    where
        T: Clone,
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua, A) -> FR,
        FR: 'lua + Future<Output = Result<R>>,
    {
        self.async_methods
            .push((name.as_ref().to_vec(), Self::box_async_function(function)));
    }

    fn add_meta_method<S, A, R, M>(&mut self, meta: S, method: M)
    where
        S: Into<MetaMethod>,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + Fn(&'lua Lua, &T, A) -> Result<R>,
    {
        self.meta_methods
            .push((meta.into(), Self::box_method(method)));
    }

    fn add_meta_method_mut<S, A, R, M>(&mut self, meta: S, method: M)
    where
        S: Into<MetaMethod>,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + FnMut(&'lua Lua, &mut T, A) -> Result<R>,
    {
        self.meta_methods
            .push((meta.into(), Self::box_method_mut(method)));
    }

    fn add_meta_function<S, A, R, F>(&mut self, meta: S, function: F)
    where
        S: Into<MetaMethod>,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua, A) -> Result<R>,
    {
        self.meta_methods
            .push((meta.into(), Self::box_function(function)));
    }

    fn add_meta_function_mut<S, A, R, F>(&mut self, meta: S, function: F)
    where
        S: Into<MetaMethod>,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + FnMut(&'lua Lua, A) -> Result<R>,
    {
        self.meta_methods
            .push((meta.into(), Self::box_function_mut(function)));
    }
}

impl<'lua, T: 'static + UserData> StaticUserDataMethods<'lua, T> {
    fn box_method<A, R, M>(method: M) -> Callback<'lua, 'static>
    where
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + Fn(&'lua Lua, &T, A) -> Result<R>,
    {
        Box::new(move |lua, mut args| {
            if let Some(front) = args.pop_front() {
                let userdata = AnyUserData::from_lua(front, lua)?;
                let userdata = userdata.borrow::<T>()?;
                method(lua, &userdata, A::from_lua_multi(args, lua)?)?.to_lua_multi(lua)
            } else {
                Err(Error::FromLuaConversionError {
                    from: "missing argument",
                    to: "userdata",
                    message: None,
                })
            }
        })
    }

    fn box_method_mut<A, R, M>(method: M) -> Callback<'lua, 'static>
    where
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + FnMut(&'lua Lua, &mut T, A) -> Result<R>,
    {
        let method = RefCell::new(method);
        Box::new(move |lua, mut args| {
            if let Some(front) = args.pop_front() {
                let userdata = AnyUserData::from_lua(front, lua)?;
                let mut userdata = userdata.borrow_mut::<T>()?;
                let mut method = method
                    .try_borrow_mut()
                    .map_err(|_| Error::RecursiveMutCallback)?;
                (&mut *method)(lua, &mut userdata, A::from_lua_multi(args, lua)?)?.to_lua_multi(lua)
            } else {
                Err(Error::FromLuaConversionError {
                    from: "missing argument",
                    to: "userdata",
                    message: None,
                })
            }
        })
    }

    #[cfg(feature = "async")]
    fn box_async_method<A, R, M, MR>(method: M) -> AsyncCallback<'lua, 'static>
    where
        T: Clone,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + Fn(&'lua Lua, T, A) -> MR,
        MR: 'lua + Future<Output = Result<R>>,
    {
        Box::new(move |lua, mut args| {
            let fut_res = || {
                if let Some(front) = args.pop_front() {
                    let userdata = AnyUserData::from_lua(front, lua)?;
                    let userdata = userdata.borrow::<T>()?.clone();
                    Ok(method(lua, userdata, A::from_lua_multi(args, lua)?))
                } else {
                    Err(Error::FromLuaConversionError {
                        from: "missing argument",
                        to: "userdata",
                        message: None,
                    })
                }
            };
            match fut_res() {
                Ok(fut) => Box::pin(fut.and_then(move |ret| future::ready(ret.to_lua_multi(lua)))),
                Err(e) => Box::pin(future::err(e)),
            }
        })
    }

    fn box_function<A, R, F>(function: F) -> Callback<'lua, 'static>
    where
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua, A) -> Result<R>,
    {
        Box::new(move |lua, args| function(lua, A::from_lua_multi(args, lua)?)?.to_lua_multi(lua))
    }

    fn box_function_mut<A, R, F>(function: F) -> Callback<'lua, 'static>
    where
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + FnMut(&'lua Lua, A) -> Result<R>,
    {
        let function = RefCell::new(function);
        Box::new(move |lua, args| {
            let function = &mut *function
                .try_borrow_mut()
                .map_err(|_| Error::RecursiveMutCallback)?;
            function(lua, A::from_lua_multi(args, lua)?)?.to_lua_multi(lua)
        })
    }

    #[cfg(feature = "async")]
    fn box_async_function<A, R, F, FR>(function: F) -> AsyncCallback<'lua, 'static>
    where
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua, A) -> FR,
        FR: 'lua + Future<Output = Result<R>>,
    {
        Box::new(move |lua, args| {
            let args = match A::from_lua_multi(args, lua) {
                Ok(args) => args,
                Err(e) => return Box::pin(future::err(e)),
            };
            Box::pin(function(lua, args).and_then(move |ret| future::ready(ret.to_lua_multi(lua))))
        })
    }
}

struct StaticUserDataFields<'lua, T: 'static + UserData> {
    field_getters: Vec<(Vec<u8>, Callback<'lua, 'static>)>,
    field_setters: Vec<(Vec<u8>, Callback<'lua, 'static>)>,
    #[allow(clippy::type_complexity)]
    meta_fields: Vec<(
        MetaMethod,
        Box<dyn Fn(&'lua Lua) -> Result<Value<'lua>> + 'static>,
    )>,
    _type: PhantomData<T>,
}

impl<'lua, T: 'static + UserData> Default for StaticUserDataFields<'lua, T> {
    fn default() -> StaticUserDataFields<'lua, T> {
        StaticUserDataFields {
            field_getters: Vec::new(),
            field_setters: Vec::new(),
            meta_fields: Vec::new(),
            _type: PhantomData,
        }
    }
}

impl<'lua, T: 'static + UserData> UserDataFields<'lua, T> for StaticUserDataFields<'lua, T> {
    fn add_field_method_get<S, R, M>(&mut self, name: &S, method: M)
    where
        S: AsRef<[u8]> + ?Sized,
        R: ToLua<'lua>,
        M: 'static + MaybeSend + Fn(&'lua Lua, &T) -> Result<R>,
    {
        self.field_getters.push((
            name.as_ref().to_vec(),
            StaticUserDataMethods::box_method(move |lua, data, ()| method(lua, data)),
        ));
    }

    fn add_field_method_set<S, A, M>(&mut self, name: &S, method: M)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLua<'lua>,
        M: 'static + MaybeSend + FnMut(&'lua Lua, &mut T, A) -> Result<()>,
    {
        self.field_setters.push((
            name.as_ref().to_vec(),
            StaticUserDataMethods::box_method_mut(method),
        ));
    }

    fn add_field_function_get<S, R, F>(&mut self, name: &S, function: F)
    where
        S: AsRef<[u8]> + ?Sized,
        R: ToLua<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua, AnyUserData<'lua>) -> Result<R>,
    {
        self.field_getters.push((
            name.as_ref().to_vec(),
            StaticUserDataMethods::<T>::box_function(move |lua, data| function(lua, data)),
        ));
    }

    fn add_field_function_set<S, A, F>(&mut self, name: &S, mut function: F)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLua<'lua>,
        F: 'static + MaybeSend + FnMut(&'lua Lua, AnyUserData<'lua>, A) -> Result<()>,
    {
        self.field_setters.push((
            name.as_ref().to_vec(),
            StaticUserDataMethods::<T>::box_function_mut(move |lua, (data, val)| {
                function(lua, data, val)
            }),
        ));
    }

    fn add_meta_field_with<S, R, F>(&mut self, meta: S, f: F)
    where
        S: Into<MetaMethod>,
        R: ToLua<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua) -> Result<R>,
    {
        let meta = meta.into();
        self.meta_fields.push((
            meta.clone(),
            Box::new(move |lua| {
                let value = f(lua)?.to_lua(lua)?;
                if meta == MetaMethod::Index || meta == MetaMethod::NewIndex {
                    match value {
                        Value::Nil | Value::Table(_) | Value::Function(_) => {}
                        _ => {
                            return Err(Error::MetaMethodTypeError {
                                method: meta.to_string(),
                                type_name: value.type_name(),
                                message: Some("expected nil, table or function".to_string()),
                            })
                        }
                    }
                }
                Ok(value)
            }),
        ));
    }
}
