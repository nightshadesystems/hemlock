//! gRPC surface of pmon (`hemlock.v1.Pmon`).

use hemlock_common::proto::v1 as pb;
use tonic::{Request, Response, Status};

use crate::SharedEnv;

pub struct PmonService {
    env: SharedEnv,
}

impl PmonService {
    pub fn new(env: SharedEnv) -> Self {
        Self { env }
    }
}

#[tonic::async_trait]
impl pb::pmon_server::Pmon for PmonService {
    async fn get_environment(
        &self,
        _request: Request<pb::GetEnvironmentRequest>,
    ) -> Result<Response<pb::Environment>, Status> {
        let state = self
            .env
            .read()
            .map_err(|_| Status::internal("env state poisoned"))?;
        Ok(Response::new(pb::Environment {
            temperatures: state
                .temperatures
                .iter()
                .map(|t| pb::TemperatureSensor {
                    name: t.name.clone(),
                    celsius: t.celsius,
                    warn_celsius: t.warn_c,
                    crit_celsius: t.crit_c,
                })
                .collect(),
            fans: state
                .fans
                .iter()
                .map(|f| pb::Fan {
                    name: f.name.clone(),
                    rpm: f.rpm,
                    pwm_percent: f.pwm_percent,
                    ok: f.ok,
                })
                .collect(),
            psus: state
                .psus
                .iter()
                .map(|p| pb::Psu {
                    name: p.name.clone(),
                    present: p.present,
                    ok: p.ok,
                })
                .collect(),
        }))
    }

    async fn list_transceivers(
        &self,
        _request: Request<pb::ListTransceiversRequest>,
    ) -> Result<Response<pb::ListTransceiversResponse>, Status> {
        let state = self
            .env
            .read()
            .map_err(|_| Status::internal("env state poisoned"))?;
        Ok(Response::new(pb::ListTransceiversResponse {
            transceivers: state
                .transceivers
                .iter()
                .map(|x| pb::Transceiver {
                    port: x.port.clone(),
                    present: x.present,
                    form_factor: x.info.form_factor.clone(),
                    vendor: x.info.vendor.clone(),
                    part_number: x.info.part_number.clone(),
                    serial: x.info.serial.clone(),
                    date_code: x.info.date_code.clone(),
                    media_type: x.info.media_type.clone(),
                    dom: x.info.dom.map(|d| pb::DomValues {
                        temp_c: d.temp_c,
                        voltage_v: d.voltage_v,
                        bias_ma: d.bias_ma,
                        tx_dbm: d.tx_dbm,
                        rx_dbm: d.rx_dbm,
                    }),
                    thresholds: x.info.thresholds.map(|t| pb::DomThresholds {
                        temperature: Some(threshold(t.temperature)),
                        voltage: Some(threshold(t.voltage)),
                        bias: Some(threshold(t.bias)),
                        tx_power: Some(threshold(t.tx_power)),
                        rx_power: Some(threshold(t.rx_power)),
                    }),
                    eeprom_a0: x.info.eeprom_a0.clone(),
                    eeprom_a2: x.info.eeprom_a2.clone(),
                    age_secs: x.read_at.elapsed().as_secs(),
                })
                .collect(),
        }))
    }
}

fn threshold(t: crate::hw::DomThresholdInfo) -> pb::DomThreshold {
    pb::DomThreshold {
        high_alarm: t.high_alarm,
        high_warn: t.high_warn,
        low_alarm: t.low_alarm,
        low_warn: t.low_warn,
    }
}
