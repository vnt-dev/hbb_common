use crate::punch::{PunchContext, Puncher};
use rust_p2p_core::nat::NatType;
use rust_p2p_core::tunnel::udp::Model;
use rust_p2p_core::tunnel::SocketManager;
use std::sync::Arc;
use std::time::Duration;

pub(crate) async fn nat_test_loop(punch_context: Arc<PunchContext>, socket_manager: SocketManager) {
    loop {
        nat_test(&punch_context, &socket_manager).await;
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

async fn nat_test(punch_context: &PunchContext, socket_manager: &SocketManager) {
    match punch_context.update_nat_info().await {
        Ok(nat_info) => match nat_info.nat_type {
            NatType::Cone => {
                if let Some(socket_manager) = socket_manager.udp_socket_manager_as_ref() {
                    _ = socket_manager.switch_model(Model::Low);
                }
            }
            NatType::Symmetric => {
                if let Some(socket_manager) = socket_manager.udp_socket_manager_as_ref() {
                    _ = socket_manager.switch_model(Model::High);
                }
            }
        },
        Err(e) => {
            log::debug!("stun_test_nat {e:?} ")
        }
    }
}
