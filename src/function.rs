use std::ffi::CStr;
use std::os::raw::c_int;
use std::ptr;

use crate::error::{Error, Result};
use crate::ffi;
use crate::types::LuaRef;
use crate::util::{
    assert_stack, check_stack, error_traceback, pop_error, protect_lua_closure, StackGuard,
};
use crate::value::{FromLuaMulti, MultiValue, ToLuaMulti};

#[cfg(feature = "async")]
use {futures_core::future::LocalBoxFuture, futures_util::future};

/// Handle to an internal Lua function.
#[derive(Clone, Debug)]
pub struct Function<'lua>(pub(crate) LuaRef<'lua>);

impl<'lua> Function<'lua> {
    /// Calls the function, passing `args` as function arguments.
    ///
    /// The function's return values are converted to the generic type `R`.
    ///
    /// # Examples
    ///
    /// Call Lua's built-in `tostring` function:
    ///
    /// ```
    /// # use mlua::{Function, Lua, Result};
    /// # fn main() -> Result<()> {
    /// # let lua = Lua::new();
    /// let globals = lua.globals();
    ///
    /// let tostring: Function = globals.get("tostring")?;
    ///
    /// assert_eq!(tostring.call::<_, String>(123)?, "123");
    ///
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Call a function with multiple arguments:
    ///
    /// ```
    /// # use mlua::{Function, Lua, Result};
    /// # fn main() -> Result<()> {
    /// # let lua = Lua::new();
    /// let sum: Function = lua.load(
    ///     r#"
    ///         function(a, b)
    ///             return a + b
    ///         end
    /// "#).eval()?;
    ///
    /// assert_eq!(sum.call::<_, u32>((3, 4))?, 3 + 4);
    ///
    /// # Ok(())
    /// # }
    /// ```
    pub fn call<A: ToLuaMulti<'lua>, R: FromLuaMulti<'lua>>(&self, args: A) -> Result<R> {
        let lua = self.0.lua;

        let args = args.to_lua_multi(lua)?;
        let nargs = args.len() as c_int;

        let results = unsafe {
            let _sg = StackGuard::new(lua.state);
            check_stack(lua.state, nargs + 3)?;

            ffi::lua_pushcfunction(lua.state, error_traceback);
            let stack_start = ffi::lua_gettop(lua.state);
            lua.push_ref(&self.0);
            for arg in args {
                lua.push_value(arg)?;
            }
            let ret = ffi::lua_pcall(lua.state, nargs, ffi::LUA_MULTRET, stack_start);
            if ret != ffi::LUA_OK {
                return Err(pop_error(lua.state, ret));
            }
            let nresults = ffi::lua_gettop(lua.state) - stack_start;
            let mut results = MultiValue::new();
            assert_stack(lua.state, 2);
            for _ in 0..nresults {
                results.push_front(lua.pop_value());
            }
            ffi::lua_pop(lua.state, 1);
            results
        };
        R::from_lua_multi(results, lua)
    }

    /// Returns a Feature that, when polled, calls `self`, passing `args` as function arguments,
    /// and drives the execution.
    ///
    /// Internaly it wraps the function to an [`AsyncThread`].
    ///
    /// Requires `feature = "async"`
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use futures_timer::Delay;
    /// # use mlua::{Lua, Result};
    /// # #[tokio::main]
    /// # async fn main() -> Result<()> {
    /// # let lua = Lua::new();
    ///
    /// let sleep = lua.create_async_function(move |_lua, n: u64| async move {
    ///     Delay::new(Duration::from_millis(n)).await;
    ///     Ok(())
    /// })?;
    ///
    /// sleep.call_async(10).await?;
    ///
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// [`AsyncThread`]: struct.AsyncThread.html
    #[cfg(feature = "async")]
    pub fn call_async<'fut, A, R>(&self, args: A) -> LocalBoxFuture<'fut, Result<R>>
    where
        'lua: 'fut,
        A: ToLuaMulti<'lua>,
        R: FromLuaMulti<'lua> + 'fut,
    {
        let lua = self.0.lua;
        match lua.create_thread(self.clone()) {
            Ok(t) => Box::pin(t.into_async(args)),
            Err(e) => Box::pin(future::err(e)),
        }
    }

