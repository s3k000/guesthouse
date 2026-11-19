//! Looks at this PC. Nothing here changes anything.

use super::cards::{Card, Support, amd_support, card_in_use, find_cards};
use crate::context::Context;
use crate::gpu_maker::GpuMaker;
use crate::host::{RamOffer, free_gib, total_ram};
use crate::iso::any_iso_maker;
use crate::library::INSTALL_ROOM_GIB;
use crate::linux::amd::amd_cards;
use crate::linux::driver_versions::{branch_refusal, branch_refusal_among, untested_warning};
use crate::machine::{MIN_RAM_GIB, Machine};
use crate::setup::intel::{intel_cards_on_i915, supported_intel_cards, xe_force_probe};
use crate::usb::sysfs_text;
use crate::vm_args::on_path;
use guesthouse_backend_nvidia::profile::BRANCHES;
use rustix::fs::{Access, access};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const TOOLS: [&str; 4] = ["7z", "genisoimage", "qemu-img", "swtpm"];
/// `nvidia-smi` gives the card's clocks to the record builder.
const NVIDIA_TOOL: &str = "nvidia-smi";
/// What an Ubuntu guest's install takes, beyond what any machine does.
const UBUNTU_TOOLS: [&str; 6] = ["isoinfo", "genisoimage", "openssl", "ssh-keygen", "setarch", "bwrap"];
const ISO_TOOL: &str = "genisoimage";
const TOOLS_FIX: &str = "On Fedora: sudo dnf install 7zip genisoimage qemu-img swtpm edk2-ovmf. On Debian or \
                         Ubuntu: sudo apt install 7zip genisoimage qemu-utils swtpm ovmf. On Arch: sudo pacman -S \
                         7zip cdrtools qemu-img swtpm edk2-ovmf.";
const UBUNTU_TOOLS_FIX: &str = "On Fedora: sudo dnf install genisoimage openssl openssh-clients bubblewrap. On \
                                Debian or Ubuntu: sudo apt install genisoimage openssl openssh-client bubblewrap. \
                                On Arch: sudo pacman -S cdrtools openssl openssh bubblewrap.";
const BUILD_FIX: &str = "qemu/README.md in Guesthouse's source says how to build it.";
const QEMU_LIBRARIES_FIX: &str = "On Fedora: sudo dnf install pixman libslirp libfdt libusb1 pipewire-libs. On \
                                  Debian or Ubuntu: sudo apt install libpixman-1-0 libslirp0 libfdt1 libusb-1.0-0 \
                                  libpipewire-0.3-0. On Arch: sudo pacman -S pixman libslirp dtc libusb pipewire.";
const BRANCH_FIX: &str = "GPU machines stay off until the branches match. Plain VMs still run.";
const CARD_FIX: &str = "GPU-PV needs an NVIDIA RTX 50, 40, 30 or 20 series or GTX 16 series card on the open \
                        kernel modules, or an AMD card from the RX 5000 series on, or RDNA2 or newer graphics in a \
                        Ryzen, on amdgpu.";
/// For a PC with no card of any maker GPU-PV runs on.
const NO_CARD_FIX: &str = "GPU-PV needs an NVIDIA RTX 50, 40, 30 or 20 series or GTX 16 series card on the open \
                           kernel modules, an AMD card from the RX 5000 series on, or RDNA2 or newer graphics in a \
                           Ryzen, on amdgpu, or as an experiment, an Intel Arc B580 or Arc 140V on xe.";
const EXPERIMENTAL_FIX: &str =
    "Nobody has run Windows on this card yet. Others on the same chip work, so it likely does too.";
const AMD_UNSUPPORTED_FIX: &str = "Windows on AMD needs an RDNA card: the RX 5000 series and newer, or RDNA2 or \
                                   newer graphics in a Ryzen. Older cards like Polaris and Vega aren't handled.";
/// For a render node whose group can't be told from the node.
const RENDER_GROUP_FIX: &str =
    "It usually belongs to the render group: run sudo usermod -aG render $USER, then log in again.";
/// What setup does on a PC whose only cards are AMD's: nothing, as that driver comes with a machine.
pub const AMD_NEEDS_NO_SETUP: &str = "This PC's card is AMD's, and it needs no setup. AMD's driver is asked for \
                                      when you make a Windows machine.";
/// What setup does on a PC whose only card GPU-PV runs on is Intel's: nothing, as with AMD's.
pub const INTEL_NEEDS_NO_SETUP: &str = "This PC's card is Intel's, and it needs no setup. Windows with GPU-PV runs \
                                        on it is experimental. Intel's \
                                        driver is asked for when you make a Windows machine.";
/// The Linux drivers Windows has run on, newest first. The guest's graphics depend on the exact version.
pub const TESTED_DRIVERS: [&str; 2] = ["620.70.06", "615.78.08"];
/// What setup keeps of the installer, and room to swap it for a new copy.
pub const DRIVER_ROOM_GIB: u64 = 4;
/// What a new Windows machine gets by default.
pub const WANTED_RAM_GIB: u64 = 16;
const QEMU_CHECK_SECONDS: &str = "10";
/// The system's own list of groups, a line for each as name, password, number and members.
const GROUPS: &str = "/etc/group";
const ROOT_GROUP: u32 = 0;
const MOST_QEMU_WORDS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Ok,
    Warning,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub title: String,
    pub state: CheckState,
    pub text: String,
    pub fix: String,
    /// Whose GPU the check is about. A failed check about the PC stops both makers' cards.
    pub part: Part,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Pc,
    Nvidia,
    Amd,
    Intel,
}

/// Which makers' cards a GPU machine can run on, as far as the checks can say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GpuVerdict {
    pub nvidia: bool,
    pub amd: bool,
    pub intel: bool,
}

