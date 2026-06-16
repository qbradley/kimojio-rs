// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::proto;

/// Shared HTTP admin and gRPC health/status response model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminStatus {
    pub ready: bool,
    pub shutting_down: bool,
    pub in_flight: u64,
    pub completed: u64,
    pub failed: u64,
    pub version: String,
}

impl AdminStatus {
    pub fn starting(version: impl Into<String>) -> Self {
        Self {
            ready: false,
            shutting_down: false,
            in_flight: 0,
            completed: 0,
            failed: 0,
            version: version.into(),
        }
    }

    pub fn ready(version: impl Into<String>) -> Self {
        Self {
            ready: true,
            shutting_down: false,
            in_flight: 0,
            completed: 0,
            failed: 0,
            version: version.into(),
        }
    }

    pub fn begin_shutdown(&mut self) {
        self.ready = false;
        self.shutting_down = true;
    }

    pub fn is_ready(&self) -> bool {
        self.ready && !self.shutting_down
    }

    pub fn shutdown_complete(&self) -> bool {
        self.shutting_down && self.in_flight == 0
    }
}

impl From<AdminStatus> for proto::HealthResponse {
    fn from(value: AdminStatus) -> Self {
        Self {
            ready: value.ready,
            shutting_down: value.shutting_down,
            in_flight: value.in_flight,
            completed: value.completed,
            failed: value.failed,
            version: value.version,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_gateway_admin_status_tracks_readiness_and_shutdown() {
        let mut status = AdminStatus::ready("phase2");
        status.in_flight = 1;
        assert!(status.is_ready());
        assert!(!status.shutdown_complete());

        status.begin_shutdown();
        assert!(!status.is_ready());
        assert!(!status.shutdown_complete());

        status.in_flight = 0;
        assert!(status.shutdown_complete());
    }

    #[test]
    fn object_gateway_admin_status_maps_to_grpc_health_response() {
        let status = AdminStatus {
            ready: true,
            shutting_down: false,
            in_flight: 2,
            completed: 3,
            failed: 1,
            version: "v".to_owned(),
        };
        let response = proto::HealthResponse::from(status);

        assert!(response.ready);
        assert_eq!(response.in_flight, 2);
        assert_eq!(response.completed, 3);
        assert_eq!(response.failed, 1);
        assert_eq!(response.version, "v");
    }
}
