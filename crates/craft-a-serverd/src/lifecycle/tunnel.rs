//! Online play (the playit.gg tunnel) as seen by the control socket and the
//! panel. The tunnel is independent of any one server, so none of this takes
//! a server lock.

use std::sync::Arc;

use craft_a_server_core::tunnel::TunnelInfo;

use super::{Result, Service, ServiceError, StatusProbe};
use crate::runtime::ContainerRuntime;
use crate::tunnel::{TunnelError, TunnelManager};

impl<R: ContainerRuntime, P: StatusProbe> Service<R, P> {
    /// The tunnel manager, if online play is set up in this daemon.
    pub fn tunnel_manager(&self) -> Option<&Arc<TunnelManager>> {
        self.tunnel.as_ref()
    }

    fn tunnel_or_err(&self) -> Result<&Arc<TunnelManager>> {
        self.tunnel
            .as_ref()
            .ok_or(ServiceError::Tunnel(TunnelError::NotAvailable))
    }

    /// Current state of online play.
    pub fn tunnel_info(&self) -> Result<TunnelInfo> {
        Ok(self.tunnel_or_err()?.info()?)
    }

    /// Turns online play on. `confirm` must be true: the caller has shown
    /// what is shared with playit.gg.
    pub fn tunnel_enable(&self, confirm: bool) -> Result<TunnelInfo> {
        Ok(self.tunnel_or_err()?.enable(confirm)?)
    }

    /// Turns online play off; optionally forgets the playit.gg link.
    pub async fn tunnel_disable(&self, forget_claim: bool) -> Result<TunnelInfo> {
        Ok(self.tunnel_or_err()?.disable(forget_claim).await?)
    }
}