impl GpuVerdict {
    pub fn any(self) -> bool {
        self.nvidia || self.amd || self.intel
    }
}

/// A maker's cards can run a GPU machine when the PC has one, and nothing about them or the PC failed.
pub fn gpu_verdict(checks: &[Check]) -> GpuVerdict {
    let fine = |part| {
        checks.iter().filter(|check| check.part == part).all(|check| check.state != CheckState::Failed)
    };
    let found = |part| checks.iter().any(|check| check.part == part);
    let pc = fine(Part::Pc);
    GpuVerdict {
        nvidia: pc && found(Part::Nvidia) && fine(Part::Nvidia),
        amd: pc && found(Part::Amd) && fine(Part::Amd),
        intel: pc && found(Part::Intel) && fine(Part::Intel),
    }
}

impl Check {
    fn new(title: &str, state: CheckState, text: impl Into<String>, fix: impl Into<String>) -> Self {
        Self { title: title.to_owned(), state, text: text.into(), fix: fix.into(), part: Part::Pc }
    }

    fn ok(title: &str, text: impl Into<String>) -> Self {
        Self::new(title, CheckState::Ok, text, "")
    }

    fn about(self, part: Part) -> Self {
        Self { part, ..self }
    }
}

/// The card GPU machines run on, or if setup's choice can't be told, the one it would choose now.
/// Start checks say when setup's choice can't be told.
fn card_judged(context: &Context) -> Option<Card> {
    card_in_use(context).or_else(|| find_cards(context).into_iter().next())
}

/// A check for each maker's card this PC has, or one failure if it has none. Intel's cards are only
/// looked at on a PC with neither NVIDIA's nor AMD's, as they are experimental, and a PC that has
/// one of those runs on it.
fn check_cards(context: &Context) -> Vec<Check> {
    let checks: Vec<Check> = [check_card(context), check_amd_card(context)].into_iter().flatten().collect();
    if !checks.is_empty() {
        return checks;
    }
    match check_intel_card(context) {
        Some(intel) => vec![intel],
        None => {
            let text = "No NVIDIA, AMD or Intel card was found.";
            vec![Check::new("Graphics card", CheckState::Failed, text, NO_CARD_FIX)]
        }
    }
}

/// The first Intel card the Intel daemon is written for. It's a warning, not a failure: Windows sets
/// up and starts on it, and only one card has been tried.
fn check_intel_card(context: &Context) -> Option<Check> {
    const TITLE: &str = "Intel card";
    let Some(card) = supported_intel_cards(context).into_iter().next() else {
        return check_intel_card_on_i915(context);
    };
    let check = Check::new(
        TITLE,
        CheckState::Warning,
        format!("{} at {}. Experimental.", card.name, card.pci),
        "The card draws Windows' desktop on an Arc B580, and other cards are untried. Work on the card can \
         hang or reset it, which can freeze a Linux desktop on the same card. Intel's driver is asked for \
         when you make the machine.",
    );
    Some(check.about(Part::Intel))
}

/// An Intel card of a generation the daemon knows, on i915, which takes Alchemist, Meteor Lake and
/// Arrow Lake by default. xe takes it only when the kernel is told to. It fails, as nothing can run
/// on it as it is, and the fix says how.
fn check_intel_card_on_i915(context: &Context) -> Option<Check> {
    const TITLE: &str = "Intel card";
    let (card, generation) = intel_cards_on_i915(context).into_iter().next()?;
    let check = Check::new(
        TITLE,
        CheckState::Failed,
        format!(
            "{} at {}, {}, is on the i915 driver. Guesthouse needs xe.",
            card.name, card.pci, generation.name
        ),
        format!(
            "xe takes this card only when the kernel is told to. Add {} to the kernel's command line and \
             restart. xe on this card is newer than i915 and less tried.",
            xe_force_probe(card.device)
        ),
    );
    Some(check.about(Part::Intel))
}

fn check_card(context: &Context) -> Option<Check> {
    Some(judge_card(context, &card_judged(context)?).about(Part::Nvidia))
}

fn judge_card(context: &Context, card: &Card) -> Check {
    const TITLE: &str = "NVIDIA card";
    // With one card the name says which. With more the address does.
    let name = if find_cards(context).len() > 1 {
        format!("{} at {}", card.name, card.pci)
    } else {
        card.name.clone()
    };
    match card.support {
        Support::Tested => Check::ok(TITLE, format!("{name}. Tested.")),
        Support::Experimental => Check::new(
            TITLE,
            CheckState::Warning,
            format!("{name} ({}). Experimental.", card.chip),
            EXPERIMENTAL_FIX,
        ),
        Support::Unsupported => {
            Check::new(TITLE, CheckState::Failed, format!("{name}. Not supported."), CARD_FIX)
        }
    }
}

/// The best of this PC's AMD cards, as a Windows machine can run on any of them.
fn check_amd_card(context: &Context) -> Option<Check> {
    const TITLE: &str = "AMD card";
    let cards = amd_cards(context);
    let (card, support) =
        cards.iter().map(|card| (card, amd_support(context, card))).min_by_key(|(_, support)| *support)?;
    let name = if cards.len() > 1 { format!("{} at {}", card.name, card.pci) } else { card.name.clone() };
    let check = match support {
        Support::Tested => Check::new(
            TITLE,
            CheckState::Ok,
            format!("{name}. Tested."),
            "Windows on AMD is experimental, and AMD's driver is asked for when you make the machine.",
        ),
        Support::Experimental => Check::new(
            TITLE,
            CheckState::Warning,
            format!("{name}. Experimental."),
            format!("{EXPERIMENTAL_FIX} AMD's driver is asked for when you make the machine."),
        ),
        Support::Unsupported => {
            Check::new(TITLE, CheckState::Failed, format!("{name}. Not supported."), AMD_UNSUPPORTED_FIX)
        }
    };
    Some(check.about(Part::Amd))
}

