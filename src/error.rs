use std::error::Error as StdError;
use std::fmt;
use std::io::Error as IoError;
use std::net::AddrParseError;
use std::result::Result as StdResult;
use std::str::Utf8Error;
use std::string::String as StdString;
use std::sync::Arc;

/// Error type returned by `mlua` methods.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Error {
    /// Syntax error while parsing Lua source code.
    SyntaxError {
        /// The error message as returned by Lua.
        message: StdString,
        /// `true` if the error can likely be fixed by appending more input to the source code.
        ///
        /// This is useful for implementing REPLs as they can query the user for more input if this
        /// is set.
        incomplete_input: bool,
    },
    /// Lua runtime error, aka `LUA_ERRRUN`.
    ///
    /// The Lua VM returns this error when a builtin operation is performed on incompatible types.
    /// Among other things, this includes invoking operators on wrong types (such as calling or
    /// indexing a `nil` value).
    RuntimeError(StdString),
    /// Lua memory error, aka `LUA_ERRMEM`
    ///
    /// The Lua VM returns this error when the allocator does not return the requested memory, aka
    /// it is an out-of-memory error.
    MemoryError(StdString),
    /// Lua garbage collector error, aka `LUA_ERRGCMM`.
    ///
    /// The Lua VM returns this error when there is an error running a `__gc` metamethod.
    #[cfg(any(feature = "lua53", feature = "lua52"))]
    GarbageCollectorError(StdString),
    /// Potentially unsafe action in safe mode.
    SafetyError(StdString),
    /// Setting memory limit is not available.
    ///
    /// This error can only happen when Lua state was not created by us and does not have the
    /// custom allocator attached.
    MemoryLimitNotAvailable,
    /// Main thread is not available.
    ///
    /// This error can only happen in Lua5.1/LuaJIT module mode, when module loaded within a coroutine.
    /// These Lua versions does not have `LUA_RIDX_MAINTHREAD` registry key.
    MainThreadNotAvailable,
    /// A mutable callback has triggered Lua code that has called the same mutable callback again.
    ///
    /// This is an error because a mutable callback can only be borrowed mutably once.
    RecursiveMutCallback,
    /// Either a callback or a userdata method has been called, but the callback or userdata has
    /// been destructed.
    ///
    /// This can happen either due to to being destructed in a previous __gc, or due to being
    /// destructed from exiting a `Lua::scope` call.
    CallbackDestructed,
    /// Not enough stack space to place arguments to Lua functions or return values from callbacks.
    ///
    /// Due to the way `mlua` works, it should not be directly possible to run out of stack space
    /// during normal use. The only way that this error can be triggered is if a `Function` is
    /// called with a huge number of arguments, or a rust callback returns a huge number of return
    /// values.
    StackError,
    /// Too many arguments to `Function::bind`
    BindError,
    /// A Rust value could not be converted to a Lua value.
    ToLuaConversionError {
        /// Name of the Rust type that could not be converted.
        from: &'static str,
        /// Name of the Lua type that could not be created.
        to: &'static str,
        /// A message indicating why the conversion failed in more detail.
        message: Option<StdString>,
    },
    /// A Lua value could not be converted to the expected Rust type.
    FromLuaConversionError {
        /// Name of the Lua type that could not be converted.
        from: &'static str,
        /// Name of the Rust type that could not be created.
        to: &'static str,
        /// A string containing more detailed error information.
        message: Option<StdString>,
    },
    /// [`Thread::resume`] was called on an inactive coroutine.
    ///
    /// A coroutine is inactive if its main function has returned or if an error has occured inside
    /// the coroutine.
    ///
    /// [`Thread::status`] can be used to check if the coroutine can be resumed without causing this
    /// error.
    ///
    /// [`Thread::resume`]: struct.Thread.html#method.resume
    /// [`Thread::status`]: struct.Thread.html#method.status
    CoroutineInactive,
    /// An [`AnyUserData`] is not the expected type in a borrow.
    ///
    /// This error can only happen when manually using [`AnyUserData`], or when implementing
    /// metamethods for binary operators. Refer to the documentation of [`UserDataMethods`] for
    /// details.
    ///
    /// [`AnyUserData`]: struct.AnyUserData.html
    /// [`UserDataMethods`]: trait.UserDataMethods.html
    UserDataTypeMismatch,
    /// An [`AnyUserData`] borrow failed because it has been destructed.
    ///
    /// This error can happen either due to to being destructed in a previous __gc, or due to being
    /// destructed from exiting a `Lua::scope` call.
    ///
    /// [`AnyUserData`]: struct.AnyUserData.html
    UserDataDestructed,
    /// An [`AnyUserData`] immutable borrow failed because it is already borrowed mutably.
    ///
    /// This error can occur when a method on a [`UserData`] type calls back into Lua, which then
    /// tries to call a method on the same [`UserData`] type. Consider restructuring your API to
    /// prevent these errors.
    ///
    /// [`AnyUserData`]: struct.AnyUserData.html
    /// [`UserData`]: trait.UserData.html
    UserDataBorrowError,
    /// An [`AnyUserData`] mutable borrow failed because it is already borrowed.
    ///
    /// This error can occur when a method on a [`UserData`] type calls back into Lua, which then
    /// tries to call a method on the same [`UserData`] type. Consider restructuring your API to
    /// prevent these errors.
    ///
    /// [`AnyUserData`]: struct.AnyUserData.html
    /// [`UserData`]: trait.UserData.html
    UserDataBorrowMutError,
    /// A [`MetaMethod`] operation is restricted (typically for `__gc` or `__metatable`).
    ///
    /// [`MetaMethod`]: enum.MetaMethod.html
    MetaMethodRestricted(StdString),
    /// A [`MetaMethod`] (eg. `__index` or `__newindex`) has invalid type.
    ///
    /// [`MetaMethod`]: enum.MetaMethod.html
    MetaMethodTypeError {
        method: StdString,
        type_name: &'static str,
        message: Option<StdString>,
    },
    /// A `RegistryKey` produced from a different Lua state was used.
    MismatchedRegistryKey,
    /// A Rust callback returned `Err`, raising the contained `Error` as a Lua error.
    CallbackError {
        /// Lua call stack backtrace.
        traceback: StdString,
        /// Original error returned by the Rust code.
        cause: Arc<Error>,
    },
    /// A Rust panic that was previosly resumed, returned again.
    ///
    /// This error can occur only when a Rust panic resumed previously was recovered
    /// and returned again.
    PreviouslyResumedPanic,
    /// Serialization error.
    #[cfg(feature = "serialize")]
    #[cfg_attr(docsrs, doc(cfg(feature = "serialize")))]
    SerializeError(StdString),
    /// Deserialization error.
    #[cfg(feature = "serialize")]
    #[cfg_attr(docsrs, doc(cfg(feature = "serialize")))]
    DeserializeError(StdString),
    /// A custom error.
    ///
    /// This can be used for returning user-defined errors from callbacks.
    ///
    /// Returning `Err(ExternalError(...))` from a Rust callback will raise the error as a Lua
    /// error. The Rust code that originally invoked the Lua code then receives a `CallbackError`,
    /// from which the original error (and a stack traceback) can be recovered.
    #[cfg(feature = "send")]
    ExternalError(Arc<dyn StdError + Send + Sync>),
    #[cfg(not(feature = "send"))]
    ExternalError(Arc<dyn StdError>),
}

