#![allow(clippy::wrong_self_convention)]

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::hash::{BuildHasher, Hash};
use std::string::String as StdString;

use bstr::{BStr, BString};
use num_traits::cast;

use crate::error::{Error, Result};
use crate::function::Function;
use crate::lua::Lua;
use crate::string::String;
use crate::table::Table;
use crate::thread::Thread;
use crate::types::{LightUserData, MaybeSend};
use crate::userdata::{AnyUserData, UserData};
use crate::value::{FromLua, Nil, ToLua, Value};

impl<'lua> ToLua<'lua> for Value<'lua> {
    fn to_lua(self, _: &'lua Lua) -> Result<Value<'lua>> {
        Ok(self)
    }
}

impl<'lua> FromLua<'lua> for Value<'lua> {
    fn from_lua(lua_value: Value<'lua>, _: &'lua Lua) -> Result<Self> {
        Ok(lua_value)
    }
}

impl<'lua> ToLua<'lua> for () {
    fn to_lua(self, _: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::Nil)
    }
}

impl<'lua> ToLua<'lua> for String<'lua> {
    fn to_lua(self, _: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::String(self))
    }
}

impl<'lua> FromLua<'lua> for String<'lua> {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> Result<String<'lua>> {
        let ty = value.type_name();
        lua.coerce_string(value)?
            .ok_or_else(|| Error::FromLuaConversionError {
                from: ty,
                to: "String",
                message: Some("expected string or number".to_string()),
            })
    }
}

impl<'lua> ToLua<'lua> for Table<'lua> {
    fn to_lua(self, _: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::Table(self))
    }
}

impl<'lua> FromLua<'lua> for Table<'lua> {
    fn from_lua(value: Value<'lua>, _: &'lua Lua) -> Result<Table<'lua>> {
        match value {
            Value::Table(table) => Ok(table),
            _ => Err(Error::FromLuaConversionError {
                from: value.type_name(),
                to: "table",
                message: None,
            }),
        }
    }
}

impl<'lua> ToLua<'lua> for Function<'lua> {
    fn to_lua(self, _: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::Function(self))
    }
}

impl<'lua> FromLua<'lua> for Function<'lua> {
    fn from_lua(value: Value<'lua>, _: &'lua Lua) -> Result<Function<'lua>> {
        match value {
            Value::Function(table) => Ok(table),
            _ => Err(Error::FromLuaConversionError {
                from: value.type_name(),
                to: "function",
                message: None,
            }),
        }
    }
}

impl<'lua> ToLua<'lua> for Thread<'lua> {
    fn to_lua(self, _: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::Thread(self))
    }
}

impl<'lua> FromLua<'lua> for Thread<'lua> {
    fn from_lua(value: Value<'lua>, _: &'lua Lua) -> Result<Thread<'lua>> {
        match value {
            Value::Thread(t) => Ok(t),
            _ => Err(Error::FromLuaConversionError {
                from: value.type_name(),
                to: "thread",
                message: None,
            }),
        }
    }
}

impl<'lua> ToLua<'lua> for AnyUserData<'lua> {
    fn to_lua(self, _: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::UserData(self))
    }
}

impl<'lua> FromLua<'lua> for AnyUserData<'lua> {
    fn from_lua(value: Value<'lua>, _: &'lua Lua) -> Result<AnyUserData<'lua>> {
        match value {
            Value::UserData(ud) => Ok(ud),
            _ => Err(Error::FromLuaConversionError {
                from: value.type_name(),
                to: "userdata",
                message: None,
            }),
        }
    }
}

impl<'lua, T: 'static + MaybeSend + UserData> ToLua<'lua> for T {
    fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::UserData(lua.create_userdata(self)?))
    }
}

impl<'lua, T: 'static + UserData + Clone> FromLua<'lua> for T {
    fn from_lua(value: Value<'lua>, _: &'lua Lua) -> Result<T> {
        match value {
            Value::UserData(ud) => Ok(ud.borrow::<T>()?.clone()),
            _ => Err(Error::FromLuaConversionError {
                from: value.type_name(),
                to: "userdata",
                message: None,
            }),
        }
    }
}

impl<'lua> ToLua<'lua> for Error {
    fn to_lua(self, _: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::Error(self))
    }
}