/// Says if the card GPU machines run on is only experimental.
pub fn card_warning(context: &Context) -> Option<String> {
    let card = card_judged(context).filter(|card| card.support == Support::Experimental)?;
    Some(format!("{} is experimental. {EXPERIMENTAL_FIX}", card.name))
}

/// What a person should hear before a VM on the GPU starts.
pub fn gpu_warning(context: &Context) -> Option<String> {
    let warnings: Vec<String> =
        [card_warning(context), driver_warning(context)].into_iter().flatten().collect();
    (!warnings.is_empty()).then(|| warnings.join(" "))
}

/// What a person should hear before this machine starts: only a Windows machine on the NVIDIA card is
/// about NVIDIA's card and driver.
pub fn machine_gpu_warning(context: &Context, machine: &Machine) -> Option<String> {
    (machine.windows_maker() == Some(GpuMaker::Nvidia)).then(|| gpu_warning(context)).flatten()
}

/// Says if the host's driver isn't the one this was tested on.
pub fn driver_warning(context: &Context) -> Option<String> {
    let version = sysfs_text(&context.driver_version_file);
    if version.is_empty() {
        return Some("The NVIDIA driver does not seem to be loaded on this PC.".to_owned());
    }
    (!TESTED_DRIVERS.contains(&version.as_str())).then(|| {
        format!(
            "This PC has NVIDIA driver {version}, and the VM was tested on {}. The guest's graphics depend \
             on the exact version, so they may not work.",
            tested_drivers()
        )
    })
}

fn tested_drivers() -> String {
    TESTED_DRIVERS.join(" and ")
}

/// Only a PC with an NVIDIA card is asked about NVIDIA's driver and tool.
fn nvidia_checks(context: &Context) -> Vec<Check> {
    if find_cards(context).is_empty() {
        return Vec::new();
    }
    let mut checks = vec![check_host_driver(context)];
    if !tool_here(NVIDIA_TOOL) {
        let text = format!("{NVIDIA_TOOL} is missing.");
        checks.push(Check::new("NVIDIA tool", CheckState::Failed, text, "It comes with NVIDIA's driver."));
    }
    checks.into_iter().map(|check| check.about(Part::Nvidia)).collect()
}

fn check_host_driver(context: &Context) -> Check {
    const TITLE: &str = "Host NVIDIA driver";
    let version = sysfs_text(&context.driver_version_file);
    if version.is_empty() {
        let fix = format!("Install NVIDIA's driver {} for Linux and start this PC again.", TESTED_DRIVERS[0]);
        return Check::new(TITLE, CheckState::Failed, "The NVIDIA driver is not loaded.", fix);
    }
    if TESTED_DRIVERS.contains(&version.as_str()) {
        return Check::ok(TITLE, format!("{version}. Tested."));
    }
    let tested = tested_drivers();
    Check::new(
        TITLE,
        CheckState::Warning,
        format!("{version}, and this was tested on {tested}."),
        format!(
            "The guest's graphics depend on the exact version. Install {} if they fail.",
            TESTED_DRIVERS[0]
        ),
    )
}

fn check_kvm(context: &Context) -> Check {
    const TITLE: &str = "Virtualization";
    let kvm = context.kvm.display();
    if !context.kvm.exists() {
        let fix = "Turn on SVM or VT-x in this PC's firmware settings.";
        return Check::new(TITLE, CheckState::Failed, format!("{kvm} does not exist."), fix);
    }
    if access(&context.kvm, Access::READ_OK | Access::WRITE_OK).is_err() {
        let fix = "Run sudo usermod -aG kvm $USER, then log in again.";
        return Check::new(TITLE, CheckState::Failed, format!("This user cannot open {kvm}."), fix);
    }
    Check::ok(TITLE, format!("{kvm} can be opened."))
}

/// The daemon hands a guest's memory to an AMD card through this device, pinned once. Without it the
/// card still gets the memory, by its address, and every submission pays for all of it. So this never
/// fails a PC, and a PC with no AMD card isn't asked.
fn check_amd_memory(context: &Context) -> Option<Check> {
    const TITLE: &str = "AMD card's guest memory";
    const SLOWER: &str = "Windows on the AMD card works without it, only slower.";
    if amd_cards(context).is_empty() {
        return None;
    }
    let device = context.udmabuf.display();
    if !context.udmabuf.exists() {
        let fix = format!("Load the kernel's udmabuf module. {SLOWER}");
        return Some(Check::new(TITLE, CheckState::Warning, format!("{device} does not exist."), fix));
    }
    if access(&context.udmabuf, Access::READ_OK | Access::WRITE_OK).is_err() {
        let fix = format!(
            "It usually belongs to the kvm group: run sudo usermod -aG kvm $USER, then log in again. {SLOWER}"
        );
        return Some(Check::new(TITLE, CheckState::Warning, format!("This user cannot open {device}."), fix));
    }
    Some(Check::ok(TITLE, format!("{device} can be opened.")))
}

/// The name of the group a device belongs to, from the list in `groups`. A group that isn't in the
/// list has no name here, and neither has root's, as nobody should be told to join that one.
fn group_of(device: &Path, groups: &str) -> Option<String> {
    let number =
        fs::metadata(device).ok().map(|device| device.gid()).filter(|number| *number != ROOT_GROUP)?;
    groups.lines().find_map(|group| {
        let mut fields = group.split(':');
        let name = fields.next()?;
        (fields.nth(1)?.parse() == Ok(number)).then(|| name.to_owned())
    })
}