/// A specialized `Result` type used by `mlua`'s API.
pub type Result<T> = StdResult<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::SyntaxError { ref message, .. } => write!(fmt, "syntax error: {}", message),
            Error::RuntimeError(ref msg) => write!(fmt, "runtime error: {}", msg),
            Error::MemoryError(ref msg) => {
                write!(fmt, "memory error: {}", msg)
            }
            #[cfg(any(feature = "lua53", feature = "lua52"))]
            Error::GarbageCollectorError(ref msg) => {
                write!(fmt, "garbage collector error: {}", msg)
            }
            Error::SafetyError(ref msg) => {
                write!(fmt, "safety error: {}", msg)
            },
            Error::MemoryLimitNotAvailable => {
                write!(fmt, "setting memory limit is not available")
            }
            Error::MainThreadNotAvailable => {
                write!(fmt, "main thread is not available in Lua 5.1")
            }
            Error::RecursiveMutCallback => write!(fmt, "mutable callback called recursively"),
            Error::CallbackDestructed => write!(
                fmt,
                "a destructed callback or destructed userdata method was called"
            ),
            Error::StackError => write!(
                fmt,
                "out of Lua stack, too many arguments to a Lua function or too many return values from a callback"
            ),
            Error::BindError => write!(
                fmt,
                "too many arguments to Function::bind"
            ),
            Error::ToLuaConversionError { from, to, ref message } => {
                write!(fmt, "error converting {} to Lua {}", from, to)?;
                match *message {
                    None => Ok(()),
                    Some(ref message) => write!(fmt, " ({})", message),
                }
            }
            Error::FromLuaConversionError { from, to, ref message } => {
                write!(fmt, "error converting Lua {} to {}", from, to)?;
                match *message {
                    None => Ok(()),
                    Some(ref message) => write!(fmt, " ({})", message),
                }
            }
            Error::CoroutineInactive => write!(fmt, "cannot resume inactive coroutine"),
            Error::UserDataTypeMismatch => write!(fmt, "userdata is not expected type"),
            Error::UserDataDestructed => write!(fmt, "userdata has been destructed"),
            Error::UserDataBorrowError => write!(fmt, "userdata already mutably borrowed"),
            Error::UserDataBorrowMutError => write!(fmt, "userdata already borrowed"),
            Error::MetaMethodRestricted(ref method) => write!(fmt, "metamethod {} is restricted", method),
            Error::MetaMethodTypeError { ref method, type_name, ref message } => {
                write!(fmt, "metamethod {} has unsupported type {}", method, type_name)?;
                match *message {
                    None => Ok(()),
                    Some(ref message) => write!(fmt, " ({})", message),
                }
            }
            Error::MismatchedRegistryKey => {
                write!(fmt, "RegistryKey used from different Lua state")
            }
            Error::CallbackError { ref traceback, ref cause } => {
                write!(fmt, "callback error: {}\n{}", cause, traceback)
            }
            Error::PreviouslyResumedPanic => {
                write!(fmt, "previously resumed panic returned again")
            }
            #[cfg(feature = "serialize")]
            Error::SerializeError(ref err) => {
                write!(fmt, "serialize error: {}", err)
            },
            #[cfg(feature = "serialize")]
            Error::DeserializeError(ref err) => {
                write!(fmt, "deserialize error: {}", err)
            },
            Error::ExternalError(ref err) => write!(fmt, "{}", err),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match *self {
            Error::CallbackError { ref cause, .. } => Some(cause.as_ref()),
            Error::ExternalError(ref err) => err.source(),
            _ => None,
        }
    }
}

