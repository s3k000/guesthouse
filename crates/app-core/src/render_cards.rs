//! The cards in this PC that have a render node, by maker. A card is found by its vendor number in the
//! DRM class, as render node numbers move between boots. Which of them a guest may be given is up to
//! the caller: a Linux guest takes AMD's, a Windows guest's GPU-PV takes any maker's it can be set up for.

use crate::context::Context;
use crate::gpu_maker::GpuMaker;
use crate::machine::sorted_names;
use crate::setup::cards::product_name;
use crate::usb::sysfs_text;
use std::fs;
use std::path::{Path, PathBuf};

const RENDER_PREFIX: &str = "renderD";
const DRM_DEVICE_LINK: &str = "device";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderCard {
    /// Where the card is on the PCI bus, as sysfs names it.
    pub pci: String,
    /// Its render node, as `/dev/dri/renderD128`.
    pub render_node: PathBuf,
    pub name: String,
    /// Its PCI device id.
    pub device: u32,
}

fn hex_number(text: &str) -> Option<u32> {
    u32::from_str_radix(text.trim_start_matches("0x"), 16).ok()
}

/// The maker's name for a device, from pci.ids, with the maker in front where the name lacks it.
fn named(names: &Path, maker: GpuMaker, number: u32) -> String {
    // Where the maker's devices start in pci.ids.
    let names_start = format!("\n{:04x}  ", maker.vendor());
    let product = product_name(&names_start, number, names).unwrap_or_else(|| format!("card {number:04x}"));
    // Intel's own product names say who made them already.
    if product.starts_with(maker.name()) { product } else { format!("{} {product}", maker.name()) }
}

fn card_of(context: &Context, node: &str, maker: GpuMaker) -> Option<RenderCard> {
    let device = context.drm_class.join(node).join(DRM_DEVICE_LINK);
    if hex_number(&sysfs_text(&device.join("vendor"))) != Some(u32::from(maker.vendor())) {
        return None;
    }
    let pci = fs::read_link(&device).ok()?.file_name()?.to_string_lossy().into_owned();
    let number = hex_number(&sysfs_text(&device.join("device")))?;
    let render_node = context.render_nodes.join(node);
    Some(RenderCard { pci, render_node, name: named(&context.pci_names, maker, number), device: number })
}

/// The Intel card a recording stands in for, when this process was started with one, for a lab run
/// on a PC without the card. It has no render node, and its name says it is recorded. A recording
/// that can't be read is no card: the host facts of a bug report say why.
fn recorded_intel_card(context: &Context) -> Option<RenderCard> {
    let text = fs::read_to_string(context.recorded_intel_card.as_ref()?).ok()?;
    let card = guesthouse_backend_intel::recording::read(&text).ok()?;
    let device = u32::from(card.pci.device);
    Some(RenderCard {
        pci: card.address?,
        render_node: PathBuf::new(),
        name: format!("{}, {RECORDED}", named(&context.pci_names, GpuMaker::Intel, device)),
        device,
    })
}

/// What a recorded card's name ends with.
pub const RECORDED: &str = "recorded, no card";

/// The cards of `maker` with a render node, by PCI address. Any of them will do, iGPU or not, and so
/// will the one that drives the host's desktop.
pub fn render_cards(context: &Context, maker: GpuMaker) -> Vec<RenderCard> {
    let nodes = sorted_names(&context.drm_class).into_iter().filter(|name| name.starts_with(RENDER_PREFIX));
    let mut cards: Vec<RenderCard> = nodes.filter_map(|node| card_of(context, &node, maker)).collect();
    if maker == GpuMaker::Intel {
        cards.extend(recorded_intel_card(context));
    }
    cards.sort_by(|one, other| one.pci.cmp(&other.pci));
    cards
}

/// The card of `maker` at this address, if it's still in this PC.
pub fn render_card(context: &Context, maker: GpuMaker, pci: &str) -> Option<RenderCard> {
    render_cards(context, maker).into_iter().find(|card| card.pci == pci)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::linux::amd::stand_in::fake_render_node;
    use crate::testing::{Scratch, scratch_context};

    const NAMES: &str = "8085  Other\n\t64a0  Not this one\n8086  Intel Corporation\n\
                         \t64a0  Lunar Lake [Intel Arc Graphics 130V / 140V]\n1002  Advanced Micro Devices, Inc. [AMD/ATI]\n\
                         \t13c0  Granite Ridge [Radeon 610M]\n";

    fn context(scratch: &Scratch) -> Context {
        let mut context = scratch_context(scratch);
        context.drm_class = scratch.path("drm");
        context.pci_names = scratch.write("pci.ids", NAMES);
        context
    }

    #[test]
    fn a_card_is_found_under_its_own_maker_only() {
        let scratch = Scratch::new();
        fake_render_node(&scratch, 128, "0000:00:02.0", "0x8086", 0x64a0);
        fake_render_node(&scratch, 129, "0000:7c:00.0", "0x1002", 0x13c0);
        fake_render_node(&scratch, 130, "0000:01:00.0", "0x10de", 0x2b85);
        let context = context(&scratch);
        let intel = render_cards(&context, GpuMaker::Intel);
        assert_eq!(intel.len(), 1);
        assert_eq!((intel[0].pci.as_str(), intel[0].device), ("0000:00:02.0", 0x64A0));
        assert_eq!(intel[0].name, "Intel Arc Graphics 130V / 140V");
        assert_eq!(intel[0].render_node, Path::new("/dev/dri/renderD128"));
        let amd = render_cards(&context, GpuMaker::Amd);
        assert_eq!(amd.iter().map(|card| card.pci.as_str()).collect::<Vec<_>>(), ["0000:7c:00.0"]);
    }

    /// The tester's Arc B580 as the Intel backend's test reads it.
    pub const ARC_B580: &str = include_str!("../../backend-intel/tests/cards/arc-b580.card");

    #[test]
    fn a_recorded_intel_card_is_listed_only_when_one_is_named_and_says_it_is_recorded() {
        let scratch = Scratch::new();
        fake_render_node(&scratch, 128, "0000:7c:00.0", "0x1002", 0x13c0);
        let mut context = context(&scratch);
        assert!(render_cards(&context, GpuMaker::Intel).is_empty(), "nothing unless it is asked for");
        context.recorded_intel_card = Some(scratch.write("b580.card", ARC_B580));
        let intel = render_cards(&context, GpuMaker::Intel);
        assert_eq!(intel.len(), 1);
        assert_eq!((intel[0].pci.as_str(), intel[0].device), ("0000:03:00.0", 0xE20B));
        assert!(intel[0].name.ends_with(RECORDED), "{}", intel[0].name);
        assert_eq!(render_cards(&context, GpuMaker::Amd).len(), 1, "another maker's list is as it was");
        assert!(crate::setup::intel::supported_intel_cards(&context).len() == 1);
        context.recorded_intel_card = Some(scratch.write("broken.card", "intel-probe 0.4.1\n"));
        assert!(render_cards(&context, GpuMaker::Intel).is_empty(), "a damaged one is no card");
    }
}