/// The daemon and QEMU open the card's render node as this user, and a machine on the card can't start
/// without it. A desktop session opens it to whoever is at the seat, so it's a login with no seat, as
/// over ssh or for a service's own user, that finds it shut on a distro that keeps it to a group. The
/// same PC can be fine from its desktop, and one shut card of two leaves the other, so this warns and
/// never fails a PC. Nothing is opened here, so the card isn't touched. A PC with no AMD card isn't asked.
fn check_amd_render_node(context: &Context) -> Option<Check> {
    const TITLE: &str = "AMD card's render node";
    const NEEDED: &str = "No machine on that card starts without it.";
    let nodes: Vec<PathBuf> = amd_cards(context).into_iter().map(|card| card.render_node).collect();
    if nodes.is_empty() {
        return None;
    }
    if let Some(missing) = nodes.iter().find(|node| !node.exists()) {
        let text = format!("{} does not exist.", missing.display());
        let fix = format!("The card's kernel driver makes it. {NEEDED}");
        return Some(Check::new(TITLE, CheckState::Warning, text, fix));
    }
    let Some(shut) = nodes.iter().find(|node| access(*node, Access::READ_OK | Access::WRITE_OK).is_err())
    else {
        let shown: Vec<String> = nodes.iter().map(|node| node.display().to_string()).collect();
        return Some(Check::ok(TITLE, format!("{} can be opened.", shown.join(" and "))));
    };
    let named = group_of(shut, &fs::read_to_string(GROUPS).unwrap_or_default()).map(|group| {
        format!("It belongs to the {group} group: run sudo usermod -aG {group} $USER, then log in again.")
    });
    let fix = format!("{} {NEEDED}", named.unwrap_or_else(|| RENDER_GROUP_FIX.to_owned()));
    Some(Check::new(TITLE, CheckState::Warning, format!("This user cannot open {}.", shut.display()), fix))
}

fn check_qemu(context: &Context) -> Check {
    const TITLE: &str = "Patched QEMU";
    let qemu = &context.paths.qemu;
    let shown = qemu.display();
    if access(qemu, Access::EXEC_OK).is_err() {
        return Check::new(TITLE, CheckState::Failed, format!("{shown} is missing."), BUILD_FIX);
    }
    let said = Command::new("timeout")
        .arg(QEMU_CHECK_SECONDS)
        .arg(qemu)
        .arg("-version")
        .stdin(Stdio::null())
        .output();
    let said = match said {
        Ok(said) => said,
        Err(error) => {
            return Check::new(
                TITLE,
                CheckState::Failed,
                format!("{shown} did not run: {error}"),
                QEMU_LIBRARIES_FIX,
            );
        }
    };
    if said.status.success() {
        return Check::ok(TITLE, shown.to_string());
    }
    let error = String::from_utf8_lossy(&said.stderr);
    let error = error.trim();
    let start = error.char_indices().rev().nth(MOST_QEMU_WORDS - 1).map_or(0, |found| found.0);
    let text = if error.is_empty() { format!("{shown} did not run.") } else { error[start..].to_owned() };
    Check::new(TITLE, CheckState::Failed, text, QEMU_LIBRARIES_FIX)
}

fn check_tools(context: &Context, tools: &[&str]) -> Check {
    const TITLE: &str = "Tools";
    let mut missing: Vec<&str> = tools.iter().copied().filter(|tool| !tool_here(tool)).collect();
    let firmware = &context.paths.firmware;
    if ![&firmware.vars, &firmware.vars_secure, &firmware.code, &firmware.code_secure]
        .iter()
        .all(|path| path.exists())
    {
        missing.push("OVMF firmware");
    }
    if !missing.is_empty() {
        return Check::new(TITLE, CheckState::Failed, format!("Missing: {}.", missing.join(", ")), TOOLS_FIX);
    }
    Check::ok(TITLE, format!("{} and the OVMF firmware are here.", tools.join(", ")))
}

/// Arch has no `genisoimage`, and its `mkisofs` takes the same words.
fn tool_here(tool: &str) -> bool {
    if tool == ISO_TOOL { any_iso_maker() } else { on_path(tool) }
}

/// An Ubuntu guest boots whatever this says, so nothing here is a failure: at worst its GPU stays off.
fn check_ubuntu(context: &Context, tools: &[&str]) -> Check {
    const TITLE: &str = "Ubuntu guests";
    let missing: Vec<&str> = tools.iter().copied().filter(|tool| !tool_here(tool)).collect();
    if !missing.is_empty() {
        let text = format!("Missing: {}.", missing.join(", "));
        return Check::new(TITLE, CheckState::Warning, text, UBUNTU_TOOLS_FIX);
    }
    let version = sysfs_text(&context.driver_version_file);
    if find_cards(context).is_empty() && !amd_cards(context).is_empty() {
        return Check::ok(
            TITLE,
            "They install by themselves, with Mesa on the AMD card. The GPU is a preview.",
        );
    }
    let refused = if version.is_empty() {
        Some("The NVIDIA driver is not loaded.".to_owned())
    } else {
        branch_refusal(&version)
    };
    if let Some(reason) = refused {
        return Check::new(TITLE, CheckState::Warning, "They boot, with no GPU.", reason);
    }
    let text = "They install by themselves. The GPU is a preview.";
    match untested_warning(&version) {
        Some(warning) => Check::new(TITLE, CheckState::Warning, text, warning),
        None => Check::ok(TITLE, text),
    }
}

