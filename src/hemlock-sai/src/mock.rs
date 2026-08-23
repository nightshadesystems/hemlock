//! Pure-Rust mock SAI backend.
//!
//! Behaves like a well-mannered ASIC: `create_switch` "boots" it, `ports()`
//! returns one port per platform port-table entry (SAI ports are created
//! from config.bcm on real hardware, so the mock synthesizes the same
//! outcome from the manifest), and admin-state changes produce the
//! corresponding oper-status notifications — links come up when enabled,
//! exactly what the layers above need for end-to-end testing.

use std::collections::HashMap;

use hemlock_platform::PortDef;
use tokio::sync::mpsc;

use crate::{
    IpPrefix, Oid, PortCounters, PortId, QueueCounters, RouteTarget, SaiBackend, SaiError,
    SaiEvent, SaiPort, SwitchInfo,
};

/// Synthetic OIDs: obviously fake, stable, and readable in logs.
const MOCK_SWITCH_OID: u64 = 0x2100_0000_0000_0000;
const MOCK_PORT_OID_BASE: u64 = 0x2100_0000_0000_1000;
const MOCK_HOSTIF_OID_BASE: u64 = 0x2100_0000_0000_2000;
const MOCK_RIF_OID_BASE: u64 = 0x2100_0000_0000_3000;

pub struct MockSai {
    port_table: Vec<PortDef>,
    ports: Vec<SaiPort>,
    created: bool,
    events_tx: mpsc::UnboundedSender<SaiEvent>,
    events_rx: Option<mpsc::UnboundedReceiver<SaiEvent>>,
    /// L3 model, mirroring what the real ASIC tracks: punt install,
    /// per-port hostif netdev names, per-port RIFs (a routed port has
    /// left the default 802.1Q bridge), and the default-VR route table.
    punt_installed: bool,
    hostifs: HashMap<PortId, (Oid, String)>,
    rifs: HashMap<PortId, Oid>,
    routes: HashMap<IpPrefix, RouteTarget>,
    next_oid: u64,
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
            punt_installed: false,
            hostifs: HashMap::new(),
            rifs: HashMap::new(),
            routes: HashMap::new(),
            next_oid: 0,
        }
    }

    fn port_mut(&mut self, id: PortId) -> Result<&mut SaiPort, SaiError> {
        self.ports
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or(SaiError::UnknownPort(id))
    }

    fn require_switch(&self) -> Result<(), SaiError> {
        if self.created {
            Ok(())
        } else {
            Err(SaiError::NoSwitch)
        }
    }

    fn require_port(&self, id: PortId) -> Result<(), SaiError> {
        if self.ports.iter().any(|p| p.id == id) {
            Ok(())
        } else {
            Err(SaiError::UnknownPort(id))
        }
    }

    fn alloc(&mut self, base: u64) -> Oid {
        self.next_oid += 1;
        Oid(base + self.next_oid)
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

    fn port_counters(&mut self, id: PortId) -> Result<PortCounters, SaiError> {
        if !self.created {
            return Err(SaiError::NoSwitch);
        }
        // The mock ASIC forwards nothing, so its counters honestly read 0.
        self.port_mut(id)?;
        Ok(PortCounters::default())
    }

    fn port_queue_counters(&mut self, id: PortId) -> Result<Vec<QueueCounters>, SaiError> {
        if !self.created {
            return Err(SaiError::NoSwitch);
        }
        self.port_mut(id)?;
        Ok(Vec::new())
    }

    fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<SaiEvent>> {
        self.events_rx.take()
    }

    fn setup_host_punt(&mut self) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.punt_installed {
            return Err(SaiError::Other("host punt already installed".into()));
        }
        self.punt_installed = true;
        Ok(())
    }

    fn create_hostif(&mut self, port: PortId, name: &str) -> Result<Oid, SaiError> {
        self.require_switch()?;
        self.require_port(port)?;
        if self.hostifs.contains_key(&port) {
            return Err(SaiError::Other(format!("port {port} already has a hostif")));
        }
        let oid = self.alloc(MOCK_HOSTIF_OID_BASE);
        self.hostifs.insert(port, (oid, name.to_string()));
        Ok(oid)
    }

    fn create_router_interface(&mut self, port: PortId) -> Result<Oid, SaiError> {
        self.require_switch()?;
        self.require_port(port)?;
        if self.rifs.contains_key(&port) {
            return Err(SaiError::Other(format!("port {port} already has a RIF")));
        }
        let oid = self.alloc(MOCK_RIF_OID_BASE);
        self.rifs.insert(port, oid);
        Ok(oid)
    }

    fn remove_router_interface(&mut self, port: PortId, rif: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        match self.rifs.get(&port) {
            Some(have) if *have == rif => {
                self.rifs.remove(&port);
                Ok(())
            }
            _ => Err(SaiError::Other(format!("port {port} has no RIF {rif}"))),
        }
    }

    fn create_route(&mut self, dest: IpPrefix, target: RouteTarget) -> Result<(), SaiError> {
        self.require_switch()?;
        if let RouteTarget::Rif(rif) = target {
            if !self.rifs.values().any(|r| *r == rif) {
                return Err(SaiError::Other(format!("no such RIF {rif}")));
            }
        }
        // Like the real ASIC: creating an existing destination fails
        // (callers replace via remove + create).
        if self.routes.insert(dest, target).is_some() {
            return Err(SaiError::Other(format!(
                "route {}/{} exists",
                dest.0, dest.1
            )));
        }
        Ok(())
    }

    fn remove_route(&mut self, dest: IpPrefix) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.routes.remove(&dest).is_none() {
            return Err(SaiError::Other(format!("no route {}/{}", dest.0, dest.1)));
        }
        Ok(())
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
                phy_model: None,
                supported_modes: vec![],
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

    #[test]
    fn l3_lifecycle_rif_and_routes() {
        let mut sai = MockSai::new(port_table(2));
        assert!(matches!(sai.setup_host_punt(), Err(SaiError::NoSwitch)));
        sai.create_switch().unwrap();
        sai.setup_host_punt().unwrap();
        assert!(sai.setup_host_punt().is_err(), "punt installs once");

        let port = sai.ports().unwrap()[0].id;
        let hostif = sai.create_hostif(port, "Ethernet0").unwrap();
        assert!(hostif.0 >= MOCK_HOSTIF_OID_BASE);
        assert!(sai.create_hostif(port, "Ethernet0").is_err());

        let rif = sai.create_router_interface(port).unwrap();
        assert!(
            sai.create_router_interface(port).is_err(),
            "one RIF per port"
        );

        let addr: std::net::IpAddr = "10.42.10.9".parse().unwrap();
        let subnet: std::net::IpAddr = "10.42.10.0".parse().unwrap();
        sai.create_route((addr, 32), RouteTarget::Cpu).unwrap();
        sai.create_route((subnet, 24), RouteTarget::Rif(rif))
            .unwrap();
        assert!(
            sai.create_route((addr, 32), RouteTarget::Cpu).is_err(),
            "duplicate destination fails like the real ASIC"
        );
        assert!(
            sai.create_route((subnet, 25), RouteTarget::Rif(Oid(0xbad)))
                .is_err(),
            "routes must target a live RIF"
        );

        sai.remove_route((addr, 32)).unwrap();
        sai.remove_route((subnet, 24)).unwrap();
        assert!(sai.remove_route((subnet, 24)).is_err());

        sai.remove_router_interface(port, rif).unwrap();
        assert!(sai.remove_router_interface(port, rif).is_err());
        // Back to L2: a fresh RIF can be created again.
        sai.create_router_interface(port).unwrap();
    }
}
