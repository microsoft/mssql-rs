# gdb batch script used by run_mt_test.sh (MSSQL_ODBC_MT_GDB=1).
#
# Runs the test once. A normal exit returns the test's exit code. A stop on a
# signal (SIGSEGV, SIGBUS, or the watchdog's SIGABRT) prints every thread's
# stack and exits with 128 + the signal number, matching what a shell reports
# for a process killed by that signal.

set pagination off
set confirm off
set width 0
set print thread-events off
set print inferior-events off
# Containers commonly forbid personality(ADDR_NO_RANDOMIZE); gdb would warn on
# every run.
set disable-randomization off
handle SIGPIPE nostop noprint pass

run

if !$_isvoid($_exitsignal)
    echo \n=== run_mt_test: terminated by a signal; no live process for stacks ===\n
    quit 128 + $_exitsignal
end
if $_isvoid($_exitcode)
    echo \n=== run_mt_test: stopped on a signal; stacks of all threads follow ===\n
    info threads
    thread apply all bt 40
    echo === run_mt_test: end of stacks ===\n
    if $_isvoid($_siginfo)
        kill
        quit 134
    end
    set $odbc_mt_signal = $_siginfo.si_signo
    kill
    quit 128 + $odbc_mt_signal
end
quit $_exitcode