impl<'lua> FromLua<'lua> for Error {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> Result<Error> {
        match value {
            Value::Error(err) => Ok(err),
            val => Ok(Error::RuntimeError(
                lua.coerce_string(val)?
                    .and_then(|s| Some(s.to_str().ok()?.to_owned()))
                    .unwrap_or_else(|| "<unprintable error>".to_owned()),
            )),
        }
    }
}

impl<'lua> ToLua<'lua> for bool {
    fn to_lua(self, _: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::Boolean(self))
    }
}

impl<'lua> FromLua<'lua> for bool {
    fn from_lua(v: Value<'lua>, _: &'lua Lua) -> Result<Self> {
        match v {
            Value::Nil => Ok(false),
            Value::Boolean(b) => Ok(b),
            _ => Ok(true),
        }
    }
}

impl<'lua> ToLua<'lua> for LightUserData {
    fn to_lua(self, _: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::LightUserData(self))
    }
}

impl<'lua> FromLua<'lua> for LightUserData {
    fn from_lua(value: Value<'lua>, _: &'lua Lua) -> Result<Self> {
        match value {
            Value::LightUserData(ud) => Ok(ud),
            _ => Err(Error::FromLuaConversionError {
                from: value.type_name(),
                to: "light userdata",
                message: None,
            }),
        }
    }
}

impl<'lua> ToLua<'lua> for StdString {
    fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::String(lua.create_string(&self)?))
    }
}

impl<'lua> FromLua<'lua> for StdString {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> Result<Self> {
        let ty = value.type_name();
        Ok(lua
            .coerce_string(value)?
            .ok_or_else(|| Error::FromLuaConversionError {
                from: ty,
                to: "String",
                message: Some("expected string or number".to_string()),
            })?
            .to_str()?
            .to_owned())
    }
}

impl<'lua> ToLua<'lua> for &str {
    fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::String(lua.create_string(self)?))
    }
}

impl<'lua> ToLua<'lua> for Cow<'_, str> {
    fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::String(lua.create_string(self.as_bytes())?))
    }
}

impl<'lua> ToLua<'lua> for Box<str> {
    fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::String(lua.create_string(&*self)?))
    }
}

impl<'lua> FromLua<'lua> for Box<str> {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> Result<Self> {
        let ty = value.type_name();
        Ok(lua
            .coerce_string(value)?
            .ok_or_else(|| Error::FromLuaConversionError {
                from: ty,
                to: "Box<str>",
                message: Some("expected string or number".to_string()),
            })?
            .to_str()?
            .to_owned()
            .into_boxed_str())
    }
}

impl<'lua> ToLua<'lua> for CString {
    fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::String(lua.create_string(self.as_bytes())?))
    }
}

impl<'lua> FromLua<'lua> for CString {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> Result<Self> {
        let ty = value.type_name();
        let string = lua
            .coerce_string(value)?
            .ok_or_else(|| Error::FromLuaConversionError {
                from: ty,
                to: "CString",
                message: Some("expected string or number".to_string()),
            })?;

        match CStr::from_bytes_with_nul(string.as_bytes_with_nul()) {
            Ok(s) => Ok(s.into()),
            Err(_) => Err(Error::FromLuaConversionError {
                from: ty,
                to: "CString",
                message: Some("invalid C-style string".to_string()),
            }),
        }
    }
}

impl<'lua> ToLua<'lua> for &CStr {
    fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::String(lua.create_string(self.to_bytes())?))
    }
}

impl<'lua> ToLua<'lua> for Cow<'_, CStr> {
    fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::String(lua.create_string(self.to_bytes())?))
    }
}

impl<'lua> ToLua<'lua> for BString {
    fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::String(lua.create_string(&self)?))
    }
}

impl<'lua> FromLua<'lua> for BString {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> Result<Self> {
        let ty = value.type_name();
        Ok(BString::from(
            lua.coerce_string(value)?
                .ok_or_else(|| Error::FromLuaConversionError {
                    from: ty,
                    to: "String",
                    message: Some("expected string or number".to_string()),
                })?
                .as_bytes()
                .to_vec(),
        ))
    }
}

impl<'lua> ToLua<'lua> for &BStr {
    fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::String(lua.create_string(&self)?))
    }
}

