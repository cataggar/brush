//! Signal processing utilities

use crate::{error, sys, traps};

pub(crate) use nix::sys::signal::Signal;

static PROCESS_ENTRY_IGNORED_SIGNALS: std::sync::OnceLock<Vec<(i32, String)>> =
    std::sync::OnceLock::new();

#[cfg_attr(
    target_vendor = "apple",
    unsafe(link_section = "__DATA,__mod_init_func")
)]
#[cfg_attr(not(target_vendor = "apple"), unsafe(link_section = ".init_array"))]
#[used]
static CAPTURE_IGNORED_SIGNALS_AT_PROCESS_ENTRY: extern "C" fn() =
    capture_ignored_signals_at_process_entry;

extern "C" fn capture_ignored_signals_at_process_entry() {
    let _ignored_signals = PROCESS_ENTRY_IGNORED_SIGNALS.get_or_init(query_ignored_signals);
}

pub(crate) fn ignored_signals() -> &'static [(i32, String)] {
    PROCESS_ENTRY_IGNORED_SIGNALS
        .get_or_init(query_ignored_signals)
        .as_slice()
}

fn query_ignored_signals() -> Vec<(i32, String)> {
    let signals = Signal::iterator().map(|signal| (signal as i32, signal.as_str().to_owned()));

    #[cfg(target_os = "linux")]
    let signals = signals.chain(realtime_signals());

    signals
        .filter_map(|(number, name)| match signal_is_ignored(number) {
            Ok(true) => Some((number, name)),
            Ok(false) | Err(_) => None,
        })
        .collect()
}

fn signal_is_ignored(signal: i32) -> Result<bool, error::Error> {
    let mut action = std::mem::MaybeUninit::<nix::libc::sigaction>::uninit();

    // SAFETY: A null second argument queries the current disposition without changing it.
    // On success, `sigaction` fully initializes the output structure.
    nix::errno::Errno::result(unsafe {
        nix::libc::sigaction(signal, std::ptr::null(), action.as_mut_ptr())
    })?;

    // SAFETY: The successful `sigaction` call above initialized `action`.
    let action = unsafe { action.assume_init() };
    Ok(action.sa_sigaction == nix::libc::SIG_IGN)
}

#[cfg(target_os = "linux")]
fn realtime_signals() -> impl Iterator<Item = (i32, String)> {
    let min = nix::libc::SIGRTMIN();
    let max = nix::libc::SIGRTMAX();
    let midpoint = min + (max - min) / 2;

    (min..=max).map(move |signal| {
        let name = if signal == min {
            "SIGRTMIN".to_owned()
        } else if signal <= midpoint {
            format!("SIGRTMIN+{}", signal - min)
        } else if signal == max {
            "SIGRTMAX".to_owned()
        } else {
            format!("SIGRTMAX-{}", max - signal)
        };
        (signal, name)
    })
}

pub(crate) fn continue_process(pid: sys::process::ProcessId) -> Result<(), error::Error> {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::SIGCONT)
        .map_err(|_errno| error::ErrorKind::FailedToSendSignal)?;
    Ok(())
}

/// Sends a signal to a specific process.
///
/// # Arguments
/// * `pid` - The process ID to send the signal to
/// * `signal` - The signal to send (must be a real signal, not a trap signal)
pub fn kill_process(
    pid: sys::process::ProcessId,
    signal: traps::TrapSignal,
) -> Result<(), error::Error> {
    let translated_signal = match signal {
        traps::TrapSignal::Signal(signal) => signal,
        traps::TrapSignal::Debug
        | traps::TrapSignal::Err
        | traps::TrapSignal::Exit
        | traps::TrapSignal::Return => {
            return Err(error::ErrorKind::InvalidSignal(signal.to_string()).into());
        }
    };

    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), translated_signal)
        .map_err(|_errno| error::ErrorKind::FailedToSendSignal)?;

    Ok(())
}

pub(crate) fn lead_new_process_group() -> Result<(), error::Error> {
    nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))?;
    Ok(())
}

pub(crate) fn tstp_signal_listener() -> Result<tokio::signal::unix::Signal, error::Error> {
    let signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(
        nix::libc::SIGTSTP,
    ))?;
    Ok(signal)
}

pub(crate) fn chld_signal_listener() -> Result<tokio::signal::unix::Signal, error::Error> {
    let signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;
    Ok(signal)
}

pub(crate) use tokio::signal::ctrl_c as await_ctrl_c;

