//! Pure-Rust mock SAI backend.
//!
//! Behaves like a well-mannered ASIC: `create_switch` "boots" it, `ports()`
//! returns one port per platform port-table entry (SAI ports are created
//! from config.bcm on real hardware, so the mock synthesizes the same
//! outcome from the manifest), and admin-state changes produce the
//! corresponding oper-status notifications — links come up when enabled,
//! exactly what the layers above need for end-to-end testing.

use hemlock_platform::PortDef;
use tokio::sync::mpsc;

use crate::{PortId, SaiBackend, SaiError, SaiEvent, SaiPort, SwitchInfo};

/// Synthetic OIDs: obviously fake, stable, and readable in logs.
const MOCK_SWITCH_OID: u64 = 0x2100_0000_0000_0000;
const MOCK_PORT_OID_BASE: u64 = 0x2100_0000_0000_1000;

pub struct MockSai {
    port_table: Vec<PortDef>,
    ports: Vec<SaiPort>,
    created: bool,
    events_tx: mpsc::UnboundedSender<SaiEvent>,
    events_rx: Option<mpsc::UnboundedReceiver<SaiEvent>>,
}

impl MockSai {
    /// Build a mock ASIC whose config.bcm "created" exactly the ports in
    /// the platform port table.
    pub fn new(port_table: Vec<PortDef>) -> Self {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        Self {
            port_table,
            ports: Vec::new(),
            created: false,
            events_tx,
            events_rx: Some(events_rx),
        }
    }

    fn port_mut(&mut self, id: PortId) -> Result<&mut SaiPort, SaiError> {
        self.ports
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or(SaiError::UnknownPort(id))
    }
}

impl SaiBackend for MockSai {
    fn name(&self) -> String {
        "mock".into()
    }

    fn create_switch(&mut self) -> Result<SwitchInfo, SaiError> {
        if self.created {
            return Err(SaiError::Other("switch already created".into()));
        }
        self.created = true;
        self.ports = self
            .port_table
            .iter()
            .enumerate()
            .map(|(i, def)| SaiPort {
                id: PortId(MOCK_PORT_OID_BASE + i as u64),
                lanes: def.lanes.clone(),
                speed_mbps: def.speed_mbps,
                admin_up: false,
                oper_up: false,
            })
            .collect();
        Ok(SwitchInfo {
            oid: MOCK_SWITCH_OID,
        })
    }

    fn ports(&mut self) -> Result<Vec<SaiPort>, SaiError> {
        if !self.created {
            return Err(SaiError::NoSwitch);
        }
        Ok(self.ports.clone())
    }

    fn set_port_admin_state(&mut self, id: PortId, up: bool) -> Result<(), SaiError> {
        if !self.created {
            return Err(SaiError::NoSwitch);
        }
        let tx = self.events_tx.clone();
        let port = self.port_mut(id)?;
        port.admin_up = up;
        // The mock's links follow admin state; notify like the real ASIC
        // would (from a callback thread, hence the channel).
        if port.oper_up != up {
            port.oper_up = up;
            let _ = tx.send(SaiEvent::PortOperStatus { port: id, up });
        }
        Ok(())
    }

    fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<SaiEvent>> {
        self.events_rx.take()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn port_table(n: usize) -> Vec<PortDef> {
        (0..n)
            .map(|i| PortDef {
                name: format!("Ethernet{i}"),
                index: i as u32 + 1,
                lanes: vec![i as u32 + 1],
                speed_mbps: 1000,
                alias: None,
                autoneg: false,
                media: None,
                breakout: vec![],
            })
            .collect()
    }

    #[test]
    fn lifecycle_create_enumerate_admin_up() {
        let mut sai = MockSai::new(port_table(52));
        assert!(matches!(sai.ports(), Err(SaiError::NoSwitch)));

        let info = sai.create_switch().unwrap();
        assert_eq!(info.oid, MOCK_SWITCH_OID);
        assert!(sai.create_switch().is_err(), "double create must fail");

        let ports = sai.ports().unwrap();
        assert_eq!(ports.len(), 52);
        assert!(ports.iter().all(|p| !p.admin_up && !p.oper_up));

        let mut events = sai.take_events().unwrap();
        assert!(sai.take_events().is_none(), "receiver is single-take");

        let first = ports[0].id;
        sai.set_port_admin_state(first, true).unwrap();
        let ports = sai.ports().unwrap();
        assert!(ports[0].admin_up && ports[0].oper_up);

        match events.try_recv().unwrap() {
            SaiEvent::PortOperStatus { port, up } => {
                assert_eq!(port, first);
                assert!(up);
            }
        }

        // Re-applying the same state emits no duplicate event.
        sai.set_port_admin_state(first, true).unwrap();
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn unknown_port_is_an_error() {
        let mut sai = MockSai::new(port_table(1));
        sai.create_switch().unwrap();
        assert!(matches!(
            sai.set_port_admin_state(PortId(0xdead), true),
            Err(SaiError::UnknownPort(_))
        ));
    }
}
