//! `hemlockctl show ...` — read-only views of daemon state.

use anyhow::{Context, Result};
use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;

fn speed_str(mbps: u32) -> String {
    if mbps >= 1000 && mbps % 1000 == 0 {
        format!("{}G", mbps / 1000)
    } else {
        format!("{mbps}M")
    }
}

fn admin_str(state: i32) -> &'static str {
    match pb::AdminState::try_from(state) {
        Ok(pb::AdminState::Up) => "up",
        Ok(pb::AdminState::Down) => "down",
        _ => "?",
    }
}

fn oper_str(state: i32) -> &'static str {
    match pb::OperStatus::try_from(state) {
        Ok(pb::OperStatus::Up) => "up",
        Ok(pb::OperStatus::Down) => "down",
        _ => "?",
    }
}

pub async fn interfaces(endpoint: IpcEndpoint) -> Result<()> {
    let channel = endpoint.connect().await.context("connecting to syncd")?;
    let mut client = pb::syncd_client::SyncdClient::new(channel);
    let ports = client
        .list_ports(pb::ListPortsRequest {})
        .await?
        .into_inner()
        .ports;

    println!(
        "{:<12} {:>5} {:>6} {:>5} {:>4}  Description",
        "Interface", "Index", "Speed", "Admin", "Oper"
    );
    for p in ports {
        println!(
            "{:<12} {:>5} {:>6} {:>5} {:>4}  {}",
            p.name,
            p.index,
            speed_str(p.speed_mbps),
            admin_str(p.admin_state),
            oper_str(p.oper_status),
            p.description
        );
    }
    Ok(())
}

pub async fn switch(endpoint: IpcEndpoint) -> Result<()> {
    let channel = endpoint.connect().await.context("connecting to syncd")?;
    let mut client = pb::syncd_client::SyncdClient::new(channel);
    let info = client
        .get_switch_info(pb::GetSwitchInfoRequest {})
        .await?
        .into_inner();
    println!("Platform:   {}", info.platform_id);
    println!("Backend:    {}", info.backend);
    println!("Switch OID: {:#x}", info.switch_oid);
    println!("Ports:      {}", info.port_count);
    Ok(())
}

pub async fn environment(endpoint: IpcEndpoint) -> Result<()> {
    let channel = endpoint.connect().await.context("connecting to pmon")?;
    let mut client = pb::pmon_client::PmonClient::new(channel);
    let env = client
        .get_environment(pb::GetEnvironmentRequest {})
        .await?
        .into_inner();

    if !env.temperatures.is_empty() {
        println!("Temperatures:");
        for t in &env.temperatures {
            let flag = if t.celsius >= t.crit_celsius {
                "  CRIT"
            } else if t.celsius >= t.warn_celsius {
                "  WARN"
            } else {
                ""
            };
            println!(
                "  {:<28} {:>6.1} C  (warn {:.0}, crit {:.0}){flag}",
                t.name, t.celsius, t.warn_celsius, t.crit_celsius
            );
        }
    }
    if !env.fans.is_empty() {
        println!("Fans:");
        for f in &env.fans {
            println!(
                "  {:<28} {:>5} rpm  pwm {:>3}%  {}",
                f.name,
                f.rpm,
                f.pwm_percent,
                if f.ok { "ok" } else { "FAULT" }
            );
        }
    }
    if !env.psus.is_empty() {
        println!("PSUs:");
        for p in &env.psus {
            let status = match (p.present, p.ok) {
                (false, _) => "absent",
                (true, true) => "ok",
                (true, false) => "FAULT",
            };
            println!("  {:<28} {status}", p.name);
        }
    }
    Ok(())
}

pub async fn transceivers(endpoint: IpcEndpoint) -> Result<()> {
    let channel = endpoint.connect().await.context("connecting to pmon")?;
    let mut client = pb::pmon_client::PmonClient::new(channel);
    let xcvrs = client
        .list_transceivers(pb::ListTransceiversRequest {})
        .await?
        .into_inner()
        .transceivers;

    println!(
        "{:<12} {:<8} {:<6} {:<16} {:<16} Serial",
        "Port", "Present", "Type", "Vendor", "Part"
    );
    for x in xcvrs {
        println!(
            "{:<12} {:<8} {:<6} {:<16} {:<16} {}",
            x.port,
            if x.present { "yes" } else { "no" },
            x.form_factor,
            x.vendor,
            x.part_number,
            x.serial
        );
    }
    Ok(())
}

pub async fn config(endpoint: IpcEndpoint) -> Result<()> {
    let channel = endpoint.connect().await.context("connecting to mgmtd")?;
    let mut client = pb::mgmt_client::MgmtClient::new(channel);
    let text = client
        .get_config(pb::GetConfigRequest {
            source: pb::ConfigSource::Running as i32,
        })
        .await?
        .into_inner()
        .text;
    print!("{text}");
    Ok(())
}