pub(crate) fn mask_sigttou() -> Result<(), error::Error> {
    let ignore = nix::sys::signal::SigAction::new(
        nix::sys::signal::SigHandler::SigIgn,
        nix::sys::signal::SaFlags::empty(),
        nix::sys::signal::SigSet::empty(),
    );

    // SAFETY:
    // Setting the signal action should be safe here. The unsafe concerns
    // for calling `sigaction` are primarily around ensuring that any provided
    // signal handler functions are only performing operations that are
    // safe to do in a signal handler context. Here we are not providing
    // a custom handler, just asking the OS to ignore the signal.
    unsafe { nix::sys::signal::sigaction(nix::sys::signal::Signal::SIGTTOU, &ignore) }?;

    Ok(())
}

pub(crate) fn poll_for_stopped_children() -> Result<bool, error::Error> {
    let mut found_stopped = false;

    loop {
        let wait_status = waitid_all(
            nix::sys::wait::WaitPidFlag::WUNTRACED | nix::sys::wait::WaitPidFlag::WNOHANG,
        );
        match wait_status {
            Ok(nix::sys::wait::WaitStatus::Stopped(_stopped_pid, _signal)) => {
                found_stopped = true;
            }
            Ok(_) => break,
            Err(nix::errno::Errno::ECHILD) => break,
            Err(e) => return Err(e.into()),
        }
    }

    Ok(found_stopped)
}

#[cfg(not(any(target_os = "macos", target_os = "netbsd", target_os = "openbsd")))]
fn waitid_all(
    flags: nix::sys::wait::WaitPidFlag,
) -> Result<nix::sys::wait::WaitStatus, nix::errno::Errno> {
    nix::sys::wait::waitid(nix::sys::wait::Id::All, flags)
}

// nix does not expose `waitid` on NetBSD/OpenBSD; `waitpid` for any child is
// equivalent for the flags used here.
#[cfg(any(target_os = "netbsd", target_os = "openbsd"))]
fn waitid_all(
    flags: nix::sys::wait::WaitPidFlag,
) -> Result<nix::sys::wait::WaitStatus, nix::errno::Errno> {
    nix::sys::wait::waitpid(None, Some(flags))
}

//
// N.B. These functions were mostly copied from nix::sys::wait (https://github.com/nix-rust/nix, MIT license)
// to enable use of the `waitid` call on macOS. Ideally nix would expose it on macOS and we would
// remove this code.
//

#[cfg(target_os = "macos")]
fn waitid_all(
    flags: nix::sys::wait::WaitPidFlag,
) -> Result<nix::sys::wait::WaitStatus, nix::errno::Errno> {
    // SAFETY:
    // Code copied from nix::sys::wait implementation of waitid for other platforms.
    // The siginfo structure is valid when filled with zeroes. Memory is zeroed
    // rather than uninitialized, as not all platforms initialize the memory in
    // the StillAlive case.
    let mut siginfo: nix::libc::siginfo_t = unsafe { std::mem::zeroed() };

    // SAFETY:
    // Code copied from nix::sys::wait implementation of waitid for other platforms.
    nix::errno::Errno::result(unsafe {
        nix::libc::waitid(nix::libc::P_ALL, 0, &raw mut siginfo, flags.bits())
    })?;

    siginfo_to_wait_status(siginfo)
}

#[cfg(target_os = "macos")]
fn siginfo_to_wait_status(
    siginfo: nix::libc::siginfo_t,
) -> Result<nix::sys::wait::WaitStatus, nix::errno::Errno> {
    // SAFETY:
    // Code copied from nix::sys::wait implementation of waitid for other platforms.
    let si_pid = unsafe { siginfo.si_pid() };
    if si_pid == 0 {
        return Ok(nix::sys::wait::WaitStatus::StillAlive);
    }

    let pid = nix::unistd::Pid::from_raw(si_pid);

    // SAFETY:
    // Code copied from nix::sys::wait implementation of waitid for other platforms.
    let si_status = unsafe { siginfo.si_status() };

    let status = match siginfo.si_code {
        nix::libc::CLD_EXITED => nix::sys::wait::WaitStatus::Exited(pid, si_status),
        nix::libc::CLD_KILLED | nix::libc::CLD_DUMPED => nix::sys::wait::WaitStatus::Signaled(
            pid,
            nix::sys::signal::Signal::try_from(si_status)?,
            siginfo.si_code == nix::libc::CLD_DUMPED,
        ),
        nix::libc::CLD_STOPPED => {
            nix::sys::wait::WaitStatus::Stopped(pid, nix::sys::signal::Signal::try_from(si_status)?)
        }
        nix::libc::CLD_CONTINUED => nix::sys::wait::WaitStatus::Continued(pid),
        _ => return Err(nix::errno::Errno::EINVAL),
    };

    Ok(status)
}