macro_rules! lua_convert_int {
    ($x:ty) => {
        impl<'lua> ToLua<'lua> for $x {
            fn to_lua(self, _: &'lua Lua) -> Result<Value<'lua>> {
                if let Some(i) = cast(self) {
                    Ok(Value::Integer(i))
                } else {
                    cast(self)
                        .ok_or_else(|| Error::ToLuaConversionError {
                            from: stringify!($x),
                            to: "number",
                            message: Some("out of range".to_owned()),
                        })
                        .map(Value::Number)
                }
            }
        }

        impl<'lua> FromLua<'lua> for $x {
            fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> Result<Self> {
                let ty = value.type_name();
                (if let Some(i) = lua.coerce_integer(value.clone())? {
                    cast(i)
                } else {
                    cast(lua.coerce_number(value)?.ok_or_else(|| {
                        Error::FromLuaConversionError {
                            from: ty,
                            to: stringify!($x),
                            message: Some(
                                "expected number or string coercible to number".to_string(),
                            ),
                        }
                    })?)
                })
                .ok_or_else(|| Error::FromLuaConversionError {
                    from: ty,
                    to: stringify!($x),
                    message: Some("out of range".to_owned()),
                })
            }
        }
    };
}

lua_convert_int!(i8);
lua_convert_int!(u8);
lua_convert_int!(i16);
lua_convert_int!(u16);
lua_convert_int!(i32);
lua_convert_int!(u32);
lua_convert_int!(i64);
lua_convert_int!(u64);
lua_convert_int!(i128);
lua_convert_int!(u128);
lua_convert_int!(isize);
lua_convert_int!(usize);

macro_rules! lua_convert_float {
    ($x:ty) => {
        impl<'lua> ToLua<'lua> for $x {
            fn to_lua(self, _: &'lua Lua) -> Result<Value<'lua>> {
                cast(self)
                    .ok_or_else(|| Error::ToLuaConversionError {
                        from: stringify!($x),
                        to: "number",
                        message: Some("out of range".to_string()),
                    })
                    .map(Value::Number)
            }
        }

        impl<'lua> FromLua<'lua> for $x {
            fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> Result<Self> {
                let ty = value.type_name();
                lua.coerce_number(value)?
                    .ok_or_else(|| Error::FromLuaConversionError {
                        from: ty,
                        to: stringify!($x),
                        message: Some("expected number or string coercible to number".to_string()),
                    })
                    .and_then(|n| {
                        cast(n).ok_or_else(|| Error::FromLuaConversionError {
                            from: ty,
                            to: stringify!($x),
                            message: Some("number out of range".to_string()),
                        })
                    })
            }
        }
    };
}

lua_convert_float!(f32);
lua_convert_float!(f64);

impl<'lua, T> ToLua<'lua> for &[T]
where
    T: Clone + ToLua<'lua>,
{
    fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::Table(
            lua.create_sequence_from(self.iter().cloned())?,
        ))
    }
}

macro_rules! lua_convert_array {
    ($($N:literal)+) => {
        $(
            impl<'lua, T> ToLua<'lua> for [T; $N]
            where
                T: Clone + ToLua<'lua>,
            {
                fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
                    (&self).to_lua(lua)
                }
            }

            impl<'lua, T> ToLua<'lua> for &[T; $N]
            where
                T: Clone + ToLua<'lua>,
            {
                fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
                    Ok(Value::Table(
                        lua.create_sequence_from(self.iter().cloned())?,
                    ))
                }
            }
        )+
    }
}

lua_convert_array! {
    0  1  2  3  4  5  6  7  8  9
   10 11 12 13 14 15 16 17 18 19
   20 21 22 23 24 25 26 27 28 29
   30 31 32
}

impl<'lua, T: ToLua<'lua>> ToLua<'lua> for Box<[T]> {
    fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::Table(lua.create_sequence_from(self.into_vec())?))
    }
}

impl<'lua, T: FromLua<'lua>> FromLua<'lua> for Box<[T]> {
    fn from_lua(value: Value<'lua>, _: &'lua Lua) -> Result<Self> {
        if let Value::Table(table) = value {
            table.sequence_values().collect()
        } else {
            Err(Error::FromLuaConversionError {
                from: value.type_name(),
                to: "Box<[T]>",
                message: Some("expected table".to_string()),
            })
        }
    }
}

impl<'lua, T: ToLua<'lua>> ToLua<'lua> for Vec<T> {
    fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::Table(lua.create_sequence_from(self)?))
    }
}

