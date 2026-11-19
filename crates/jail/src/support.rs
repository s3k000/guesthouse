//! What this kernel can do for a jail, asked without changing anything.

use crate::landlock;
use std::fs;
use std::io;

const OWN_THREADS: &str = "/proc/self/task";
const NO_LANDLOCK: &str =
    "this kernel has no Landlock. Landlock is in the kernels of Fedora, Ubuntu 24.04 and Debian 13.";
/// `lsm=` replaces the kernel's own list, so the modules that run now have to be named in it too.
const LANDLOCK_OFF: &str = "this kernel has Landlock built in, but not turned on. To turn it on, put lsm= on the \
                            kernel's command line with landlock and every module /sys/kernel/security/lsm names \
                            now, separated by commas, and restart the PC.";

/// What the running kernel offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Support {
    pub landlock_abi: u32,
    /// The kernel has Landlock built in, but it isn't among the security modules it started.
    pub landlock_off: bool,
    pub seccomp: bool,
}

impl Support {
    pub fn probe() -> Self {
        // SAFETY: this only reads this thread's seccomp mode. A kernel without seccomp says EINVAL.
        let seccomp = unsafe { libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0) } >= 0;
        Self { landlock_abi: landlock::abi(), landlock_off: landlock::turned_off(), seccomp }
    }

    /// Why a jail can't be had here, if it can't. These are the parts that need no namespaces.
    pub fn refusal(&self) -> Option<&'static str> {
        if self.landlock_abi == 0 {
            return Some(if self.landlock_off { LANDLOCK_OFF } else { NO_LANDLOCK });
        }
        (!self.seccomp).then_some("this kernel has no seccomp filters.")
    }
}

/// Landlock covers only the thread that applies it and what that thread starts later. This says
/// whether this process has just the one thread, so a jail can't leave one out.
pub fn only_thread() -> io::Result<bool> {
    Ok(fs::read_dir(OWN_THREADS)?.count() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_landlock_is_a_refusal_and_its_reason_says_so() {
        let support = Support { landlock_abi: 0, landlock_off: false, seccomp: true };
        assert!(support.refusal().unwrap().contains("Landlock"));
        assert_eq!(Support { landlock_abi: 4, landlock_off: false, seccomp: true }.refusal(), None);
        assert!(Support { landlock_abi: 4, landlock_off: false, seccomp: false }.refusal().is_some());
    }

    #[test]
    fn landlock_left_off_says_how_to_turn_it_on_and_a_kernel_without_it_does_not() {
        let off = Support { landlock_abi: 0, landlock_off: true, seccomp: true }.refusal().unwrap();
        assert!(off.contains("lsm=") && off.contains("/sys/kernel/security/lsm"), "{off}");
        let none = Support { landlock_abi: 0, landlock_off: false, seccomp: true }.refusal().unwrap();
        assert!(!none.contains("lsm="), "{none}");
    }

    #[test]
    fn a_kernel_that_gives_an_abi_has_not_left_landlock_off() {
        let here = Support::probe();
        assert!(here.landlock_abi == 0 || !here.landlock_off, "{here:?}");
    }
}
