#!/usr/bin/env python3
"""End-to-end checks for the three shell integrations, driven through a real pty.

Everything interesting about zcomplete only happens in a terminal: preexec fires
in interactive shells, the not-found handler runs in the shell's own process, and
the confirmation prompt reads /dev/tty directly. A subprocess with pipes proves
none of it, so each scenario below runs against a shell on a pty and answers the
prompts by typing at it.

Usage:  python3 tests/shells.py [zsh|bash|fish]...
"""

import os
import pty
import re
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = ROOT / "target" / "release"
TIMEOUT = 6.0


def watchdog(*_):
    raise AssertionError("check hung")

ANSI = re.compile(
    r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)"  # OSC, terminated by BEL or ST
    r"|\x1b[P^_][^\x1b]*\x1b\\"  # DCS and friends
    r"|\x1b\[[0-9;?>!]*[A-Za-z]"  # CSI
    r"|\x1b[>=<]"
    r"|\r"
)

# fish will not draw a prompt until the terminal answers its capability probes,
# so the harness has to be just enough of a terminal to reply.
TERMINAL_REPLIES = [
    ("\x1b[0c", "\x1b[?6c"),  # primary device attributes
    ("\x1b[?u", "\x1b[?0u"),  # kitty keyboard protocol
    ("\x1b[>0q", "\x1bP>|xterm(370)\x1b\\"),  # XTVERSION
    ("\x1b]11;?", "\x1b]11;rgb:0000/0000/0000\x1b\\"),  # background colour
    ("\x1bP+q", "\x1bP0+r\x1b\\"),  # XTGETTCAP: nothing to say
    ("\x1b[6n", "\x1b[1;1R"),  # cursor position
]


