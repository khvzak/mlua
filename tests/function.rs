#![cfg_attr(
    all(feature = "luajit", target_os = "macos", target_arch = "x86_64"),
    feature(link_args)
)]

#[cfg_attr(
    all(feature = "luajit", target_os = "macos", target_arch = "x86_64"),
    link_args = "-pagezero_size 10000 -image_base 100000000",
    allow(unused_attributes)
)]
extern "system" {}

use mlua::{Function, Lua, Result, String};

#[test]
fn test_function() -> Result<()> {
    let lua = Lua::new();

    let globals = lua.globals();
    lua.load(
        r#"
        function concat(arg1, arg2)
            return arg1 .. arg2
        end
    "#,
    )
    .exec()?;

    let concat = globals.get::<_, Function>("concat")?;
    assert_eq!(concat.call::<_, String>(("foo", "bar"))?, "foobar");

    Ok(())
}

#[test]
fn test_bind() -> Result<()> {
    let lua = Lua::new();

    let globals = lua.globals();
    lua.load(
        r#"
        function concat(...)
            local res = ""
            for _, s in pairs({...}) do
                res = res..s
            end
            return res
        end
    "#,
    )
    .exec()?;

    let mut concat = globals.get::<_, Function>("concat")?;
    concat = concat.bind("foo")?;
    concat = concat.bind("bar")?;
    concat = concat.bind(("baz", "baf"))?;
    assert_eq!(
        concat.call::<_, String>(("hi", "wut"))?,
        "foobarbazbafhiwut"
    );

    Ok(())
}

#[test]
fn test_rust_function() -> Result<()> {
    let lua = Lua::new();

    let globals = lua.globals();
    lua.load(
        r#"
        function lua_function()
            return rust_function()
        end

        -- Test to make sure chunk return is ignored
        return 1
    "#,
    )
    .exec()?;

    let lua_function = globals.get::<_, Function>("lua_function")?;
    let rust_function = lua.create_function(|_, ()| Ok("hello"))?;

    globals.set("rust_function", rust_function)?;
    assert_eq!(lua_function.call::<_, String>(())?, "hello");

    Ok(())
}

#[test]
fn test_function_info() -> Result<()> {
    let lua = Lua::new();

    let globals = lua.globals();
    lua.load(
        r#"
        function function_1()
            return function()
            end
        end

        function function_2()
            return function()
            end
        end
    "#,
    )
    .set_name("source 1")?
    .exec()?;
    lua.load(
        r#"
        function function_3()
            return function()
            end
        end

        function function_4()
            return function()
            end
        end
    "#,
    )
    .set_name("source 2")?
    .exec()?;

    let function_5 = lua.create_function(|_, ()| Ok(()))?;
    globals.set("function_5", function_5)?;

    let function_1 = globals.get::<_, Function>("function_1")?;
    let function_2 = globals.get::<_, Function>("function_2")?;
    let function_3 = globals.get::<_, Function>("function_3")?;
    let function_4 = globals.get::<_, Function>("function_4")?;
    let function_5 = globals.get::<_, Function>("function_5")?;

    let function_1a = function_1.call::<_, Function>(())?;
    let function_2a = function_2.call::<_, Function>(())?;
    let function_3a = function_3.call::<_, Function>(())?;
    let function_4a = function_4.call::<_, Function>(())?;

    let function_1_source = function_1.source();
    assert_eq!(function_1_source.source, "source 1");
    assert_eq!(function_1_source.line_defined, 2);
    assert_eq!(function_1_source.last_line_defined, 5);
    assert_eq!(function_1_source.what, "Lua");

    let function_1a_source = function_1a.source();
    assert_eq!(function_1a_source.source, "source 1");
    assert_eq!(function_1a_source.line_defined, 3);
    assert_eq!(function_1a_source.last_line_defined, 4);
    assert_eq!(function_1a_source.what, "Lua");

    let function_2_source = function_2.source();
    assert_eq!(function_2_source.source, "source 1");
    assert_eq!(function_2_source.line_defined, 7);
    assert_eq!(function_2_source.last_line_defined, 10);
    assert_eq!(function_2_source.what, "Lua");

    let function_2a_source = function_2a.source();
    assert_eq!(function_2a_source.source, "source 1");
    assert_eq!(function_2a_source.line_defined, 8);
    assert_eq!(function_2a_source.last_line_defined, 9);
    assert_eq!(function_2a_source.what, "Lua");

    let function_3_source = function_3.source();
    assert_eq!(function_3_source.source, "source 2");
    assert_eq!(function_3_source.line_defined, 2);
    assert_eq!(function_3_source.last_line_defined, 5);
    assert_eq!(function_3_source.what, "Lua");

    let function_3a_source = function_3a.source();
    assert_eq!(function_3a_source.source, "source 2");
    assert_eq!(function_3a_source.line_defined, 3);
    assert_eq!(function_3a_source.last_line_defined, 4);
    assert_eq!(function_3a_source.what, "Lua");

    let function_4_source = function_4.source();
    assert_eq!(function_4_source.source, "source 2");
    assert_eq!(function_4_source.line_defined, 7);
    assert_eq!(function_4_source.last_line_defined, 10);
    assert_eq!(function_4_source.what, "Lua");

    let function_4a_source = function_4a.source();
    assert_eq!(function_4a_source.source, "source 2");
    assert_eq!(function_4a_source.line_defined, 8);
    assert_eq!(function_4a_source.last_line_defined, 9);
    assert_eq!(function_4a_source.what, "Lua");

    let function_5_source = function_5.source();
    assert_eq!(function_5_source.source, "=[C]");
    assert_eq!(function_5_source.line_defined, -1);
    assert_eq!(function_5_source.last_line_defined, -1);
    assert_eq!(function_5_source.what, "C");

    Ok(())
}
