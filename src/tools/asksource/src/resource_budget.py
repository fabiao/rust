"""Shared Linux process-tree budget for source analysis and compiler capture."""

import os
from pathlib import Path, PurePosixPath
import re
import stat
import subprocess
import sys


SLICE = "asksource.slice"
DEFAULT_MEMORY = "8G"
DEFAULT_SWAP = "2G"
CGROUP_ROOT = Path("/sys/fs/cgroup")


class BudgetError(ValueError):
    pass


def size(value, *, zero=False):
    match = re.fullmatch(r"([0-9]+)([KMGTPE]?)(?:i?B)?", value)
    if match is None:
        raise BudgetError("memory budgets require integer bytes or K/M/G/T/P/E suffixes")
    amount = int(match[1]) * 1024 ** (" KMGTPE".index(match[2]) if match[2] else 0)
    if amount >= 2**63 or (amount == 0 and not zero):
        raise BudgetError("memory budget must be finite and positive (swap may be zero)")
    return amount


def budgets():
    return (size(os.environ.get("ASKSOURCE_MEMORY_MAX", DEFAULT_MEMORY)),
            size(os.environ.get("ASKSOURCE_SWAP_MAX", DEFAULT_SWAP), zero=True))


def manager(*arguments):
    result = subprocess.run(["systemctl", "--user", *arguments],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    if result.returncode:
        raise BudgetError("user systemd containment unavailable: " + result.stderr.strip())
    return result.stdout.strip()


def group_path():
    value = manager("show", SLICE, "--property=ControlGroup", "--value")
    if not value:
        return None
    path = PurePosixPath(value)
    if not path.is_absolute() or ".." in path.parts or path.name != SLICE:
        raise BudgetError("user manager returned an invalid analysis cgroup")
    return CGROUP_ROOT / str(path).lstrip("/")


def membership():
    unified = [line[3:] for line in Path("/proc/self/cgroup").read_text().splitlines()
               if line.startswith("0::")]
    if len(unified) != 1:
        raise BudgetError("Linux analysis requires a readable unified cgroup v2 hierarchy")
    return PurePosixPath(unified[0])


def validate_group(group, memory, swap):
    for filename, expected in (("memory.max", memory), ("memory.swap.max", swap)):
        if group.joinpath(filename).read_text().strip() != str(expected):
            raise BudgetError("analysis slice budget differs from this invocation; "
                              "use matching ASKSOURCE_MEMORY_MAX/ASKSOURCE_SWAP_MAX")


def contained(memory, swap):
    own = membership()
    if SLICE not in own.parts:
        return False
    group = group_path()
    if group is None:
        raise BudgetError("analysis slice membership has no active user-manager owner")
    relative = PurePosixPath("/") / group.relative_to(CGROUP_ROOT).as_posix()
    if own != relative and relative not in own.parents:
        raise BudgetError("analysis process belongs to another slice owner")
    validate_group(group, memory, swap)
    return True


def jobs():
    value = os.environ.setdefault("CARGO_BUILD_JOBS", "1")
    if not value.isascii() or not value.isdecimal() or int(value) < 1:
        raise BudgetError("CARGO_BUILD_JOBS must be a positive integer for analysis")


def metrics(group):
    values = []
    for filename in ("memory.peak", "memory.swap.peak", "memory.events"):
        path = group / filename
        if path.exists():
            values.append(filename + "=" + ",".join(path.read_text().splitlines()))
    return " ".join(values)


def launch(command, memory, swap):
    import fcntl

    runtime = Path(os.environ.get("XDG_RUNTIME_DIR", "/run/user/" + str(os.getuid())))
    info = runtime.stat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise BudgetError("analysis requires the user's private runtime directory")
    descriptor = os.open(runtime / "asksource-budget.lock",
                         os.O_CREAT | os.O_RDWR | os.O_CLOEXEC | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "w") as lock:
        info = os.fstat(lock.fileno())
        if info.st_uid != os.getuid() or not stat.S_ISREG(info.st_mode):
            raise BudgetError("analysis budget lock has an invalid owner or file type")
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print("asksource: waiting for the shared analysis budget", file=sys.stderr, flush=True)
            fcntl.flock(lock, fcntl.LOCK_EX)
        group = group_path()
        if group is not None and group.exists():
            if "populated 1" in group.joinpath("cgroup.events").read_text().splitlines():
                raise BudgetError("analysis descendants are still active without their outer owner; "
                                  "wait for them to exit before starting another analysis")
        manager("set-property", "--runtime", SLICE, "MemoryAccounting=yes",
                "MemoryMax=" + str(memory), "MemorySwapMax=" + str(swap))
        manager("start", SLICE)
        group = group_path()
        if group is None:
            raise BudgetError("user manager did not activate the analysis slice")
        validate_group(group, memory, swap)
        print(f"asksource: shared budget memory={memory} swap={swap} jobs="
              + os.environ["CARGO_BUILD_JOBS"], file=sys.stderr, flush=True)
        before = metrics(group)
        try:
            result = subprocess.run(["systemd-run", "--user", "--scope", "--quiet",
                                     "--slice=" + SLICE,
                                     "--", *command])
            return result.returncode if result.returncode >= 0 else 128 - result.returncode
        finally:
            print("asksource: slice counters before: " + before, file=sys.stderr, flush=True)
            print("asksource: slice counters after: " + metrics(group), file=sys.stderr, flush=True)


def ensure(command):
    if sys.platform != "linux":
        return
    memory, swap = budgets()
    jobs()
    if not contained(memory, swap):
        raise SystemExit(launch(command, memory, swap))


def main(arguments):
    if arguments == ["--guard"]:
        if sys.platform != "linux":
            return 0
        memory, swap = budgets()
        jobs()
        return 0 if contained(memory, swap) else 1
    if len(arguments) < 2 or arguments[0] != "--":
        raise BudgetError("usage: resource_budget.py -- COMMAND [ARGUMENTS...]")
    command = arguments[1:]
    ensure(command)
    os.execvpe(command[0], command, os.environ)


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except (BudgetError, OSError, subprocess.SubprocessError) as error:
        print("asksource: shared memory containment refused: " + str(error), file=sys.stderr)
        sys.exit(2)
