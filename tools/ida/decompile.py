"""
Headless decompiler helper (idalib), for when the IDA GUI/MCP isn't available.

    ~/.idapro/venv/bin/python tools/ida/decompile.py DB.i64 0xFFF1EF1C [0x... ...]
    ~/.idapro/venv/bin/python tools/ida/decompile.py DB.i64 --script tools/ida/c1700_names.py --save

Work on a COPY of the database unless you really mean to modify it:
idalib opens the .i64 read-write.
"""

import argparse
import sys

import idapro  # must be imported before any ida_* module


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("db")
    ap.add_argument("addrs", nargs="*", help="function addresses to decompile")
    ap.add_argument("--script", action="append", default=[], help="IDAPython file to exec first")
    ap.add_argument("--save", action="store_true", help="save the database at the end")
    ap.add_argument("--disasm", action="store_true", help="print disassembly instead of pseudocode")
    args = ap.parse_args()

    if idapro.open_database(args.db, run_auto_analysis=True) != 0:
        sys.exit(f"cannot open {args.db}")
    import ida_auto
    import ida_funcs
    import ida_bytes  # noqa: F401
    import ida_hexrays
    import idc

    ida_auto.auto_wait()
    for path in args.script:
        exec(open(path).read(), {"__name__": "__ida_script__"})
    ida_hexrays.init_hexrays_plugin()
    for a in args.addrs:
        ea = int(a, 0)
        f = ida_funcs.get_func(ea)
        if f and f.start_ea != ea:
            # Asked for a function start that IDA folded into a bigger function:
            # split it so the requested address becomes its own function.
            print(f"note: {ea:#x} was inside {f.start_ea:#x}..{f.end_ea:#x}; splitting")
            ida_funcs.set_func_end(f.start_ea, ea)
            f = None
        if not f:
            idc.create_insn(ea)
            ida_funcs.add_func(ea)
            f = ida_funcs.get_func(ea)
        print(f"===== {ea:#x} {idc.get_func_name(ea)}" + (f" ({f.start_ea:#x}..{f.end_ea:#x})" if f else " (no function)"))
        if args.disasm and f:
            e = f.start_ea
            while e < f.end_ea:
                print(f"{e:08x}  {idc.GetDisasm(e)}")
                e = idc.next_head(e, f.end_ea)
        else:
            hf = ida_hexrays.hexrays_failure_t()
            cf = ida_hexrays.decompile_func(f, hf, 0) if f else None
            print(cf if cf else f"decompilation failed at {hf.errea:#x}: {hf.desc()}")
    idapro.close_database(save=args.save)


if __name__ == "__main__":
    main()
