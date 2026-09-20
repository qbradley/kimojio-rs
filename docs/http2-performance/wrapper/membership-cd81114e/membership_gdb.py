"""GDB-only observer for frozen x86_64 binaries; never used for timing."""
import collections
import gdb
import json
import os

counts = collections.Counter()
version = os.environ["MEMBERSHIP_VERSION"]
failure = None
# The independent frozen DWARF layout and entry disassembly establish these offsets.
# --readnever avoids loading the full Rust type graph on every breakpoint.
wait_size = 80


def word(address):
    return int.from_bytes(gdb.selected_inferior().read_memory(address, 8), "little")


class Retirement(gdb.Breakpoint):
    def stop(self):
        global failure
        try:
            # Both frozen entry sequences load the RcInner pointer from [rdi].
            owner = word(int(gdb.parse_and_eval("$rdi")))
            if version == "baseline":
                length = word(owner + 0x50)
            else:
                discriminator = word(owner + 0x40)
                if discriminator == 0x8000000000000000:
                    length = 0
                elif discriminator == 0x8000000000000001:
                    length = 1
                else:
                    assert discriminator < 0x8000000000000000
                    length = word(owner + 0x50)
                    assert length >= 2
            assert length < 1024
            counts[length] += 1
            assert sum(counts.values()) <= 100000
            return False
        except Exception as error:
            failure = str(error)
            return True


gdb.execute("set pagination off")
gdb.execute("set language c")
gdb.execute("set confirm off")
gdb.execute("set disable-randomization off")
gdb.execute("set startup-with-shell off")
breakpoint = Retirement("*" + os.environ["MEMBERSHIP_SYMBOL"], internal=True)
breakpoint.silent = True
gdb.execute("run")
record = {
    "version": version, "wait_data_bytes": wait_size, "rc_wait_allocation_bytes": wait_size + 16,
    "membership_storage_bytes": 24, "counts_by_membership_length": dict(counts),
    "retirement_observations": sum(counts.values()), "failure": failure,
    "timing_evidence": False, "scope": "entry to retire_from_scopes; excludes ready waits without WaitData",
    "instrumentation": "software breakpoint changes scheduling; frozen on-disk normal binary unchanged",
}
with open(os.environ["MEMBERSHIP_OUTPUT"], "x") as output:
    json.dump(record, output, indent=2)
    output.write("\n")
if failure:
    raise RuntimeError(failure)
