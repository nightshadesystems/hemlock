//! Golden-file tests: every renderer's output is pinned byte-for-byte to
//! the reference outputs in `tests/golden/` (the normative EOS-parity
//! spec, adapted to Hemlock conventions).

use super::fixtures as fx;
use super::model::Interface;
use super::render;
use super::render::summary::StatusFilter;

/// Golden files are authored with LF; normalize in case a checkout
/// rewrote them with CRLF.
fn norm(text: &str) -> String {
    text.replace("\r\n", "\n")
}

#[track_caller]
fn assert_golden(rendered: &str, golden: &str) {
    let golden = norm(golden);
    if rendered != golden {
        // Line-by-line diff beats a wall of text when a column drifts.
        for (n, (got, want)) in rendered.lines().zip(golden.lines()).enumerate() {
            assert_eq!(got, want, "first mismatch at line {}", n + 1);
        }
        assert_eq!(
            rendered.lines().count(),
            golden.lines().count(),
            "line count mismatch"
        );
        assert_eq!(rendered, golden, "whitespace-only mismatch");
    }
}

fn counter_set() -> Vec<Interface> {
    // Po1 and Lo0 prove the counters-table filters (port-channels and
    // virtual interfaces are excluded).
    vec![
        fx::et1(),
        fx::et2(),
        fx::et3(),
        fx::et49(),
        fx::et50(),
        fx::ma1(),
        fx::po1(),
        fx::lo0(),
    ]
}

#[test]
fn detail() {
    let ifs = vec![
        fx::et1(),
        fx::et2(),
        fx::et3(),
        fx::ma1(),
        fx::lo0(),
        fx::vl10(),
        fx::po1(),
    ];
    assert_golden(
        &render::detail::render(&ifs),
        include_str!("../../tests/golden/detail.txt"),
    );
}

#[test]
fn description() {
    let ifs = vec![
        fx::et1(),
        fx::et2(),
        fx::et3(),
        fx::et49(),
        fx::et50(),
        fx::lo0(),
        fx::ma1(),
        fx::po1(),
        fx::vl10(),
    ];
    assert_golden(
        &render::summary::description(&ifs),
        include_str!("../../tests/golden/description.txt"),
    );
}

#[test]
fn status() {
    let ifs = vec![
        fx::et1(),
        fx::et2(),
        fx::et3(),
        fx::et49(),
        fx::et50(),
        fx::et51(),
        fx::ma1(),
        fx::po1(),
        fx::lo0(),  // filtered: not port-like
        fx::vl10(), // filtered: not port-like
    ];
    assert_golden(
        &render::summary::status(&ifs, StatusFilter::All),
        include_str!("../../tests/golden/status.txt"),
    );
}

#[test]
fn status_errdisabled() {
    let ifs = vec![fx::et1(), fx::et7(), fx::et8(), fx::ma1()];
    assert_golden(
        &render::summary::status(&ifs, StatusFilter::ErrDisabled),
        include_str!("../../tests/golden/status_errdisabled.txt"),
    );
}

#[test]
fn status_filters_rows() {
    let ifs = vec![fx::et1(), fx::et51()];
    let out = render::summary::status(&ifs, StatusFilter::NotConnect);
    assert!(out.contains("Et51"));
    assert!(!out.contains("Et1 "));
    // Header is unchanged by filters.
    assert!(out.starts_with("Port       Name"));
}

#[test]
fn counters() {
    assert_golden(
        &render::counters::counters(&counter_set()),
        include_str!("../../tests/golden/counters.txt"),
    );
}

#[test]
fn counters_errors() {
    assert_golden(
        &render::counters::errors(&counter_set()),
        include_str!("../../tests/golden/counters_errors.txt"),
    );
}

#[test]
fn counters_discards() {
    assert_golden(
        &render::counters::discards(&counter_set()),
        include_str!("../../tests/golden/counters_discards.txt"),
    );
}

#[test]
fn counters_rates() {
    assert_golden(
        &render::counters::rates(&counter_set()),
        include_str!("../../tests/golden/counters_rates.txt"),
    );
}

