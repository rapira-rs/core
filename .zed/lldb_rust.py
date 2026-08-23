"""Rust data formatters for LLDB.

CodeLLDB's own Rust loader refuses to run: it requires both `lldb_lookup.py` and `lldb_commands`
in the toolchain, and rustc no longer ships `lldb_commands` (the registration moved into
`lldb_lookup.py`'s `__lldb_init_module`). Without this, LLDB shows raw `Vec`/`String`/enum guts.
"""

import subprocess

import lldb


def _rust_etc_dir():
    sysroot = subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip()
    return sysroot + "/lib/rustlib/etc"


def _guard_scalar_unwrap():
    """Bound rustc's `unwrap_scalar_wrappers`, which spins forever on a type LLDB cannot parse.

    `core::sync::atomic::Atomic<usize>` is emitted as a sized struct with no members, so LLDB
    fails to parse it and `GetChildAtIndex(0)` keeps handing back an invalid value. The upstream
    loop has no validity guard, so a single `Arc` in scope hangs the whole variables pane.
    Stop on the first invalid child and reinterpret the stalled value as an integer so the
    strong/weak counts stay correct.
    """
    import lldb_providers

    def unwrap_scalar_wrappers(wrapper):
        for _ in range(8):
            if not wrapper.IsValid():
                return wrapper
            if wrapper.GetType().GetTypeFlags() & lldb.eTypeIsInteger:
                return wrapper
            child = wrapper.GetChildAtIndex(0)
            if not child.IsValid():
                break
            wrapper = child

        addr = wrapper.GetLoadAddress()
        if addr == lldb.LLDB_INVALID_ADDRESS:
            return wrapper
        uint = wrapper.GetTarget().GetBasicType(lldb.eBasicTypeUnsignedLong)
        return wrapper.CreateValueFromAddress(wrapper.GetName() or "count", addr, uint)

    lldb_providers.unwrap_scalar_wrappers = unwrap_scalar_wrappers


def __lldb_init_module(debugger, internal_dict):
    try:
        etc = _rust_etc_dir()
    except (OSError, subprocess.CalledProcessError) as err:
        print("rust formatters: cannot locate the toolchain sysroot: {}".format(err))
        return

    debugger.HandleCommand('command script import "{}/lldb_lookup.py"'.format(etc))

    try:
        _guard_scalar_unwrap()
    except ImportError as err:
        print("rust formatters: loaded without the Arc guard: {}".format(err))