/// Judges what this PC's RAM leaves a GPU machine, with no other VM running.
fn judge_ram(offer: &RamOffer) -> Check {
    const TITLE: &str = "Memory";
    let least = u64::from(MIN_RAM_GIB);
    let text = offer.explanation(least);
    if offer.limit_gib() < least {
        Check::new(TITLE, CheckState::Failed, text, "A GPU machine can't start on this PC.")
    } else if offer.tight(least) {
        let fix = format!(
            "Give the machine {least} GiB, and close other programs before it starts. It won't start while the \
             PC is using too much."
        );
        Check::new(TITLE, CheckState::Warning, text, fix)
    } else if offer.limit_gib() < WANTED_RAM_GIB {
        let fix = format!(
            "A new Windows machine gets {WANTED_RAM_GIB} GiB unless it's given less, so give it {} GiB or less.",
            offer.limit_gib()
        );
        Check::new(TITLE, CheckState::Warning, text, fix)
    } else {
        Check::ok(TITLE, text)
    }
}

fn check_ram(context: &Context) -> Check {
    judge_ram(&RamOffer::new(total_ram(context), 0))
}

/// The nearest folder that exists.
pub fn existing_parent(path: &Path) -> PathBuf {
    path.ancestors().find(|folder| folder.exists()).unwrap_or(Path::new("/")).to_owned()
}

/// Judges the disk: `room` is what is free in `place`.
fn judge_disk(room: f64, place: &Path) -> Check {
    const TITLE: &str = "Free disk";
    let need = INSTALL_ROOM_GIB + DRIVER_ROOM_GIB;
    let text = format!("{room:.0} GiB free in {}.", place.display());
    if room < need as f64 {
        let fix = format!("A Windows machine needs {need} GiB. Free some space there.");
        return Check::new(TITLE, CheckState::Failed, text, fix);
    }
    Check::ok(TITLE, text)
}

fn check_disk(context: &Context) -> Check {
    let store = &context.paths.store;
    let place = existing_parent(store);
    if access(&place, Access::WRITE_OK).is_err() {
        let fix = format!("Make {} and give it to this user.", store.display());
        return Check::new(
            "Free disk",
            CheckState::Failed,
            format!("This user cannot write to {}.", place.display()),
            fix,
        );
    }
    judge_disk(free_gib(&place).unwrap_or(0.0), &place)
}

pub fn check_pc(context: &Context) -> Vec<Check> {
    let mut checks = check_cards(context);
    checks.extend(nvidia_checks(context));
    checks.push(check_kvm(context));
    checks.extend(check_amd_render_node(context));
    checks.extend(check_amd_memory(context));
    checks.extend([
        check_qemu(context),
        check_tools(context, &TOOLS),
        check_ubuntu(context, &UBUNTU_TOOLS),
        check_ram(context),
        check_disk(context),
    ]);
    checks
}

/// The branches the NVIDIA backend has a profile for, which a Windows guest can run on. Linux guests
/// forward the driver's own calls, and know only the branches in `driver_versions`.
fn windows_host_branches() -> Vec<&'static str> {
    BRANCHES.iter().map(|branch| branch.name).collect()
}

/// A host driver on a branch this build has no profile for runs nothing on the card, whatever else is
/// right, so the GPU's verdict needs this as well as the version check above.
fn check_driver_branch(context: &Context) -> Option<Check> {
    const TITLE: &str = "Host driver branch";
    let version = sysfs_text(&context.driver_version_file);
    let refusal = Some(version)
        .filter(|text| !text.is_empty())
        .and_then(|text| branch_refusal_among(&text, &windows_host_branches()))?;
    Some(Check::new(TITLE, CheckState::Failed, refusal, BRANCH_FIX).about(Part::Nvidia))
}

/// What an installer asks before it has put anything on this PC: only what no install can bring, so no
/// tools and no QEMU. The checks for room say what a machine will need, and don't decide.
pub struct HostReport {
    pub checks: Vec<Check>,
    /// Whether this PC can run a GPU machine on either maker's card, as far as the host can say.
    pub gpu: bool,
}

