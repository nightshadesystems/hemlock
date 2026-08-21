//! gRPC surface of syncd (`hemlock.v1.Syncd`).

use hemlock_common::proto::v1 as pb;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tonic::{Request, Response, Status};

use crate::actor::SaiHandle;
use crate::state::PortState;

pub struct SyncdService {
    handle: SaiHandle,
}

impl SyncdService {
    pub fn new(handle: SaiHandle) -> Self {
        Self { handle }
    }

    fn to_proto(port: &PortState) -> pb::Port {
        pb::Port {
            name: port.def.name.clone(),
            index: port.def.index,
            lanes: port.def.lanes.clone(),
            speed_mbps: port.def.speed_mbps,
            admin_state: if port.admin_up {
                pb::AdminState::Up
            } else {
                pb::AdminState::Down
            } as i32,
            oper_status: if port.oper_up {
                pb::OperStatus::Up
            } else {
                pb::OperStatus::Down
            } as i32,
            description: port.description.clone(),
            sai_oid: port.sai_id.0,
            media: port.def.media.clone().unwrap_or_default(),
        }
    }
}

#[tonic::async_trait]
impl pb::syncd_server::Syncd for SyncdService {
    async fn get_switch_info(
        &self,
        _request: Request<pb::GetSwitchInfoRequest>,
    ) -> Result<Response<pb::SwitchInfo>, Status> {
        Ok(Response::new(pb::SwitchInfo {
            platform_id: self.handle.platform_id.clone(),
            backend: self.handle.backend_name.clone(),
            switch_oid: self.handle.switch.oid,
            port_count: self.handle.initial_ports() as u32,
        }))
    }

    async fn list_ports(
        &self,
        _request: Request<pb::ListPortsRequest>,
    ) -> Result<Response<pb::ListPortsResponse>, Status> {
        let table = self
            .handle
            .ports
            .read()
            .map_err(|_| Status::internal("port table poisoned"))?;
        let mut ports: Vec<pb::Port> = table.values().map(Self::to_proto).collect();
        ports.sort_by_key(|p| p.index);
        Ok(Response::new(pb::ListPortsResponse { ports }))
    }

    async fn set_port_attrs(
        &self,
        request: Request<pb::SetPortAttrsRequest>,
    ) -> Result<Response<pb::SetPortAttrsResponse>, Status> {
        let req = request.into_inner();

        let (sai_id, admin_change) = {
            let table = self
                .handle
                .ports
                .read()
                .map_err(|_| Status::internal("port table poisoned"))?;
            let port = table
                .get(&req.name)
                .ok_or_else(|| Status::not_found(format!("no port {:?}", req.name)))?;
            let admin_change = match req.admin_state.map(pb::AdminState::try_from) {
                Some(Ok(pb::AdminState::Up)) => Some(true),
                Some(Ok(pb::AdminState::Down)) => Some(false),
                Some(_) => {
                    return Err(Status::invalid_argument("unspecified admin_state"));
                }
                None => None,
            };
            (port.sai_id, admin_change)
        };

        // Drive the ASIC outside the lock.
        if let Some(up) = admin_change {
            self.handle
                .set_admin_state(sai_id, up)
                .await
                .map_err(|e| Status::internal(format!("SAI: {e}")))?;
        }

        let mut table = self
            .handle
            .ports
            .write()
            .map_err(|_| Status::internal("port table poisoned"))?;
        let port = table
            .get_mut(&req.name)
            .ok_or_else(|| Status::not_found(format!("no port {:?}", req.name)))?;
        if let Some(up) = admin_change {
            port.admin_up = up;
        }
        if let Some(description) = req.description {
            port.description = description;
        }
        Ok(Response::new(pb::SetPortAttrsResponse {
            port: Some(Self::to_proto(port)),
        }))
    }

    type WatchPortEventsStream =
        std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<pb::PortEvent, Status>> + Send>>;

    async fn watch_port_events(
        &self,
        _request: Request<pb::WatchPortEventsRequest>,
    ) -> Result<Response<Self::WatchPortEventsStream>, Status> {
        let stream = BroadcastStream::new(self.handle.events.subscribe()).filter_map(|item| {
            match item {
                Ok(event) => Some(Ok(pb::PortEvent {
                    name: event.name,
                    oper_status: if event.oper_up {
                        pb::OperStatus::Up
                    } else {
                        pb::OperStatus::Down
                    } as i32,
                })),
                // Slow consumer skipped `n` events; keep streaming.
                Err(BroadcastStreamRecvError::Lagged(_)) => None,
            }
        });
        Ok(Response::new(Box::pin(stream)))
    }
}