class Session:
    """An interactive shell attached to a pty, driven a line at a time."""

    def __init__(self, argv, env, cwd):
        self.buffer = ""
        self.counter = 0
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.environ.update(env)
            try:
                os.chdir(cwd)
                os.execvp(argv[0], argv)
            finally:
                os._exit(127)
        self.drain(0.4)

    def drain(self, seconds):
        deadline = time.time() + seconds
        while time.time() < deadline:
            ready, _, _ = select.select([self.fd], [], [], max(0, deadline - time.time()))
            if not ready:
                break
            try:
                chunk = os.read(self.fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            text = chunk.decode("utf-8", "replace")
            self.buffer += text
            for query, reply in TERMINAL_REPLIES:
                if query in text:
                    os.write(self.fd, reply.encode())

    def expect(self, needle, timeout=TIMEOUT):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if needle in ANSI.sub("", self.buffer):
                return True
            self.drain(0.05)
        return False

    def type(self, text):
        os.write(self.fd, text.encode())

    def run(self, command, timeout=TIMEOUT):
        """Send a command, wait for it to finish, return (output, exit status).

        Lines end in a carriage return, which is what a terminal sends when the
        user presses enter — fish binds that key, and sending a line feed
        instead quietly bypassed the binding."""
        self.counter += 1
        marker = f"ZC{self.counter}"
        status = "$status" if self.kind == "fish" else "$?"
        start = len(self.buffer)
        self.type(f"{command}\r")
        self.type(f'echo "{marker}:"{status}\r')
        if not self.expect(f"{marker}:", timeout):
            raise AssertionError(f"timed out running {command!r}\n{self.tail()}")
        self.expect("\n", 0.3)

        text = ANSI.sub("", self.buffer[start:])
        match = re.search(rf"{marker}:(\d+)", text)
        code = int(match.group(1)) if match else -1
        # Drop the echoed command line and the marker itself.
        cleaned = "\n".join(
            line
            for line in text.splitlines()
            if marker not in line and line.strip() != command.strip()
        )
        return cleaned, code

    def tail(self, lines=25):
        return "\n".join(ANSI.sub("", self.buffer).splitlines()[-lines:])

    def close(self):
        """Wait for the shell to really exit — bash writes its history on the
        way out, and the temporary home is deleted the moment we return."""
        try:
            self.type("exit\r")
        except OSError:
            pass
        if not self.reap(1.0):
            # Closing the master end hangs up the session, which is usually
            # enough; SIGKILL is for a shell that is wedged in raw mode.
            try:
                os.close(self.fd)
            except OSError:
                pass
            self.fd = -1
            if not self.reap(1.0):
                try:
                    os.kill(self.pid, signal.SIGKILL)
                except OSError:
                    pass
                self.reap(2.0)
        if self.fd != -1:
            try:
                os.close(self.fd)
            except OSError:
                pass

    def reap(self, seconds):
        deadline = time.time() + seconds
        while time.time() < deadline:
            try:
                if os.waitpid(self.pid, os.WNOHANG)[0]:
                    return True
            except ChildProcessError:
                return True
            time.sleep(0.02)
        return False


SHELLS = {
    "zsh": {
        "argv": lambda exe: [exe, "-f", "-i"],
        "setup": ['PS1=""', 'eval "$(zcomplete init zsh)"'],
    },
    "bash": {
        "argv": lambda exe: [exe, "--norc", "--noprofile", "-i"],
        "setup": ['PS1=""', 'set +o history; set -o history', 'eval "$(zcomplete init bash)"'],
    },
    "fish": {
        "argv": lambda exe: [exe, "--no-config", "-i"],
        "setup": ["function fish_prompt; end", "zcomplete init fish | source"],
    },
}


def find_shell(kind):
    if kind == "bash":
        # macOS ships bash 3.2, which predates command_not_found_handle.
        for candidate in ("/opt/homebrew/bin/bash", "/usr/local/bin/bash", shutil.which("bash")):
            if candidate and Path(candidate).exists():
                version = subprocess.run(
                    [candidate, "-c", "echo ${BASH_VERSINFO[0]}"], capture_output=True, text=True
                ).stdout.strip()
                if version.isdigit() and int(version) >= 4:
                    return candidate
        return None
    return shutil.which(kind)


# A fixed, tiny PATH. The host's real PATH would make every assertion depend on
# whichever binaries happen to be installed, and one of them turned out to be an
# interactive tool that a bypass-mode correction happily ran.
FAKE_PATH = [
    "mkdir", "clear", "rm", "cat", "ls", "sleep", "true", "false", "printf",
    "echo", "git", "uname", "sed", "tr", "grep", "stty", "tty", "dirname",
    "basename", "env", "id", "hostname", "date", "cut", "head", "tail",
]


def build_path(home):
    bindir = home / "bin"
    bindir.mkdir()
    for name in FAKE_PATH:
        real = shutil.which(name)
        if real:
            (bindir / name).symlink_to(real)
    (bindir / "zcomplete").symlink_to(BIN / "zcomplete")
    return bindir


class Scenario:
    def __init__(self, kind, exe, home):
        self.kind = kind
        self.exe = exe
        self.home = home
        self.data = home / "data"
        self.config = home / "config.toml"
        env = dict(os.environ)
        env.update(
            PATH=str(build_path(home)),
            ZCOMPLETE_DATA_DIR=str(self.data),
            ZCOMPLETE_CONFIG=str(self.config),
            HOME=str(home),
            TERM="xterm-256color",
            NO_COLOR="1",
        )
        self.env = env
        self.session = Session(SHELLS[kind]["argv"](exe), env, home)
        self.session.kind = kind
        for line in SHELLS[kind]["setup"]:
            self.session.run(line)

    def zcomplete(self, *args):
        return subprocess.run(
            [str(BIN / "zcomplete"), *args], capture_output=True, text=True, env=self.env
        )

    def mode(self, name):
        self.zcomplete(f"--{name}")

    def ran(self, code):
        return code == 0


CHECKS = []


def check(name):
    def register(fn):
        CHECKS.append((name, fn))
        return fn

    return register


@check("a fresh install can already correct an obvious typo")
def cold_start_works(sc):
    sc.mode("bypass")
    sc.zcomplete("forget", "--all")
    out, code = sc.session.run("mkd nothing-learned-yet")
    assert sc.ran(code), out
    assert (sc.home / "nothing-learned-yet").is_dir(), "PATH fallback did not fire"


@check("learns from preexec and corrects a prefix")
def learns_and_corrects(sc):
    sc.mode("bypass")
    sc.session.run("mkdir -p base")
    out, code = sc.session.run("mkd made-by-zcomplete")
    assert sc.ran(code), f"expected success, got {code}: {out}"
    assert (sc.home / "made-by-zcomplete").is_dir(), "mkdir did not run"


@check("a word that is not a command is never learned")
def never_learns_a_non_command(sc):
    sc.mode("bypass")
    for _ in range(3):
        sc.session.run("clean --all")
    sc.session.run("clear")
    learned = [line.split("\t")[0] for line in sc.zcomplete("export").stdout.splitlines()]
    assert "clean" not in learned, f"'clean' should never enter the database: {learned}"
    resolved = sc.zcomplete("query", "cle").stdout.strip()
    assert resolved == "clear", f"expected clear, got {resolved!r}"


@check("arguments survive intact, spaces and all")
def arguments_survive(sc):
    sc.mode("bypass")
    sc.session.run("mkdir -p keep")
    out, code = sc.session.run('mkd "two words"')
    assert sc.ran(code), out
    assert (sc.home / "two words").is_dir(), "quoted argument was not preserved"


@check("the corrected command's own exit status comes back")
def exit_status_propagates(sc):
    sc.mode("bypass")
    sc.session.run("mkdir -p already-here")
    out, code = sc.session.run("mkd already-here")
    assert code != 0, f"mkdir on an existing directory should fail, got {code}: {out}"
    assert not (sc.home / "already-here" / "already-here").exists()


@check("an unknown word still reports command not found")
def unknown_word_reports_normally(sc):
    sc.mode("bypass")
    out, code = sc.session.run("qqzzxx --nope")
    assert code == 127, f"expected 127, got {code}: {out}"
    assert "not found" in out.lower() or "unknown command" in out.lower(), out


@check("safe mode asks first, and y runs it")
def safe_mode_accepts(sc):
    sc.mode("safe")
    sc.session.run("mkdir -p seed")
    sc.session.type("mkd yes-please\r")
    assert sc.session.expect("zcomplete: run mkdir"), f"no prompt:\n{sc.session.tail()}"
    sc.session.type("y")
    time.sleep(0.5)
    sc.session.drain(0.5)
    assert (sc.home / "yes-please").is_dir(), f"y did not run it:\n{sc.session.tail()}"


@check("safe mode takes n for an answer")
def safe_mode_declines(sc):
    sc.mode("safe")
    sc.session.run("mkdir -p seed2")
    sc.session.type("mkd no-thanks\r")
    assert sc.session.expect("zcomplete: run mkdir"), f"no prompt:\n{sc.session.tail()}"
    sc.session.type("n")
    time.sleep(0.5)
    sc.session.drain(0.5)
    assert not (sc.home / "no-thanks").exists(), "n still ran the command"


@check("a declined correction stops being offered")
def declining_twice_buries_it(sc):
    sc.mode("safe")
    sc.session.run("mkdir -p seed3")
    for _ in range(2):
        sc.session.type("mkd buried\r")
        sc.session.expect("zcomplete: run mkdir")
        sc.session.type("n")
        sc.session.drain(0.4)
    assert sc.zcomplete("query", "mkd").returncode != 0, "mkd should have no candidates left"


@check("unsafe mode runs ordinary corrections without asking")
def unsafe_runs_quietly(sc):
    sc.mode("unsafe")
    sc.session.run("mkdir -p seed4")
    out, code = sc.session.run("mkd unprompted")
    assert sc.ran(code), out
    assert (sc.home / "unprompted").is_dir(), "unsafe mode should not have blocked"


@check("unsafe mode still asks before a destructive command")
def unsafe_guards_destruction(sc):
    sc.mode("unsafe")
    (sc.home / "victim.txt").write_text("keep me")
    sc.session.run("rm -f nothing-here")
    sc.session.type("rmx victim.txt\r")
    assert sc.session.expect("deletes files"), f"no danger prompt:\n{sc.session.tail()}"
    sc.session.type("n")
    sc.session.drain(0.5)
    assert (sc.home / "victim.txt").exists(), "declined delete still happened"


@check("bypass mode never asks, even about rm")
def bypass_asks_nothing(sc):
    sc.mode("bypass")
    (sc.home / "doomed.txt").write_text("bye")
    sc.session.run("rm -f nothing-here-either")
    out, code = sc.session.run("rmx doomed.txt")
    assert not (sc.home / "doomed.txt").exists(), f"bypass should have deleted it: {out}"


@check("a declined correction reports the shell's own error")
def declining_reports_not_found(sc):
    sc.mode("safe")
    sc.session.run("mkdir -p seed6")
    sc.session.type("mkd nope\r")
    assert sc.session.expect("zcomplete: run mkdir"), sc.session.tail()
    sc.session.type("n")
    out, code = sc.session.run("true")
    seen = sc.session.tail().lower()
    assert "not found" in seen or "unknown command" in seen, seen


@check("a non-interactive shell never corrects anything")
def scripts_are_left_alone(sc):
    sc.mode("bypass")
    sc.session.run("mkdir -p seed7")
    script = sc.home / "run.sh"
    script.write_text("mkd from-a-script\n")
    out, code = sc.session.run(f"{sc.exe} {script}")
    assert not (sc.home / "from-a-script").exists(), "a script got silently rewritten"


@check("piped input reaches the corrected command")
def pipes_still_work(sc):
    sc.mode("bypass")
    sc.session.run("cat /dev/null")
    # The payload must not appear in the line as typed, or the echoed command
    # makes the assertion pass while the shell is actually wedged.
    out, code = sc.session.run("printf 'PIP''EOK\\n' | ct")
    assert "PIPEOK" in out, f"stdin did not reach cat: {out!r}"


@check("redirection of a corrected command still redirects")
def redirection_still_works(sc):
    sc.mode("bypass")
    sc.session.run("cat /dev/null")
    sc.session.run("ct /dev/null > written.txt")
    sc.session.run("true")
    assert (sc.home / "written.txt").exists(), "the redirect never happened"


@check("a corrected command inside a substitution is captured")
def substitution_still_captures(sc):
    sc.mode("bypass")
    sc.session.run("printf ok > payload.txt")
    sc.session.run("cat /dev/null")
    grab = "set got (ct payload.txt)" if sc.kind == "fish" else "got=$(ct payload.txt)"
    sc.session.run(grab)
    out, code = sc.session.run("echo [$got]")
    assert "[ok]" in out, f"substitution captured {out!r}"


@check("sourcing the integration twice changes nothing")
def double_init_is_harmless(sc):
    sc.mode("bypass")
    for line in SHELLS[sc.kind]["setup"][1:]:
        sc.session.run(line)
    sc.session.run("mkdir -p seed8")
    out, code = sc.session.run("mkd twice-loaded")
    assert sc.ran(code), out
    assert (sc.home / "twice-loaded").is_dir(), "second load broke the handler"


@check("bash 3.2 offers the fix on the next prompt instead")
def legacy_bash_falls_back(sc):
    if sc.kind != "bash" or not Path("/bin/bash").exists():
        return
    version = subprocess.run(
        ["/bin/bash", "-c", "echo ${BASH_VERSINFO[0]}"], capture_output=True, text=True
    ).stdout.strip()
    if version != "3":
        return

    old = Session(["/bin/bash", "--norc", "--noprofile", "-i"], sc.env, sc.home)
    old.kind = "bash"
    try:
        for line in SHELLS["bash"]["setup"]:
            old.run(line)
        sc.mode("bypass")
        old.run("mkdir -p seed-legacy")
        old.run("mkd legacy-dir")
        old.drain(0.5)
        assert (sc.home / "legacy-dir").is_dir(), f"no post-hoc fix:\n{old.tail()}"
    finally:
        old.close()


@check("a multi-line command line survives correction")
def multiline_survives(sc):
    sc.mode("bypass")
    sc.session.run("mkdir -p seed-multi")
    if sc.kind == "fish":
        # fish splits command substitution on newlines, so the buffer used to
        # arrive as separate arguments and get joined with spaces: the second
        # command became an argument to the first.
        body = "for d in ONE TWO\n    mkd $d\nend"
    else:
        body = "for d in ONE TWO; do mkd $d; done"
    sc.session.run(body, timeout=8)
    sc.session.run("true")
    for name in ("ONE", "TWO"):
        assert (sc.home / name).is_dir(), f"{name} was not created:\n{sc.session.tail()}"
    assert not (sc.home / "mkd").exists(), "a command word was consumed as an argument"


@check("an open prompt does not stall other shells")
def a_prompt_does_not_hold_the_database(sc):
    if sc.kind != "zsh":
        return
    sc.mode("safe")
    sc.session.run("mkdir -p seed-lock")
    sc.session.type("mkd waiting-on-an-answer\r")
    assert sc.session.expect("zcomplete: run mkdir"), sc.session.tail()
    try:
        # The confirmation used to be inside the write lock, so every command
        # typed in every other shell waited out the two-second timeout.
        worst = 0
        for _ in range(3):
            started = time.perf_counter()
            sc.zcomplete("record", "--shell", "zsh", "--kind", "auto", "--", "git")
            worst = max(worst, (time.perf_counter() - started) * 1000)
        assert worst < 250, f"a waiting prompt blocked another shell for {worst:.0f}ms"
    finally:
        sc.session.type("n")
        sc.session.drain(0.3)


@check("hyphenated commands and shell functions are learned too")
def awkward_names_are_learned(sc):
    sc.mode("bypass")
    hyphen = sc.home / "bin" / "my-tool"
    hyphen.write_text("#!/bin/sh\necho tool ran\n")
    hyphen.chmod(0o755)
    define = (
        "function my_helper; echo helper ran; end"
        if sc.kind == "fish"
        else "my_helper() { echo helper ran; }"
    )
    sc.session.run(define)
    sc.session.run("my-tool")
    sc.session.run("my_helper")

    learned = [line.split("\t")[0] for line in sc.zcomplete("export").stdout.splitlines()]
    assert "my-tool" in learned, f"a hyphenated command was not learned: {learned}"
    assert "my_helper" in learned, f"a shell function was not learned: {learned}"

    out, code = sc.session.run("my-too")
    assert "tool ran" in out, f"hyphenated correction did not run: {out!r}"


@check("a correction is counted once, not twice")
def counted_once(sc):
    sc.mode("bypass")
    sc.session.run("mkdir -p seed-count")
    rank = lambda: next(
        (float(l.split("\t")[1]) for l in sc.zcomplete("export").stdout.splitlines()
         if l.startswith("mkdir\t")),
        0.0,
    )
    before = rank()
    sc.session.run("mkd counted-once")
    # fish rewrites the line before running it, so its preexec hook sees the
    # real command; counting it again inside zcomplete would double it.
    assert rank() - before == 1.0, f"mkdir went {before} -> {rank()}"


@check("a backgrounded typo does not wait on a prompt nobody can answer")
def background_never_prompts(sc):
    if sc.kind == "fish":
        return  # fish corrects before the job is backgrounded at all
    sc.mode("safe")
    sc.session.run("mkdir -p seed-bg")
    out, code = sc.session.run("mkd backgrounded &")
    sc.session.run("wait")
    seen = sc.session.tail().lower()
    assert "[y/n]" not in seen, f"asked a question a background job cannot answer:\n{seen}"
    assert not (sc.home / "backgrounded").exists(), "corrected without being able to ask"


@check("a pinned shortcut wins outright")
def pinned_shortcut_wins(sc):
    sc.mode("bypass")
    sc.session.run("mkdir -p seed5")
    sc.zcomplete("bind", "zz", "mkdir")
    out, code = sc.session.run("zz pinned-dir")
    assert sc.ran(code), out
    assert (sc.home / "pinned-dir").is_dir(), "bound shortcut did not run"


def concurrent_writes_all_land():
    """Eight shells recording at once must not lose each other's counts. The
    lock has to span the read as well as the write, which it did not at first:
    260 of 400 increments went missing."""
    with tempfile.TemporaryDirectory() as tmp:
        env = dict(os.environ, ZCOMPLETE_DATA_DIR=f"{tmp}/data", ZCOMPLETE_CONFIG=f"{tmp}/c.toml")
        binary = str(BIN / "zcomplete")
        workers = [
            subprocess.Popen(
                ["sh", "-c", f'for i in $(seq 50); do "{binary}" record --shell zsh --kind auto -- mkdir; done'],
                env=env,
            )
            for _ in range(8)
        ]
        for worker in workers:
            worker.wait()
        dump = subprocess.run([binary, "export"], capture_output=True, text=True, env=env).stdout
        rank = next((float(l.split("\t")[1]) for l in dump.splitlines() if l.startswith("mkdir\t")), 0)
        assert rank == 400, f"expected 400 increments, kept {rank}"
        assert not list(Path(f"{tmp}/data").glob("*.lock")), "a lock file was left behind"


def only_real_commands_are_offered():
    """The core invariant, end to end: a heavily-used word that is not installed
    must never win over a barely-used word that is. Built as a mutation test —
    removing the executability filter has to make this fail."""
    with tempfile.TemporaryDirectory() as tmp:
        home = Path(tmp)
        env = dict(
            os.environ,
            PATH=str(build_path(home)),
            ZCOMPLETE_DATA_DIR=f"{tmp}/data",
            ZCOMPLETE_CONFIG=f"{tmp}/c.toml",
        )
        binary = str(BIN / "zcomplete")
        seed = home / "seed.tsv"
        seed.write_text(
            "".join(f"{n}\t500.000\t{int(time.time())}\n" for n in ("clean", "gitx", "mkdirp"))
            + "".join(f"{n}\t2.000\t{int(time.time())}\n" for n in ("clear", "git", "mkdir"))
        )
        subprocess.run([binary, "import", "--restore", str(seed)], capture_output=True, env=env)

        for typed, want in (("cle", "clear"), ("git", "git"), ("mkd", "mkdir")):
            got = subprocess.run(
                [binary, "query", typed, "-n", "9"], capture_output=True, text=True, env=env
            ).stdout.split()
            assert want in got, f"{typed} should reach {want}, got {got}"
            for name in got:
                assert shutil.which(name, path=env["PATH"]), f"{typed} offered {name}, which is not a command"


def main():
    args = sys.argv[1:]
    only = None
    if "--only" in args:
        at = args.index("--only")
        only = args[at + 1]
        args = args[:at] + args[at + 2 :]
    wanted = args or list(SHELLS)
    checks = [(n, f) for n, f in CHECKS if only is None or only in n]
    if not (BIN / "zcomplete").exists():
        sys.exit("build first: cargo build --release")

    failures = 0
    try:
        concurrent_writes_all_land()
        print("ok    eight shells writing at once lose nothing")
    except Exception as err:
        failures += 1
        print(f"FAIL  eight shells writing at once lose nothing\n      {err}")

    for kind in wanted:
        exe = find_shell(kind)
        if not exe:
            note = " (needs bash 4+; macOS ships 3.2)" if kind == "bash" else ""
            print(f"\n{kind}: not available, skipping{note}")
            continue
        print(f"\n{kind}  ({exe})")
        for name, fn in checks:
            with tempfile.TemporaryDirectory(ignore_cleanup_errors=True) as tmp:
                sc = None
                # A wedged shell must not take the suite down with it.
                signal.signal(signal.SIGALRM, watchdog)
                signal.alarm(45)
                started = time.time()
                try:
                    sc = Scenario(kind, exe, Path(tmp))
                    fn(sc)
                    print(f"  ok    {name}  ({time.time() - started:.1f}s)")
                except Exception as err:
                    failures += 1
                    print(f"  FAIL  {name}  ({time.time() - started:.1f}s)\n        {err}")
                finally:
                    try:
                        if sc:
                            sc.session.close()
                    except Exception:
                        pass
                    signal.alarm(0)

    print("\nall shells passed" if not failures else f"\n{failures} failing check(s)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