impl<'lua, T: FromLua<'lua>> FromLua<'lua> for Vec<T> {
    fn from_lua(value: Value<'lua>, _: &'lua Lua) -> Result<Self> {
        if let Value::Table(table) = value {
            table.sequence_values().collect()
        } else {
            Err(Error::FromLuaConversionError {
                from: value.type_name(),
                to: "Vec",
                message: Some("expected table".to_string()),
            })
        }
    }
}

impl<'lua, K: Eq + Hash + ToLua<'lua>, V: ToLua<'lua>, S: BuildHasher> ToLua<'lua>
    for HashMap<K, V, S>
{
    fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::Table(lua.create_table_from(self)?))
    }
}

impl<'lua, K: Eq + Hash + FromLua<'lua>, V: FromLua<'lua>, S: BuildHasher + Default> FromLua<'lua>
    for HashMap<K, V, S>
{
    fn from_lua(value: Value<'lua>, _: &'lua Lua) -> Result<Self> {
        if let Value::Table(table) = value {
            table.pairs().collect()
        } else {
            Err(Error::FromLuaConversionError {
                from: value.type_name(),
                to: "HashMap",
                message: Some("expected table".to_string()),
            })
        }
    }
}

impl<'lua, K: Ord + ToLua<'lua>, V: ToLua<'lua>> ToLua<'lua> for BTreeMap<K, V> {
    fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::Table(lua.create_table_from(self)?))
    }
}

impl<'lua, K: Ord + FromLua<'lua>, V: FromLua<'lua>> FromLua<'lua> for BTreeMap<K, V> {
    fn from_lua(value: Value<'lua>, _: &'lua Lua) -> Result<Self> {
        if let Value::Table(table) = value {
            table.pairs().collect()
        } else {
            Err(Error::FromLuaConversionError {
                from: value.type_name(),
                to: "BTreeMap",
                message: Some("expected table".to_string()),
            })
        }
    }
}

impl<'lua, T: Eq + Hash + ToLua<'lua>, S: BuildHasher> ToLua<'lua> for HashSet<T, S> {
    fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::Table(lua.create_table_from(
            self.into_iter().map(|val| (val, true)),
        )?))
    }
}

impl<'lua, T: Eq + Hash + FromLua<'lua>, S: BuildHasher + Default> FromLua<'lua> for HashSet<T, S> {
    fn from_lua(value: Value<'lua>, _: &'lua Lua) -> Result<Self> {
        match value {
            Value::Table(table) if table.len()? > 0 => table.sequence_values().collect(),
            Value::Table(table) => table
                .pairs::<T, Value<'lua>>()
                .map(|res| res.map(|(k, _)| k))
                .collect(),
            _ => Err(Error::FromLuaConversionError {
                from: value.type_name(),
                to: "HashSet",
                message: Some("expected table".to_string()),
            }),
        }
    }
}

impl<'lua, T: Ord + ToLua<'lua>> ToLua<'lua> for BTreeSet<T> {
    fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
        Ok(Value::Table(lua.create_table_from(
            self.into_iter().map(|val| (val, true)),
        )?))
    }
}

impl<'lua, T: Ord + FromLua<'lua>> FromLua<'lua> for BTreeSet<T> {
    fn from_lua(value: Value<'lua>, _: &'lua Lua) -> Result<Self> {
        match value {
            Value::Table(table) if table.len()? > 0 => table.sequence_values().collect(),
            Value::Table(table) => table
                .pairs::<T, Value<'lua>>()
                .map(|res| res.map(|(k, _)| k))
                .collect(),
            _ => Err(Error::FromLuaConversionError {
                from: value.type_name(),
                to: "BTreeSet",
                message: Some("expected table".to_string()),
            }),
        }
    }
}

impl<'lua, T: ToLua<'lua>> ToLua<'lua> for Option<T> {
    fn to_lua(self, lua: &'lua Lua) -> Result<Value<'lua>> {
        match self {
            Some(val) => val.to_lua(lua),
            None => Ok(Nil),
        }
    }
}

impl<'lua, T: FromLua<'lua>> FromLua<'lua> for Option<T> {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> Result<Self> {
        match value {
            Nil => Ok(None),
            value => Ok(Some(T::from_lua(value, lua)?)),
        }
    }
}