impl Error {
    #[cfg(feature = "send")]
    pub fn external<T: Into<Box<dyn StdError + Send + Sync>>>(err: T) -> Error {
        Error::ExternalError(err.into().into())
    }

    #[cfg(not(feature = "send"))]
    pub fn external<T: Into<Box<dyn StdError>>>(err: T) -> Error {
        Error::ExternalError(err.into().into())
    }
}

pub trait ExternalError {
    fn to_lua_err(self) -> Error;
}

#[cfg(feature = "send")]
impl<E: Into<Box<dyn StdError + Send + Sync>>> ExternalError for E {
    fn to_lua_err(self) -> Error {
        Error::external(self)
    }
}

#[cfg(not(feature = "send"))]
impl<E: Into<Box<dyn StdError>>> ExternalError for E {
    fn to_lua_err(self) -> Error {
        Error::external(self)
    }
}

pub trait ExternalResult<T> {
    fn to_lua_err(self) -> Result<T>;
}

impl<T, E> ExternalResult<T> for StdResult<T, E>
where
    E: ExternalError,
{
    fn to_lua_err(self) -> Result<T> {
        self.map_err(|e| e.to_lua_err())
    }
}

impl std::convert::From<AddrParseError> for Error {
    fn from(err: AddrParseError) -> Self {
        Error::external(err)
    }
}

impl std::convert::From<IoError> for Error {
    fn from(err: IoError) -> Self {
        Error::external(err)
    }
}

impl std::convert::From<Utf8Error> for Error {
    fn from(err: Utf8Error) -> Self {
        Error::external(err)
    }
}

#[cfg(feature = "serialize")]
impl serde::ser::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self::SerializeError(msg.to_string())
    }
}

#[cfg(feature = "serialize")]
impl serde::de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self::DeserializeError(msg.to_string())
    }
}