pub fn check_host(context: &Context) -> HostReport {
    let mut deciding = check_cards(context);
    if !find_cards(context).is_empty() {
        deciding.push(check_host_driver(context).about(Part::Nvidia));
    }
    deciding.push(check_kvm(context));
    deciding.extend(check_driver_branch(context).filter(|_| !find_cards(context).is_empty()));
    let gpu = gpu_verdict(&deciding).any();
    deciding.extend(check_amd_render_node(context));
    deciding.extend(check_amd_memory(context));
    deciding.extend([check_ram(context), check_disk(context)]);
    HostReport { checks: deciding, gpu }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::amd::stand_in::{fake_graphics, fake_render_node};
    use crate::setup::cards::stand_in::{NAMES, fake_pci};
    use crate::testing::{Scratch, scratch_context};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn only_a_driver_of_another_branch_is_a_branch_failure() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.driver_version_file = scratch.path("nothing");
        assert!(check_driver_branch(&context).is_none(), "the driver check says when none is loaded");
        context.driver_version_file = scratch.write("version", "615.90.01\n");
        assert!(check_driver_branch(&context).is_none(), "another point release only warns");
        fs::write(&context.driver_version_file, "620.70.06\n").unwrap();
        assert!(check_driver_branch(&context).is_none(), "the backend has a profile for 620");
        fs::write(&context.driver_version_file, "999.11\n").unwrap();
        let found = check_driver_branch(&context).unwrap();
        assert!(found.state == CheckState::Failed && found.text.contains("999"));
    }

    #[test]
    fn a_failed_host_check_turns_the_gpu_off_and_room_does_not() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.driver_version_file = scratch.write("version", "999.11\n");
        context.pci_names = scratch.write("pci.ids", NAMES);
        fake_pci(&scratch, "0000:01:00.0", 0x2704, "0x10de", "0x030000");
        let report = check_host(&context);
        assert!(!report.gpu);
        assert!(report.checks.iter().any(|check| check.title == "Host driver branch"));
    }

    #[test]
    fn the_card_check_says_what_is_known() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.pci_devices = scratch.path("pci");
        context.pci_names = scratch.write("pci.ids", NAMES);
        context.drm_class = scratch.path("drm");
        assert_eq!(check_card(&context), None);
        let found = check_cards(&context);
        assert!(
            found.len() == 1
                && found[0].state == CheckState::Failed
                && found[0].text.contains("No NVIDIA, AMD or Intel card")
                && found[0].fix.contains("AMD")
        );
        assert!(!gpu_verdict(&found).any());
        fake_pci(&scratch, "0000:01:00.0", 0x2BB1, "0x10de", "0x030000");
        let found = check_card(&context).unwrap();
        assert!(found.state == CheckState::Failed && found.text.contains("Not supported"));
        assert_eq!(found.part, Part::Nvidia);
        fake_pci(&scratch, "0000:02:00.0", 0x2C05, "0x10de", "0x030000");
        let found = check_card(&context).unwrap();
        assert_eq!(found.state, CheckState::Warning);
        assert!(found.text.contains("Experimental") && found.text.contains("GB203") && !found.fix.is_empty());
        fake_pci(&scratch, "0000:03:00.0", 0x2B85, "0x10de", "0x030000");
        assert_eq!(check_card(&context).unwrap().state, CheckState::Ok);
    }

    #[test]
    fn the_cards_testers_ran_are_tested() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.pci_devices = scratch.path("pci");
        context.pci_names = scratch.write("pci.ids", NAMES);
        for device in [0x2C02, 0x2704, 0x2204, 0x21C4] {
            fake_pci(&scratch, "0000:01:00.0", device, "0x10de", "0x030000");
            let found = check_card(&context).unwrap();
            assert_eq!(found.state, CheckState::Ok, "{device:04x}: {}", found.text);
        }
    }

    #[test]
    fn an_amd_card_is_judged_by_its_graphics_generation() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.pci_devices = scratch.path("pci");
        context.drm_class = scratch.path("drm");
        fake_render_node(&scratch, 128, "0000:7c:00.0", "0x1002", 0x67DF);
        let found = check_cards(&context);
        assert_eq!(found.len(), 1, "an AMD card alone is no failure of NVIDIA's");
        assert!(found[0].state == CheckState::Failed && found[0].fix.contains("Polaris"), "{found:?}");
        fake_graphics(&scratch, "0000:7c:00.0", 9);
        assert_eq!(check_cards(&context)[0].state, CheckState::Failed, "Vega is before RDNA");
        fake_graphics(&scratch, "0000:7c:00.0", 11);
        let found = check_cards(&context);
        assert!(found[0].state == CheckState::Warning && found[0].text.contains("Experimental"));
        assert_eq!(gpu_verdict(&found), GpuVerdict { nvidia: false, amd: true, intel: false });
        fake_render_node(&scratch, 129, "0000:03:00.0", "0x1002", 0x7550);
        fake_graphics(&scratch, "0000:03:00.0", 12);
        let found = check_cards(&context);
        assert!(found[0].state == CheckState::Ok && found[0].text.contains("at 0000:03:00.0"), "{found:?}");
    }

    #[test]
    fn one_makers_failure_leaves_the_other_makers_card() {
        let check = |state, part| Check { part, ..Check::new("x", state, "", "") };
        let both = [check(CheckState::Failed, Part::Nvidia), check(CheckState::Warning, Part::Amd)];
        assert_eq!(gpu_verdict(&both), GpuVerdict { nvidia: false, amd: true, intel: false });
        let pc_failed = [check(CheckState::Ok, Part::Nvidia), check(CheckState::Failed, Part::Pc)];
        assert!(!gpu_verdict(&pc_failed).any());
    }

    #[test]
    fn a_pc_with_only_an_amd_card_is_not_asked_about_nvidia() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.pci_devices = scratch.path("pci");
        context.drm_class = scratch.path("drm");
        context.driver_version_file = scratch.path("nothing");
        fake_render_node(&scratch, 128, "0000:7c:00.0", "0x1002", 0x13C0);
        fake_graphics(&scratch, "0000:7c:00.0", 10);
        assert!(nvidia_checks(&context).is_empty());
        let report = check_host(&context);
        assert!(report.checks.iter().all(|check| !check.title.contains("NVIDIA")), "{:?}", report.checks);
        assert_eq!(check_ubuntu(&context, &["sh"]).state, CheckState::Ok);
    }

    fn with_render_nodes(scratch: &Scratch, nodes: &[(u32, &str, &str, u32)]) -> Context {
        let mut context = scratch_context(scratch);
        context.pci_devices = scratch.path("pci");
        context.drm_class = scratch.path("drm");
        context.pci_names = scratch.write("pci.ids", NAMES);
        context.driver_version_file = scratch.path("nothing");
        for (minor, pci, vendor, device) in nodes {
            fake_render_node(scratch, *minor, pci, vendor, *device);
        }
        context
    }

    #[test]
    fn a_pc_with_only_an_intel_card_can_run_windows_with_nothing_red_about_nvidia() {
        let scratch = Scratch::new();
        let mut context = with_render_nodes(&scratch, &[(128, "0000:03:00.0", "0x8086", 0xE20B)]);
        // The verdict takes virtualization too, which the PC the tests run on may not have.
        context.kvm = scratch.write("kvm", "");
        let found = check_cards(&context);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].part, found[0].state), (Part::Intel, CheckState::Warning));
        assert!(found[0].text.contains("0000:03:00.0"), "{found:?}");
        assert_eq!(gpu_verdict(&found), GpuVerdict { nvidia: false, amd: false, intel: true });
        assert!(nvidia_checks(&context).is_empty());
        let report = check_host(&context);
        assert!(report.gpu);
        let red = report.checks.iter().filter(|check| check.state == CheckState::Failed);
        assert!(red.clone().all(|check| !check.title.contains("NVIDIA") && check.title != "Graphics card"));
    }

    #[test]
    fn an_intel_card_beside_an_amd_one_changes_nothing_and_an_intel_igpu_it_does_not_run_on_is_not_one() {
        let scratch = Scratch::new();
        let beside = with_render_nodes(
            &scratch,
            &[(128, "0000:7c:00.0", "0x1002", 0x13C0), (129, "0000:03:00.0", "0x8086", 0xE20B)],
        );
        fake_graphics(&scratch, "0000:7c:00.0", 10);
        assert!(check_cards(&beside).iter().all(|check| check.part == Part::Amd));
        let other = Scratch::new();
        let unsupported = with_render_nodes(&other, &[(128, "0000:00:02.0", "0x8086", 0x46A6)]);
        let found = check_cards(&unsupported);
        assert_eq!(found[0].state, CheckState::Failed);
        assert_eq!(found[0].text, "No NVIDIA, AMD or Intel card was found.");
        assert!(!gpu_verdict(&found).any());
    }

    #[test]
    fn an_intel_card_the_daemon_knows_on_i915_says_how_to_give_it_to_xe() {
        let scratch = Scratch::new();
        let context = with_render_nodes(&scratch, &[(128, "0000:00:02.0", "0x8086", 0x64A0)]);
        crate::setup::intel::tests::bind(&scratch, "0000:00:02.0", "i915");
        let found = check_cards(&context);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].part, found[0].state), (Part::Intel, CheckState::Failed));
        assert!(found[0].text.contains("on the i915 driver"), "{}", found[0].text);
        assert!(found[0].fix.contains("xe.force_probe=64a0"), "{}", found[0].fix);
        assert!(!gpu_verdict(&found).any());
    }

    #[test]
    fn an_experimental_card_is_said_before_a_vm_starts_and_a_tested_one_is_not() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.pci_devices = scratch.path("pci");
        context.pci_names = scratch.write("pci.ids", NAMES);
        context.driver_version_file = scratch.write("version", format!("{}\n", TESTED_DRIVERS[0]));
        assert_eq!(gpu_warning(&context), None);
        fake_pci(&scratch, "0000:01:00.0", 0x2C05, "0x10de", "0x030000");
        let said = gpu_warning(&context).unwrap();
        assert!(said.contains("card 2c05 is experimental"), "{said}");
        fake_pci(&scratch, "0000:00:01.0", 0x2B85, "0x10de", "0x030000");
        assert_eq!(gpu_warning(&context), None);
    }

    #[test]
    fn the_host_driver_is_checked_against_what_was_tested() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.driver_version_file = scratch.path("nothing");
        assert_eq!(check_host_driver(&context).state, CheckState::Failed);
        assert!(driver_warning(&context).unwrap().contains("not seem to be loaded"));
        for tested in TESTED_DRIVERS {
            context.driver_version_file = scratch.write("version", format!("{tested}\n"));
            assert_eq!(check_host_driver(&context).state, CheckState::Ok);
            assert_eq!(driver_warning(&context), None);
        }
        fs::write(&context.driver_version_file, "600.1\n").unwrap();
        let found = check_host_driver(&context);
        assert!(
            found.state == CheckState::Warning
                && found.text.contains("600.1")
                && found.fix.contains(TESTED_DRIVERS[0])
        );
        let warning = driver_warning(&context).unwrap();
        assert!(warning.contains("600.1") && warning.contains("620.70.06 and 615.78.08"), "{warning}");
    }

    #[test]
    fn an_amd_card_is_told_when_its_guest_memory_goes_the_slow_way() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.udmabuf = scratch.path("udmabuf");
        assert_eq!(check_amd_memory(&context), None, "a PC with no AMD card isn't asked");
        fake_render_node(&scratch, 128, "0000:03:00.0", "0x1002", 0x7550);
        let found = check_amd_memory(&context).unwrap();
        assert!(found.state == CheckState::Warning && found.fix.contains("module"));
        fs::write(&context.udmabuf, "").unwrap();
        fs::set_permissions(&context.udmabuf, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(check_amd_memory(&context).unwrap().state, CheckState::Ok);
        fs::set_permissions(&context.udmabuf, fs::Permissions::from_mode(0o000)).unwrap();
        let found = check_amd_memory(&context).unwrap();
        assert!(
            rustix::process::geteuid().is_root()
                || (found.state == CheckState::Warning && found.fix.contains("usermod"))
        );
    }

    #[test]
    fn a_missing_or_shut_render_node_is_warned_of_and_does_not_take_the_gpu_from_a_pc() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.render_nodes = scratch.path("dri");
        context.kvm = scratch.write("kvm", "");
        assert_eq!(check_amd_render_node(&context), None, "a PC with no AMD card isn't asked");
        fake_render_node(&scratch, 128, "0000:03:00.0", "0x1002", 0x7550);
        fake_graphics(&scratch, "0000:03:00.0", 12);
        let found = check_amd_render_node(&context).unwrap();
        assert_eq!(found.state, CheckState::Warning);
        assert!(found.text.ends_with("renderD128 does not exist."), "{}", found.text);
        let node = scratch.write("dri/renderD128", "");
        fs::set_permissions(&node, fs::Permissions::from_mode(0o600)).unwrap();
        let found = check_amd_render_node(&context).unwrap();
        assert_eq!(found.state, CheckState::Ok);
        assert_eq!(found.text, format!("{} can be opened.", node.display()));
        fs::set_permissions(&node, fs::Permissions::from_mode(0o000)).unwrap();
        let found = check_amd_render_node(&context).unwrap();
        assert!(
            rustix::process::geteuid().is_root()
                || (found.state == CheckState::Warning
                    && found.text.contains("renderD128")
                    && found.fix.contains("usermod"))
        );
        let report = check_host(&context);
        assert!(report.gpu, "a warning leaves the verdict alone");
        assert!(report.checks.iter().any(|check| check.title == "AMD card's render node"));
    }

    #[test]
    fn the_group_to_join_is_the_one_the_device_belongs_to() {
        let scratch = Scratch::new();
        let node = scratch.write("renderD128", "");
        let number = fs::metadata(&node).unwrap().gid();
        let groups = format!("root:x:0:\nother:x:{}:\nseat:x:{number}:someone\n", u64::from(number) + 1);
        assert!(number == ROOT_GROUP || group_of(&node, &groups).as_deref() == Some("seat"));
        assert_eq!(group_of(&node, "root:x:0:\n"), None, "a group the list lacks has no name");
        assert_eq!(group_of(&scratch.path("nothing"), &groups), None);
    }

    #[test]
    fn a_closed_udmabuf_does_not_take_the_gpu_from_a_pc() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.udmabuf = scratch.path("udmabuf");
        let before = check_host(&context).gpu;
        fake_render_node(&scratch, 128, "0000:03:00.0", "0x1002", 0x7550);
        let report = check_host(&context);
        assert_eq!(report.gpu, before);
        assert!(report.checks.iter().any(|check| check.title == "AMD card's guest memory"));
    }

    #[test]
    fn kvm_must_be_there_and_open() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.kvm = scratch.path("kvm");
        let found = check_kvm(&context);
        assert!(found.state == CheckState::Failed && found.fix.contains("firmware"));
        fs::write(&context.kvm, "").unwrap();
        fs::set_permissions(&context.kvm, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(check_kvm(&context).state, CheckState::Ok);
        fs::set_permissions(&context.kvm, fs::Permissions::from_mode(0o000)).unwrap();
        let found = check_kvm(&context);
        assert!(
            rustix::process::geteuid().is_root()
                || (found.state == CheckState::Failed && found.fix.contains("usermod"))
        );
    }

    #[test]
    fn qemu_must_run_and_says_why_if_it_cannot() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.paths.qemu = scratch.path("qemu");
        let found = check_qemu(&context);
        assert!(found.state == CheckState::Failed && found.fix.contains("qemu/README.md"));
        let script = "#!/bin/sh\necho 'error while loading shared libraries: libslirp.so.0' >&2\nexit 127\n";
        scratch.program("qemu", script);
        let found = check_qemu(&context);
        assert!(
            found.state == CheckState::Failed
                && found.text.contains("libslirp.so.0")
                && found.fix.contains("dnf install")
        );
        scratch.program("qemu", "#!/bin/sh\necho QEMU\n");
        assert_eq!(check_qemu(&context).state, CheckState::Ok);
    }

    #[test]
    fn nothing_about_ubuntu_stops_it_booting() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.driver_version_file = scratch.write("version", "615.78.08\n");
        assert_eq!(check_ubuntu(&context, &["sh"]).state, CheckState::Ok);
        fs::write(&context.driver_version_file, "615.90.01\n").unwrap();
        let found = check_ubuntu(&context, &["sh"]);
        assert!(found.state == CheckState::Warning && found.fix.contains("615.90.01"));
        fs::write(&context.driver_version_file, "620.70.06\n").unwrap();
        assert_eq!(check_ubuntu(&context, &["sh"]).state, CheckState::Ok);
        fs::write(&context.driver_version_file, "625.11\n").unwrap();
        let found = check_ubuntu(&context, &["sh"]);
        assert!(
            found.state == CheckState::Warning && found.text.contains("no GPU") && found.fix.contains("625")
        );
        let found = check_ubuntu(&context, &["no-such-tool"]);
        assert!(found.state == CheckState::Warning && found.text.contains("no-such-tool"));
    }

    #[test]
    fn missing_tools_are_named() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.paths.firmware.vars = scratch.path("nothing");
        let found = check_tools(&context, &["no-such-tool", "sh"]);
        assert_eq!(found.state, CheckState::Failed);
        assert!(
            found.text.contains("no-such-tool") && found.text.contains("OVMF") && !found.text.contains("sh,")
        );
    }

    #[test]
    fn the_ram_is_judged_by_what_a_gpu_machine_could_have() {
        let states = [
            (8, CheckState::Failed),
            (11, CheckState::Warning),
            (16, CheckState::Warning),
            (32, CheckState::Ok),
        ];
        for (total_gib, state) in states {
            let found = judge_ram(&RamOffer::new(total_gib * crate::host::GIB, 0));
            assert_eq!(found.state, state, "{total_gib}");
            assert!(found.text.contains(&format!("This PC has {total_gib} GiB of RAM")), "{}", found.text);
            assert!(!found.fix.contains("/dev/shm") && !found.text.contains("/dev/shm"));
        }
    }

    #[test]
    fn the_driver_needs_room_too() {
        let place = Path::new("/");
        assert_eq!(judge_disk(INSTALL_ROOM_GIB as f64, place).state, CheckState::Failed);
        assert_eq!(judge_disk((INSTALL_ROOM_GIB + DRIVER_ROOM_GIB) as f64, place).state, CheckState::Ok);
    }

    #[test]
    fn the_nearest_folder_is_found() {
        let scratch = Scratch::new();
        assert_eq!(existing_parent(&scratch.path("a/b/store")), scratch.root());
    }
}
