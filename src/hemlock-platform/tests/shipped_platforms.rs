//! Checks over the platform manifests this repo actually ships.
//!
//! `hemlockctl platform lint` covers the structural rules; these cover the
//! facts lint cannot express — above all the AS4610's faceplate → SDK port
//! map, which is the one input to that port that cannot be re-fetched from
//! a public repository. It came from a Cumulus Linux `porttab` read off the
//! hardware, so a silent edit here would mis-cable a rack with no
//! compile-time or lint-time complaint.

use std::path::PathBuf;

use hemlock_platform::{lint, Platform};

fn platform(id: &str) -> Platform {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../platforms")
        .join(id);
    Platform::load(&dir).unwrap_or_else(|e| panic!("loading {id}: {e}"))
}

fn assert_lints_clean(id: &str) {
    let p = platform(id);
    let report = lint::lint(&p);
    let errors: Vec<&str> = report
        .diagnostics
        .iter()
        .filter(|d| d.severity == lint::Severity::Error)
        .map(|d| d.message.as_str())
        .collect();
    assert!(errors.is_empty(), "{id} lint errors: {errors:#?}");
}

#[test]
fn shipped_manifests_lint_without_errors() {
    for id in ["cel-e1031", "accton-as4610-54", "_template"] {
        assert_lints_clean(id);
    }
}

/// The AS4610's whole port table, checked against the two independent
/// facts it was built from: the porttab's SDK names, and the rule that a
/// `geN`/`xeN` name maps to a logical port number.
#[test]
fn as4610_lanes_agree_with_their_sdk_names() {
    let p = platform("accton-as4610-54");
    assert_eq!(p.ports.len(), 52, "48x1G + 4x10G front panel");

    for port in &p.ports {
        let sdk = port
            .sdk_name
            .as_deref()
            .unwrap_or_else(|| panic!("{} has no sdk_name", port.name));
        assert_eq!(port.lanes.len(), 1, "{} is single-lane", port.name);
        let lane = port.lanes[0];

        // config.bcm's bitmaps: pbmp_xport_ge = 0x3fffffffffffe puts geN
        // at logical N+1; pbmp_xport_xe = 0xfc000000000000 puts xeN at
        // logical 50+N. If the manifest's lane and its SDK name ever
        // disagree, one of them was edited without the other.
        let expected = if let Some(n) = sdk.strip_prefix("ge") {
            n.parse::<u32>().expect("geN") + 1
        } else if let Some(n) = sdk.strip_prefix("xe") {
            n.parse::<u32>().expect("xeN") + 50
        } else {
            panic!("{}: unexpected sdk_name {sdk:?}", port.name);
        };
        assert_eq!(
            lane, expected,
            "{} ({sdk}): lane {lane} does not match the logical port {sdk} implies",
            port.name
        );
    }
}

/// The two entries verified against the physical box, and the shape of
/// the permutation they belong to. Copied from the Cumulus porttab; see
/// platforms/accton-as4610-54/README.md for the raw table.
#[test]
fn as4610_port_map_matches_the_porttab() {
    let p = platform("accton-as4610-54");
    let sdk_of = |index: u32| {
        p.ports
            .iter()
            .find(|port| port.index == index)
            .and_then(|port| port.sdk_name.as_deref())
            .unwrap_or_else(|| panic!("no port at faceplate index {index}"))
    };

    // Hardware-verified: faceplate 1 is ge25 and faceplate 2 is ge24.
    assert_eq!(sdk_of(1), "ge25");
    assert_eq!(sdk_of(2), "ge24");
    // The block boundaries of the two halves.
    assert_eq!(sdk_of(24), "ge47");
    assert_eq!(sdk_of(25), "ge1");
    assert_eq!(sdk_of(26), "ge0");
    assert_eq!(sdk_of(48), "ge23");
    // SFP+ uplinks are in order.
    assert_eq!(sdk_of(49), "xe0");
    assert_eq!(sdk_of(52), "xe3");

    // Faceplate 1-24 live on ge24-47 and 25-48 on ge0-23, and together
    // they are a permutation of ge0-47 — no port doubled, none dropped.
    let ge = |index: u32| -> u32 {
        sdk_of(index)
            .strip_prefix("ge")
            .expect("copper port")
            .parse()
            .expect("geN")
    };
    let mut upper: Vec<u32> = (1..=24).map(ge).collect();
    let mut lower: Vec<u32> = (25..=48).map(ge).collect();
    upper.sort_unstable();
    lower.sort_unstable();
    assert_eq!(upper, (24..=47).collect::<Vec<_>>());
    assert_eq!(lower, (0..=23).collect::<Vec<_>>());

    // Within every block of four the first pair is swapped:
    // (b+1, b, b+2, b+3).
    for block in 0..12 {
        let first = block * 4 + 1;
        let base = ge(first + 1);
        assert_eq!(
            [ge(first), ge(first + 1), ge(first + 2), ge(first + 3)],
            [base + 1, base, base + 2, base + 3],
            "faceplate {first}-{} is not (b+1, b, b+2, b+3)",
            first + 3
        );
    }
}

/// The AS4610 is the first board that is not x86 and not SAI; these are
/// the manifest fields the rest of the pipeline keys off.
#[test]
fn as4610_declares_an_arm_openbcm_platform() {
    use hemlock_platform::schema::{AsicAttach, SaiBackendKind};

    let p = platform("accton-as4610-54");
    let m = &p.manifest;
    assert_eq!(m.platform.onie_machine, "arm-accton-as4610-54-r0");
    assert_eq!(m.platform.cpu_arch, "armhf");
    // No PCI device to find: a pcie probe would let --auto-mock mock a
    // live switch.
    assert_eq!(m.platform.asic_attach, AsicAttach::Soc);
    assert_eq!(m.sai.backend, SaiBackendKind::Openbcm);
    assert!(m.sai.shim_path.is_some() && m.sai.abi_major.is_some());
    // There is no armhf libsaibcm; carrying a SAI pin would imply one.
    assert!(m.sai.version_pin.is_none() && m.sai.libsai_path.is_none());

    // OpenBCM's knet-cb has no psample path, unlike SONiC's fork.
    assert_eq!(
        m.kernel.required_modules,
        ["linux-kernel-bde", "linux-user-bde", "linux-bcm-knet"]
    );
}