#[test]
fn counters_queue() {
    let ifs = vec![fx::et1()];
    assert_golden(
        &render::counters::queues(&ifs),
        include_str!("../../tests/golden/counters_queue.txt"),
    );
}

#[test]
fn counters_bins() {
    let ifs = vec![fx::et1(), fx::ma1()]; // Ma1 has no bins: omitted
    assert_golden(
        &render::counters::bins(&ifs),
        include_str!("../../tests/golden/counters_bins.txt"),
    );
}

#[test]
fn transceiver() {
    let xcvrs = vec![fx::xcvr49(), fx::xcvr50(), fx::xcvr51()];
    assert_golden(
        &render::transceiver::summary(&xcvrs),
        include_str!("../../tests/golden/transceiver.txt"),
    );
}

#[test]
fn transceiver_detail() {
    let xcvrs = vec![fx::xcvr49()];
    assert_golden(
        &render::transceiver::detail(&xcvrs),
        include_str!("../../tests/golden/transceiver_detail.txt"),
    );
}

#[test]
fn transceiver_properties() {
    let xcvrs = vec![fx::xcvr49()];
    let ifs = vec![fx::et49()];
    assert_golden(
        &render::transceiver::properties(&xcvrs, &ifs),
        include_str!("../../tests/golden/transceiver_properties.txt"),
    );
}

#[test]
fn transceiver_eeprom() {
    let xcvrs = vec![fx::xcvr49()];
    assert_golden(
        &render::transceiver::eeprom(&xcvrs),
        include_str!("../../tests/golden/transceiver_eeprom.txt"),
    );
}

#[test]
fn capabilities() {
    let ifs = vec![fx::et1(), fx::et49()];
    assert_golden(
        &render::phys::capabilities(&ifs),
        include_str!("../../tests/golden/capabilities.txt"),
    );
}

#[test]
fn flowcontrol() {
    let ifs = vec![fx::et1(), fx::et2(), fx::et49(), fx::et50()];
    assert_golden(
        &render::phys::flowcontrol(&ifs),
        include_str!("../../tests/golden/flowcontrol.txt"),
    );
}

#[test]
fn negotiation() {
    let ifs = vec![fx::et1(), fx::et49()];
    assert_golden(
        &render::phys::negotiation(&ifs),
        include_str!("../../tests/golden/negotiation.txt"),
    );
}

#[test]
fn negotiation_detail() {
    let ifs = vec![fx::et1()];
    assert_golden(
        &render::phys::negotiation_detail(&ifs),
        include_str!("../../tests/golden/negotiation_detail.txt"),
    );
}

#[test]
fn phy_detail() {
    let ifs = vec![fx::et1()];
    assert_golden(
        &render::phys::phy_detail(&ifs, &fx::context()),
        include_str!("../../tests/golden/phy_detail.txt"),
    );
}

#[test]
fn mac() {
    let ifs = vec![
        fx::et1(),
        fx::et2(),
        fx::et3(),
        fx::et49(),
        fx::et50(),
        fx::ma1(), // filtered: not an Ethernet port
    ];
    assert_golden(
        &render::phys::mac(&ifs),
        include_str!("../../tests/golden/mac.txt"),
    );
}

#[test]
fn mac_detail() {
    let ifs = vec![fx::et49()];
    assert_golden(
        &render::phys::mac_detail(&ifs),
        include_str!("../../tests/golden/mac_detail.txt"),
    );
}

#[test]
fn switchport() {
    let ifs = vec![fx::et1(), fx::et2(), fx::et6(), fx::ma1()];
    assert_golden(
        &render::l2::switchport(&ifs, &fx::context()),
        include_str!("../../tests/golden/switchport.txt"),
    );
}

#[test]
fn trunk() {
    let ifs = vec![fx::et1(), fx::et2(), fx::ma1(), fx::po1()];
    assert_golden(
        &render::l2::trunk(&ifs, &fx::context()),
        include_str!("../../tests/golden/trunk.txt"),
    );
}

#[test]
fn vlans() {
    let ifs = vec![fx::et1(), fx::et2(), fx::ma1(), fx::po1(), fx::vl10()];
    assert_golden(
        &render::l2::vlans(&ifs, &fx::context()),
        include_str!("../../tests/golden/vlans.txt"),
    );
}