    /// Returns a function that, when called, calls `self`, passing `args` as the first set of
    /// arguments.
    ///
    /// If any arguments are passed to the returned function, they will be passed after `args`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use mlua::{Function, Lua, Result};
    /// # fn main() -> Result<()> {
    /// # let lua = Lua::new();
    /// let sum: Function = lua.load(
    ///     r#"
    ///         function(a, b)
    ///             return a + b
    ///         end
    /// "#).eval()?;
    ///
    /// let bound_a = sum.bind(1)?;
    /// assert_eq!(bound_a.call::<_, u32>(2)?, 1 + 2);
    ///
    /// let bound_a_and_b = sum.bind(13)?.bind(57)?;
    /// assert_eq!(bound_a_and_b.call::<_, u32>(())?, 13 + 57);
    ///
    /// # Ok(())
    /// # }
    /// ```
    pub fn bind<A: ToLuaMulti<'lua>>(&self, args: A) -> Result<Function<'lua>> {
        unsafe extern "C" fn bind_call_impl(state: *mut ffi::lua_State) -> c_int {
            let nargs = ffi::lua_gettop(state);
            let nbinds = ffi::lua_tointeger(state, ffi::lua_upvalueindex(2)) as c_int;
            ffi::luaL_checkstack(state, nbinds + 2, ptr::null());

            ffi::lua_settop(state, nargs + nbinds + 1);
            ffi::lua_rotate(state, -(nargs + nbinds + 1), nbinds + 1);

            ffi::lua_pushvalue(state, ffi::lua_upvalueindex(1));
            ffi::lua_replace(state, 1);

            for i in 0..nbinds {
                ffi::lua_pushvalue(state, ffi::lua_upvalueindex(i + 3));
                ffi::lua_replace(state, i + 2);
            }

            ffi::lua_call(state, nargs + nbinds, ffi::LUA_MULTRET);
            ffi::lua_gettop(state)
        }

        let lua = self.0.lua;

        let args = args.to_lua_multi(lua)?;
        let nargs = args.len() as c_int;

        if nargs + 2 > ffi::LUA_MAX_UPVALUES {
            return Err(Error::BindError);
        }

        unsafe {
            let _sg = StackGuard::new(lua.state);
            check_stack(lua.state, nargs + 5)?;
            lua.push_ref(&self.0);
            ffi::lua_pushinteger(lua.state, nargs as ffi::lua_Integer);
            for arg in args {
                lua.push_value(arg)?;
            }

            protect_lua_closure(lua.state, nargs + 2, 1, |state| {
                ffi::lua_pushcclosure(state, bind_call_impl, nargs + 2);
            })?;

            Ok(Function(lua.pop_ref()))
        }
    }

    /// Returns source and line number information about the function.
    ///
    /// Corresponds to the `>S` what mask for `lua_getinfo` when applied
    /// to the function.
    pub fn source(&self) -> FunctionSource {
        let lua = self.0.lua;
        unsafe {
            let mut ar: ffi::lua_Debug = std::mem::zeroed();
            lua.push_ref(&self.0);
            mlua_assert!(
                ffi::lua_getinfo(lua.state, cstr!(">S"), &mut ar as *mut ffi::lua_Debug) != 0,
                "lua_getinfo failed with `>S`"
            );
            FunctionSource {
                source: String::from_utf8_lossy(CStr::from_ptr(ar.source as *const i8).to_bytes()).into_owned(),
                short_src: String::from_utf8_lossy(CStr::from_ptr(&ar.short_src as *const i8).to_bytes()).into_owned(),
                line_defined: ar.linedefined as i32,
                last_line_defined: ar.lastlinedefined as i32,
                what: String::from_utf8_lossy(CStr::from_ptr(ar.what as *const i8).to_bytes()).into_owned(),
            }
        }
    }
}

impl<'lua> PartialEq for Function<'lua> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

#[derive(Clone, Debug)]
pub struct FunctionSource {
    pub source: String,
    pub short_src: String,
    pub line_defined: i32,
    pub last_line_defined: i32,
    pub what: String,
}
